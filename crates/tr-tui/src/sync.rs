//! Background sync controller: debounced pushes, pulls, login, and the
//! persistent offline queue, plus `KOReader` xpointer ↔ position mapping.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::mpsc::{Receiver, Sender, channel},
    time::{Duration, Instant},
};

use tr_core::{MatchingMethod, SyncConfig, logging, normalize_book_path};
use tr_epub::{Block, SourcePathStep, SourcedBlock};
use tr_kosync::{
    ChecksumMethod, Credentials, KOSyncClient, ProgressQueue, ProgressRecord, ProgressUpdate,
    SyncError, document_digest, password_hash,
    xpointer::{XPointer, XPointerStep},
};

/// `KOReader` debounces sync API calls by 25 seconds.
const DEBOUNCE: Duration = Duration::from_secs(25);
const QUEUE_FILE: &str = "sync_queue.json";

#[derive(Debug)]
pub enum SyncEvent {
    Auth {
        username: String,
        userkey: String,
        result: Result<(), String>,
        registered: bool,
        generation: u64,
    },
    Push {
        update: ProgressUpdate,
        result: Result<(), String>,
        manual: bool,
        book_path: Option<PathBuf>,
        generation: u64,
        auth_generation: u64,
    },
    Pull {
        document: String,
        result: Result<Option<ProgressRecord>, String>,
        manual: bool,
        generation: u64,
    },
}

#[derive(Debug, Clone)]
struct DeferredUpdate {
    update: ProgressUpdate,
    book_path: Option<PathBuf>,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushNotice {
    pub message: String,
    pub success: bool,
}

#[derive(Debug)]
pub struct SyncController {
    tx: Sender<SyncEvent>,
    rx: Receiver<SyncEvent>,
    credentials: Option<Credentials>,
    client: Option<KOSyncClient>,
    queue: ProgressQueue,
    queue_path: Option<PathBuf>,
    last_call: Option<Instant>,
    /// Debounced pushes awaiting the window, at most one per document.
    deferred: Vec<DeferredUpdate>,
    in_flight: usize,
    pub status: Option<String>,
    push_notice: Option<PushNotice>,
    offline: bool,
    auth_generation: u64,
    push_in_flight: HashSet<String>,
    next_generation: u64,
}

impl SyncController {
    #[must_use]
    pub fn new(offline: bool) -> Self {
        let (tx, rx) = channel();
        let queue_path = tr_core::state_file(QUEUE_FILE).ok();
        let queue = queue_path
            .as_deref()
            .map(|path| match ProgressQueue::load_or_backup(path) {
                Ok((queue, Some(backup))) => {
                    logging::warn(&format!(
                        "sync queue was backed up after corruption: {}",
                        backup.display()
                    ));
                    queue
                }
                Ok((queue, None)) => queue,
                Err(error) => {
                    logging::warn(&format!("could not load sync queue: {error}"));
                    ProgressQueue::default()
                }
            })
            .unwrap_or_default();
        let next_generation = queue
            .items()
            .iter()
            .map(|item| item.generation)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Self {
            tx,
            rx,
            credentials: None,
            client: None,
            queue,
            queue_path,
            last_call: None,
            deferred: Vec::new(),
            in_flight: 0,
            status: None,
            push_notice: None,
            offline,
            auth_generation: 0,
            push_in_flight: HashSet::new(),
            next_generation,
        }
    }

    pub fn set_credentials(&mut self, credentials: Option<Credentials>) {
        if let Some(credentials) = &credentials {
            logging::register_secret(&credentials.userkey);
        }
        self.credentials = credentials;
        self.client = None;
    }

    pub fn set_credentials_for_server(
        &mut self,
        server: &str,
        credentials: Option<Credentials>,
    ) -> Result<(), SyncError> {
        let client = credentials
            .as_ref()
            .map(|credentials| KOSyncClient::new(server, credentials))
            .transpose()?;
        self.set_credentials(credentials);
        self.client = client;
        Ok(())
    }

    /// Invalidate background authentication started before logout or a server change.
    pub fn invalidate_auth(&mut self) {
        self.auth_generation = self.auth_generation.wrapping_add(1);
        self.credentials = None;
    }

    #[must_use]
    pub fn auth_generation_current(&self, generation: u64) -> bool {
        self.auth_generation == generation
    }

    #[must_use]
    pub fn logged_in(&self) -> bool {
        self.credentials.is_some()
    }

    #[must_use]
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    #[must_use]
    pub fn busy(&self) -> bool {
        self.in_flight > 0 || !self.deferred.is_empty()
    }

    pub fn take_push_notice(&mut self) -> Option<PushNotice> {
        self.push_notice.take()
    }

    /// Register (optionally) and authorize in the background.
    pub fn login(&mut self, config: &SyncConfig, username: String, password: &str, register: bool) {
        if self.offline {
            self.status = Some("Offline mode — sync is disabled.".to_owned());
            return;
        }
        let userkey = password_hash(password);
        logging::register_secret(&userkey);
        let server = config.server_url.clone();
        let password = password.to_owned();
        let tx = self.tx.clone();
        self.auth_generation = self.auth_generation.wrapping_add(1);
        let generation = self.auth_generation;
        self.in_flight += 1;
        self.status = Some(if register {
            "Registering…".to_owned()
        } else {
            "Signing in…".to_owned()
        });
        std::thread::spawn(move || {
            let result = (|| -> Result<(), SyncError> {
                if register {
                    KOSyncClient::register(&server, &username, &password)?;
                }
                let credentials = Credentials {
                    username: username.clone(),
                    userkey: userkey.clone(),
                };
                let client = KOSyncClient::new(&server, &credentials)?;
                client.authorize()
            })()
            .map_err(|error| error.to_string());
            let _ = tx.send(SyncEvent::Auth {
                username,
                userkey,
                result,
                registered: register,
                generation,
            });
        });
    }

    /// Push progress; automatic pushes within the debounce window are
    /// deferred and coalesced per document, manual pushes go out immediately.
    #[cfg(test)]
    pub fn push(&mut self, config: &SyncConfig, update: ProgressUpdate, manual: bool) {
        self.push_for_book(config, update, manual, None);
    }

    /// Push progress while retaining the local identity needed for exclusions.
    pub fn push_for_book(
        &mut self,
        config: &SyncConfig,
        update: ProgressUpdate,
        manual: bool,
        book_path: Option<PathBuf>,
    ) {
        if !self.allowed(config, manual) {
            return;
        }
        if Self::is_excluded(config, book_path.as_deref()) {
            return;
        }
        let book_path = book_path.map(|path| normalize_book_path(&path));
        let Some(credentials) = self.credentials.clone() else {
            if manual {
                self.status = Some("Not signed in.".to_owned());
            }
            return;
        };
        if !manual && should_defer(self.last_call, Instant::now()) {
            self.defer(update, book_path);
            self.status = Some("Sync queued…".to_owned());
            return;
        }
        // This update supersedes anything deferred for the same document.
        self.deferred
            .retain(|existing| existing.update.document != update.document);
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let generation =
            self.queue
                .push_with_generation(update.clone(), book_path.clone(), generation);
        if !self.save_queue() {
            return;
        }
        if self.push_in_flight.contains(&update.document) {
            self.status = Some("Sync queued…".to_owned());
            return;
        }
        self.spawn_push(
            &config.server_url,
            &credentials,
            update,
            manual,
            book_path,
            generation,
        );
    }

    /// Coalesce a debounced push, keeping the newest update per document.
    /// The update is mirrored into the persistent queue so it survives
    /// unclean exits; a successful push removes it again.
    fn defer(&mut self, update: ProgressUpdate, book_path: Option<PathBuf>) {
        self.deferred
            .retain(|existing| existing.update.document != update.document);
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.queue
            .push_with_generation(update.clone(), book_path.clone(), generation);
        let persisted = self.save_queue();
        self.deferred.push(DeferredUpdate {
            update,
            book_path,
            generation,
        });
        if !persisted {
            self.status = Some("Sync queued in memory; persistence failed.".to_owned());
        }
    }

    /// Pull the server's progress record for `document` in the background.
    pub fn pull(&mut self, config: &SyncConfig, document: String, manual: bool) {
        if !self.allowed(config, manual) {
            return;
        }
        let Some(credentials) = self.credentials.clone() else {
            if manual {
                self.status = Some("Not signed in.".to_owned());
            }
            return;
        };
        if manual {
            self.status = Some("Pulling progress…".to_owned());
        }
        let server = config.server_url.clone();
        let tx = self.tx.clone();
        let generation = self.auth_generation;
        let client = self
            .client
            .clone()
            .or_else(|| KOSyncClient::new(&server, &credentials).ok());
        self.in_flight += 1;
        self.last_call = Some(Instant::now());
        std::thread::spawn(move || {
            let result = client
                .ok_or_else(|| SyncError::Protocol("could not create sync client".to_owned()))
                .and_then(|client| client.pull(&document))
                .map_err(|error| error.to_string());
            let _ = tx.send(SyncEvent::Pull {
                document,
                result,
                manual,
                generation,
            });
        });
    }

    fn spawn_push(
        &mut self,
        server: &str,
        credentials: &Credentials,
        update: ProgressUpdate,
        manual: bool,
        book_path: Option<PathBuf>,
        generation: u64,
    ) {
        let server = server.to_owned();
        let tx = self.tx.clone();
        let auth_generation = self.auth_generation;
        let client = self
            .client
            .clone()
            .or_else(|| KOSyncClient::new(&server, credentials).ok());
        self.in_flight += 1;
        self.push_in_flight.insert(update.document.clone());
        self.last_call = Some(Instant::now());
        self.status = Some("Syncing…".to_owned());
        std::thread::spawn(move || {
            let result = client
                .ok_or_else(|| SyncError::Protocol("could not create sync client".to_owned()))
                .and_then(|client| client.push(&update).map(|_| ()))
                .map_err(|error| error.to_string());
            let _ = tx.send(SyncEvent::Push {
                update,
                result,
                manual,
                book_path,
                generation,
                auth_generation,
            });
        });
    }

    /// Flush deferred pushes and collect finished background work.
    ///
    /// Returns events the app must act on (auth outcomes and pull results);
    /// push bookkeeping — status, offline queue, drain — is handled here.
    pub fn poll(&mut self, config: &SyncConfig) -> Vec<SyncEvent> {
        let expired = !should_defer(self.last_call, Instant::now());
        if expired && config.auto_sync && !self.offline && !self.deferred.is_empty() {
            // One per window; the rest go out on later polls.
            let deferred = self.deferred.remove(0);
            let eligible = Self::eligible_book(config, deferred.book_path.as_deref());
            if eligible && !self.push_in_flight.contains(&deferred.update.document) {
                if let Some(credentials) = self.credentials.clone() {
                    self.spawn_push(
                        &config.server_url,
                        &credentials,
                        deferred.update,
                        false,
                        deferred.book_path,
                        deferred.generation,
                    );
                }
            } else {
                self.deferred.push(deferred);
            }
        }
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            match event {
                SyncEvent::Push {
                    update,
                    result,
                    manual,
                    book_path,
                    generation,
                    auth_generation,
                } => self.finish_push(
                    config,
                    update,
                    &result,
                    manual,
                    book_path,
                    generation,
                    auth_generation,
                ),
                event => events.push(event),
            }
        }
        self.drain_next(config);
        events
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_push(
        &mut self,
        config: &SyncConfig,
        update: ProgressUpdate,
        result: &Result<(), String>,
        _manual: bool,
        book_path: Option<PathBuf>,
        generation: u64,
        auth_generation: u64,
    ) {
        self.push_in_flight.remove(&update.document);
        if auth_generation != self.auth_generation {
            self.status = Some("Ignored stale sync result after account change.".to_owned());
            return;
        }
        match result {
            Ok(()) => {
                logging::info(&format!("sync push ok: {}", update.document));
                let queue_persisted = self.queue_remove_generation(&update.document, generation);
                if !queue_persisted {
                    let message =
                        "Synced remotely, but local queue cleanup failed; retry may repeat it."
                            .to_owned();
                    self.status = Some(message.clone());
                    self.push_notice = Some(PushNotice {
                        message,
                        success: false,
                    });
                    return;
                }
                let message = if self.queue.is_empty() {
                    "Synced.".to_owned()
                } else {
                    format!("Synced; {} queued.", self.queue.len())
                };
                self.status = Some(message.clone());
                self.push_notice = Some(PushNotice {
                    message,
                    success: true,
                });
                self.drain_next(config);
            }
            Err(error) => {
                logging::warn(&format!("sync push failed: {error}"));
                if self.queue.mark_failed(&update.document, generation) {
                    self.save_queue();
                } else if !self.queue.has_document(&update.document) {
                    self.queue
                        .push_with_generation(update, book_path, generation);
                    self.save_queue();
                }
                let message = format!("Sync failed ({} queued): {error}", self.queue.len());
                self.status = Some(message.clone());
                self.push_notice = Some(PushNotice {
                    message,
                    success: false,
                });
            }
        }
    }

    /// After a successful call, retry the oldest queued update, if any.
    ///
    /// Queue entries mirroring a still-deferred update are skipped so the
    /// debounce window keeps governing when those go out.
    pub fn drain_next(&mut self, config: &SyncConfig) {
        if !self.allowed(config, false) {
            return;
        }
        if should_defer(self.last_call, Instant::now()) {
            return;
        }
        let Some(credentials) = self.credentials.clone() else {
            return;
        };
        self.queue.expire();
        let Some(item) = self
            .queue
            .items()
            .iter()
            .find(|item| {
                item.book_path
                    .as_deref()
                    .is_some_and(|path| !Self::is_excluded(config, Some(path)))
                    && ProgressQueue::retry_due(item)
                    && !self.push_in_flight.contains(&item.update.document)
                    && !self
                        .deferred
                        .iter()
                        .any(|pending| pending.update.document == item.update.document)
            })
            .cloned()
        else {
            return;
        };
        self.spawn_push(
            &config.server_url,
            &credentials,
            item.update,
            false,
            item.book_path,
            item.generation,
        );
    }

    fn queue_remove_generation(&mut self, document: &str, generation: u64) -> bool {
        if self.queue.remove_generation(document, generation) {
            return self.save_queue();
        }
        true
    }

    fn save_queue(&mut self) -> bool {
        if let Some(path) = &self.queue_path {
            if let Err(error) = self.queue.save(path) {
                logging::warn(&format!("could not persist sync queue: {error}"));
                self.status = Some(format!("Could not persist sync queue: {error}"));
                return false;
            }
        }
        true
    }

    /// Send any deferred pushes immediately and wait briefly for in-flight
    /// calls, so push-on-quit completes before the process exits.
    pub fn flush(&mut self, config: &SyncConfig, timeout: Duration) {
        if self.allowed(config, false) {
            if let Some(credentials) = self.credentials.clone() {
                for deferred in std::mem::take(&mut self.deferred) {
                    if !Self::eligible_book(config, deferred.book_path.as_deref())
                        || self.push_in_flight.contains(&deferred.update.document)
                    {
                        self.deferred.push(deferred);
                        continue;
                    }
                    self.spawn_push(
                        &config.server_url,
                        &credentials,
                        deferred.update,
                        false,
                        deferred.book_path,
                        deferred.generation,
                    );
                }
            }
        }
        let deadline = Instant::now() + timeout;
        while self.in_flight > 0 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let Ok(event) = self.rx.recv_timeout(remaining) else {
                break;
            };
            self.in_flight = self.in_flight.saturating_sub(1);
            if let SyncEvent::Push {
                update,
                result,
                manual,
                book_path,
                generation,
                auth_generation,
            } = event
            {
                // Full bookkeeping: successes clear their persisted mirror
                // (and drain the backlog), failures stay queued.
                self.finish_push(
                    config,
                    update,
                    &result,
                    manual,
                    book_path,
                    generation,
                    auth_generation,
                );
            }
        }
    }

    fn allowed(&mut self, config: &SyncConfig, manual: bool) -> bool {
        if self.offline || (!manual && !config.auto_sync) {
            if manual && self.offline {
                self.status = Some("Offline mode — sync is disabled.".to_owned());
            }
            return false;
        }
        true
    }

    fn is_excluded(config: &SyncConfig, book_path: Option<&std::path::Path>) -> bool {
        book_path.is_some_and(|path| {
            let path = normalize_book_path(path);
            config
                .excluded_books
                .iter()
                .map(|excluded| normalize_book_path(excluded))
                .any(|excluded| excluded == path)
        })
    }

    fn eligible_book(config: &SyncConfig, book_path: Option<&std::path::Path>) -> bool {
        book_path.is_some_and(|path| !Self::is_excluded(config, Some(path)))
    }
}

impl SyncController {
    /// A controller with an empty queue and no persistence, for unit tests.
    #[cfg(test)]
    fn for_tests() -> Self {
        Self::for_tests_with(false)
    }

    #[cfg(test)]
    fn for_tests_with(offline: bool) -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            credentials: None,
            client: None,
            queue: ProgressQueue::default(),
            queue_path: None,
            last_call: None,
            deferred: Vec::new(),
            in_flight: 0,
            status: None,
            push_notice: None,
            offline,
            auth_generation: 0,
            push_in_flight: HashSet::new(),
            next_generation: 1,
        }
    }
}

impl Default for SyncController {
    fn default() -> Self {
        Self::new(false)
    }
}

/// True when an automatic push at `now` falls inside the debounce window
/// after the last server call.
fn should_defer(last_call: Option<Instant>, now: Instant) -> bool {
    last_call.is_some_and(|last| now.saturating_duration_since(last) < DEBOUNCE)
}

/// `KOReader` rounds percentages to four decimal places.
#[must_use]
pub fn round_percent(value: f64) -> f64 {
    (value * 10_000.0).floor() / 10_000.0
}

/// Stable 32-hex device identifier, generated once at first login.
#[must_use]
pub fn generate_device_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    password_hash(&format!("terminalreader-{}-{nanos}", std::process::id()))
}

#[must_use]
pub fn checksum_method(matching: MatchingMethod) -> ChecksumMethod {
    match matching {
        MatchingMethod::Binary => ChecksumMethod::Binary,
        MatchingMethod::Filename => ChecksumMethod::Filename,
    }
}

/// Compute the sync document id for a book per the configured matching method.
pub fn digest_for(path: &std::path::Path, matching: MatchingMethod) -> Result<String, SyncError> {
    document_digest(path, checksum_method(matching))
}

/// Convert a tr-epub source path (rooted at `html`) to xpointer steps rooted
/// at the chapter `body`, matching `KOReader`'s `DocFragment` layout.
#[must_use]
pub fn xpointer_steps(source_path: &[SourcePathStep]) -> Vec<XPointerStep> {
    let start = source_path
        .iter()
        .position(|step| step.name == "body")
        .unwrap_or(0);
    source_path
        .get(start..)
        .unwrap_or_default()
        .iter()
        .map(|step| XPointerStep {
            name: step.name.clone(),
            ordinal: step.ordinal,
        })
        .collect()
}

/// Progress string for the anchor block of `chapter_index` (0-based).
#[must_use]
pub fn progress_string(
    chapter_index: usize,
    source_path: &[SourcePathStep],
    block_text: Option<&str>,
    byte_offset: usize,
) -> String {
    let offset = block_text.map_or(0, |text| char_offset(text, byte_offset));
    XPointer {
        fragment: chapter_index + 1,
        steps: xpointer_steps(source_path),
        offset,
    }
    .format()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerConfidence {
    Exact,
    UniqueFallback,
}

/// Locate the block a pulled xpointer refers to within its chapter.
///
/// Returns `(block_index, byte_offset)`. Falls back from an exact path match
/// to matching the final element step.
#[must_use]
#[cfg(test)]
pub fn block_for_pointer(blocks: &[SourcedBlock], pointer: &XPointer) -> Option<(usize, usize)> {
    block_for_pointer_confident(blocks, pointer).map(|(index, offset, _)| (index, offset))
}

/// Locate a pointer and report whether its source path was exact or uniquely
/// recovered from the final element step.
#[must_use]
pub fn block_for_pointer_confident(
    blocks: &[SourcedBlock],
    pointer: &XPointer,
) -> Option<(usize, usize, PointerConfidence)> {
    let mut wanted: &[XPointerStep] = &pointer.steps;
    while let Some(last) = wanted.last() {
        if last.name == "text()" {
            wanted = wanted.get(..wanted.len() - 1).unwrap_or_default();
        } else {
            break;
        }
    }
    if wanted.is_empty() {
        return None;
    }
    let exact = blocks
        .iter()
        .position(|block| xpointer_steps(&block.source_path) == wanted);
    let (index, confidence) = if let Some(index) = exact {
        (index, PointerConfidence::Exact)
    } else {
        let target = wanted.last()?;
        let matches: Vec<usize> = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| {
                block
                    .source_path
                    .last()
                    .is_some_and(|step| step.name == target.name && step.ordinal == target.ordinal)
            })
            .map(|(index, _)| index)
            .collect();
        if matches.len() != 1 {
            return None;
        }
        let index = matches.first().copied()?;
        (index, PointerConfidence::UniqueFallback)
    };
    let offset = blocks
        .get(index)
        .and_then(|block| block_text(&block.block))
        .map_or(0, |text| byte_offset(text, pointer.offset));
    Some((index, offset, confidence))
}

/// Plain text of a block, when it has any.
#[must_use]
pub fn block_text(block: &Block) -> Option<&str> {
    match block {
        Block::Paragraph(text)
        | Block::Quote(text)
        | Block::Code(text)
        | Block::Heading { text, .. } => Some(text),
        Block::Rule | Block::Image { .. } => None,
    }
}

fn char_offset(text: &str, byte_offset: usize) -> usize {
    text.char_indices()
        .take_while(|(index, _)| *index < byte_offset)
        .count()
}

fn byte_offset(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn sourced(names: &[(&str, usize)], text: &str) -> SourcedBlock {
        SourcedBlock {
            block: Block::Paragraph(text.to_owned()),
            source_path: names
                .iter()
                .map(|(name, ordinal)| SourcePathStep {
                    name: (*name).to_owned(),
                    ordinal: *ordinal,
                })
                .collect(),
            inline: Vec::new(),
            ids: Vec::new(),
        }
    }

    #[test]
    fn progress_string_uses_docfragment_and_body_rooted_steps() {
        let block = sourced(
            &[("html", 1), ("body", 1), ("div", 1), ("p", 3)],
            "héllo world",
        );
        let progress = progress_string(6, &block.source_path, Some("héllo world"), 7);
        assert_eq!(progress, "/body/DocFragment[7]/body/div/p[3].6");
    }

    #[test]
    fn pointer_maps_back_to_block_and_byte_offset() {
        let blocks = vec![
            sourced(&[("html", 1), ("body", 1), ("p", 1)], "first"),
            sourced(&[("html", 1), ("body", 1), ("p", 2)], "héllo world"),
        ];
        let pointer = XPointer::parse("/body/DocFragment[3]/body/p[2].6").unwrap();
        let (index, offset) = block_for_pointer(&blocks, &pointer).unwrap();
        assert_eq!(index, 1);
        assert_eq!(offset, 7, "6 chars into héllo world is byte 7");
    }

    #[test]
    fn pointer_with_text_node_and_unknown_parent_matches_last_step() {
        let blocks = vec![sourced(&[("html", 1), ("body", 1), ("p", 5)], "text")];
        let pointer = XPointer::parse("/body/DocFragment[1]/body/section/p[5]/text().2").unwrap();
        assert_eq!(block_for_pointer(&blocks, &pointer), Some((0, 2)));
    }

    #[test]
    fn ambiguous_pointer_fallback_is_rejected() {
        let blocks = vec![
            sourced(&[("html", 1), ("body", 1), ("p", 5)], "first"),
            sourced(&[("html", 1), ("body", 1), ("p", 5)], "second"),
        ];
        let pointer = XPointer::parse("/body/DocFragment[1]/body/section/p[5].0").unwrap();
        assert_eq!(block_for_pointer_confident(&blocks, &pointer), None);
    }

    #[test]
    fn percent_rounding_matches_koreader() {
        assert!((round_percent(0.123_456) - 0.1234).abs() < 1e-12);
        assert!((round_percent(1.0) - 1.0).abs() < 1e-12);
    }

    fn update(document: &str, percentage: f64) -> ProgressUpdate {
        ProgressUpdate {
            document: document.to_owned(),
            metadata: None,
            progress: "/body/DocFragment[1]/body/p[1].0".to_owned(),
            percentage,
            device: "test".to_owned(),
            device_id: "test-id".to_owned(),
        }
    }

    #[test]
    fn debounce_defers_only_inside_window() {
        let now = Instant::now();
        assert!(!should_defer(None, now), "first call is never deferred");
        assert!(should_defer(Some(now), now));
        assert!(should_defer(
            Some(now),
            now + DEBOUNCE.saturating_sub(Duration::from_secs(1))
        ));
        assert!(!should_defer(Some(now), now + DEBOUNCE));
    }

    #[test]
    fn defer_coalesces_to_the_newest_update_per_document() {
        let mut controller = SyncController::for_tests();
        controller.defer(update("doc-a", 0.1), None);
        controller.defer(update("doc-b", 0.2), None);
        controller.defer(update("doc-a", 0.5), None);
        assert_eq!(controller.deferred.len(), 2);
        let doc_a = controller
            .deferred
            .iter()
            .find(|deferred| deferred.update.document == "doc-a")
            .unwrap();
        assert!(
            (doc_a.update.percentage - 0.5).abs() < 1e-12,
            "newest update wins"
        );
    }

    #[test]
    fn queue_remove_keeps_other_documents_in_order() {
        let mut controller = SyncController::for_tests();
        controller.queue.push(update("doc-a", 0.1));
        controller.queue.push(update("doc-b", 0.2));
        controller.queue.push(update("doc-c", 0.3));
        let generation = controller
            .queue
            .items()
            .iter()
            .find(|item| item.update.document == "doc-b")
            .map_or(0, |item| item.generation);
        controller.queue_remove_generation("doc-b", generation);
        let remaining: Vec<&str> = controller
            .queue
            .items()
            .iter()
            .map(|item| item.update.document.as_str())
            .collect();
        assert_eq!(remaining, vec!["doc-a", "doc-c"]);
    }

    #[test]
    fn finish_push_exposes_success_and_failure_notices() {
        let mut controller = SyncController::for_tests();
        let config = SyncConfig::default();
        controller.finish_push(&config, update("ok", 0.5), &Ok(()), false, None, 1, 0);
        assert_eq!(
            controller.take_push_notice(),
            Some(PushNotice {
                message: "Synced.".to_owned(),
                success: true,
            })
        );

        controller.finish_push(
            &config,
            update("failed", 0.5),
            &Err("offline".to_owned()),
            false,
            None,
            1,
            0,
        );
        assert_eq!(
            controller.take_push_notice(),
            Some(PushNotice {
                message: "Sync failed (1 queued): offline".to_owned(),
                success: false,
            })
        );
    }

    fn signed_in_controller() -> SyncController {
        let mut controller = SyncController::for_tests();
        controller.set_credentials(Some(Credentials {
            username: "user".to_owned(),
            userkey: "key".to_owned(),
        }));
        controller
    }

    /// Unroutable server so spawned pushes fail fast without network access.
    fn unreachable_config() -> SyncConfig {
        SyncConfig {
            server_url: "http://127.0.0.1:1".to_owned(),
            ..SyncConfig::default()
        }
    }

    #[test]
    fn deferred_push_is_mirrored_in_the_queue_and_reports_status() {
        let mut controller = signed_in_controller();
        controller.last_call = Some(Instant::now());
        controller.push(&unreachable_config(), update("doc-a", 0.4), false);
        assert_eq!(controller.deferred.len(), 1);
        assert_eq!(controller.queue.len(), 1, "deferred push is persisted");
        assert_eq!(controller.status.as_deref(), Some("Sync queued…"));
        assert_eq!(controller.in_flight, 0, "nothing was sent yet");
    }

    #[test]
    fn queue_persistence_failure_is_visible() {
        let mut controller = SyncController::for_tests();
        controller.queue_path = Some(
            std::env::temp_dir()
                .join(format!("terminalreader-missing-{}", std::process::id()))
                .join("sync_queue.json"),
        );
        controller.defer(update("doc-a", 0.4), None);
        assert_eq!(controller.queue.len(), 1);
        assert!(
            controller
                .status
                .as_deref()
                .is_some_and(|status| status.contains("persistence failed"))
        );
    }

    #[test]
    fn automatic_push_is_suppressed_when_auto_sync_is_disabled() {
        let mut controller = signed_in_controller();
        let config = SyncConfig {
            auto_sync: false,
            ..SyncConfig::default()
        };
        controller.push(&config, update("doc-a", 0.4), false);
        assert_eq!(controller.in_flight, 0);
        assert!(controller.deferred.is_empty());
        assert!(controller.queue.is_empty());
    }

    #[test]
    fn offline_mode_suppresses_manual_and_automatic_sync() {
        let mut controller = SyncController::for_tests_with(true);
        controller.set_credentials(Some(Credentials {
            username: "user".to_owned(),
            userkey: "key".to_owned(),
        }));
        let config = SyncConfig::default();
        controller.push(&config, update("doc-a", 0.4), true);
        controller.pull(&config, "doc-a".to_owned(), true);
        controller.login(&config, "user".to_owned(), "password", false);
        assert_eq!(controller.in_flight, 0);
        assert!(controller.deferred.is_empty());
        assert!(controller.queue.is_empty());
        assert_eq!(
            controller.status.as_deref(),
            Some("Offline mode — sync is disabled.")
        );
    }

    #[test]
    fn disabled_auto_sync_keeps_deferred_updates_paused() {
        let mut controller = signed_in_controller();
        controller.deferred.push(DeferredUpdate {
            update: update("doc-a", 0.4),
            book_path: None,
            generation: 1,
        });
        let config = SyncConfig {
            auto_sync: false,
            ..SyncConfig::default()
        };
        controller.poll(&config);
        assert_eq!(controller.deferred.len(), 1);
        assert_eq!(
            controller
                .deferred
                .first()
                .map(|deferred| deferred.update.document.as_str()),
            Some("doc-a")
        );
    }

    #[test]
    fn drain_next_skips_documents_with_deferred_updates() {
        let mut controller = signed_in_controller();
        controller
            .queue
            .push_with_book(update("doc-a", 0.1), Some(PathBuf::from("a.epub")));
        controller
            .queue
            .push_with_book(update("doc-b", 0.2), Some(PathBuf::from("b.epub")));
        controller.deferred.push(DeferredUpdate {
            update: update("doc-a", 0.3),
            book_path: Some(PathBuf::from("a.epub")),
            generation: 1,
        });
        controller.drain_next(&unreachable_config());
        assert_eq!(controller.in_flight, 1, "one push was spawned");
        let event = controller
            .rx
            .recv_timeout(Duration::from_secs(10))
            .expect("spawned push finishes");
        match event {
            SyncEvent::Push { update, .. } => {
                assert_eq!(update.document, "doc-b", "deferred doc-a is skipped");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn legacy_queue_entries_without_book_identity_stay_paused() {
        let mut controller = signed_in_controller();
        controller.queue.push(update("legacy", 0.2));
        controller.drain_next(&unreachable_config());
        assert_eq!(controller.in_flight, 0);
    }

    #[test]
    fn flush_rechecks_exclusions_before_dispatch() {
        let mut controller = signed_in_controller();
        let excluded = PathBuf::from("C:/Books/private.epub");
        controller.deferred.push(DeferredUpdate {
            update: update("excluded", 0.2),
            book_path: Some(excluded.clone()),
            generation: 1,
        });
        let config = SyncConfig {
            excluded_books: vec![excluded],
            ..unreachable_config()
        };
        controller.flush(&config, Duration::from_millis(1));
        assert_eq!(controller.in_flight, 0);
        assert_eq!(controller.deferred.len(), 1);
    }

    #[test]
    fn flush_keeps_same_document_deferred_until_in_flight_finishes() {
        let mut controller = signed_in_controller();
        controller.push(&unreachable_config(), update("doc-a", 0.4), false);
        assert_eq!(controller.in_flight, 1, "first push goes out immediately");
        controller.push(&unreachable_config(), update("doc-a", 0.5), false);
        assert_eq!(controller.deferred.len(), 1, "second push is deferred");
        controller.flush(&unreachable_config(), Duration::from_secs(10));
        assert_eq!(
            controller.deferred.len(),
            1,
            "same-document deferred push was not sent concurrently"
        );
        assert_eq!(
            controller.queue.len(),
            1,
            "failed push stays queued for the next session"
        );
        assert_eq!(controller.in_flight, 0);
    }
}
