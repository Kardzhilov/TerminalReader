//! Background sync controller: debounced pushes, pulls, login, and the
//! persistent offline queue, plus `KOReader` xpointer ↔ position mapping.

use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, Sender, channel},
    time::{Duration, Instant},
};

use tr_core::{MatchingMethod, SyncConfig, logging};
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
    },
    Push {
        update: ProgressUpdate,
        result: Result<(), String>,
        manual: bool,
    },
    Pull {
        document: String,
        result: Result<Option<ProgressRecord>, String>,
        manual: bool,
    },
}

#[derive(Debug)]
pub struct SyncController {
    tx: Sender<SyncEvent>,
    rx: Receiver<SyncEvent>,
    credentials: Option<Credentials>,
    queue: ProgressQueue,
    queue_path: Option<PathBuf>,
    last_call: Option<Instant>,
    /// Debounced pushes awaiting the window, at most one per document.
    deferred: Vec<ProgressUpdate>,
    in_flight: usize,
    pub status: Option<String>,
}

impl SyncController {
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = channel();
        let queue_path = tr_core::state_file(QUEUE_FILE).ok();
        let queue = queue_path
            .as_deref()
            .map(ProgressQueue::load)
            .unwrap_or_default();
        Self {
            tx,
            rx,
            credentials: None,
            queue,
            queue_path,
            last_call: None,
            deferred: Vec::new(),
            in_flight: 0,
            status: None,
        }
    }

    pub fn set_credentials(&mut self, credentials: Option<Credentials>) {
        if let Some(credentials) = &credentials {
            logging::register_secret(&credentials.userkey);
        }
        self.credentials = credentials;
    }

    #[must_use]
    pub fn logged_in(&self) -> bool {
        self.credentials.is_some()
    }

    #[must_use]
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Register (optionally) and authorize in the background.
    pub fn login(&mut self, config: &SyncConfig, username: String, password: &str, register: bool) {
        let userkey = password_hash(password);
        logging::register_secret(&userkey);
        let server = config.server_url.clone();
        let password = password.to_owned();
        let tx = self.tx.clone();
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
                let client = KOSyncClient::new(
                    &server,
                    Credentials {
                        username: username.clone(),
                        userkey: userkey.clone(),
                    },
                )?;
                client.authorize()
            })()
            .map_err(|error| error.to_string());
            let _ = tx.send(SyncEvent::Auth {
                username,
                userkey,
                result,
                registered: register,
            });
        });
    }

    /// Push progress; automatic pushes within the debounce window are
    /// deferred and coalesced per document, manual pushes go out immediately.
    pub fn push(&mut self, config: &SyncConfig, update: ProgressUpdate, manual: bool) {
        let Some(credentials) = self.credentials.clone() else {
            if manual {
                self.status = Some("Not signed in.".to_owned());
            }
            return;
        };
        if !manual && should_defer(self.last_call, Instant::now()) {
            self.defer(update);
            return;
        }
        // This update supersedes anything deferred for the same document.
        self.deferred
            .retain(|existing| existing.document != update.document);
        self.spawn_push(&config.server_url, credentials, update, manual);
    }

    /// Coalesce a debounced push, keeping the newest update per document.
    fn defer(&mut self, update: ProgressUpdate) {
        self.deferred
            .retain(|existing| existing.document != update.document);
        self.deferred.push(update);
    }

    /// Pull the server's progress record for `document` in the background.
    pub fn pull(&mut self, config: &SyncConfig, document: String, manual: bool) {
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
        self.in_flight += 1;
        self.last_call = Some(Instant::now());
        std::thread::spawn(move || {
            let result = KOSyncClient::new(&server, credentials)
                .and_then(|client| client.pull(&document))
                .map_err(|error| error.to_string());
            let _ = tx.send(SyncEvent::Pull {
                document,
                result,
                manual,
            });
        });
    }

    fn spawn_push(
        &mut self,
        server: &str,
        credentials: Credentials,
        update: ProgressUpdate,
        manual: bool,
    ) {
        let server = server.to_owned();
        let tx = self.tx.clone();
        self.in_flight += 1;
        self.last_call = Some(Instant::now());
        self.status = Some("Syncing…".to_owned());
        std::thread::spawn(move || {
            let result = KOSyncClient::new(&server, credentials.clone())
                .and_then(|client| client.push(&update).map(|_| ()))
                .map_err(|error| error.to_string());
            let _ = tx.send(SyncEvent::Push {
                update,
                result,
                manual,
            });
        });
    }

    /// Flush deferred pushes and collect finished background work.
    ///
    /// Returns events the app must act on (auth outcomes and pull results);
    /// push bookkeeping — status, offline queue, drain — is handled here.
    pub fn poll(&mut self, config: &SyncConfig) -> Vec<SyncEvent> {
        let expired = !should_defer(self.last_call, Instant::now());
        if expired && !self.deferred.is_empty() {
            // One per window; the rest go out on later polls.
            let update = self.deferred.remove(0);
            self.push(config, update, false);
        }
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            match event {
                SyncEvent::Push {
                    update,
                    result,
                    manual,
                } => self.finish_push(config, update, &result, manual),
                event => events.push(event),
            }
        }
        events
    }

    fn finish_push(
        &mut self,
        config: &SyncConfig,
        update: ProgressUpdate,
        result: &Result<(), String>,
        _manual: bool,
    ) {
        match result {
            Ok(()) => {
                logging::info(&format!("sync push ok: {}", update.document));
                self.queue_remove(&update.document);
                self.status = Some(if self.queue.is_empty() {
                    "Synced.".to_owned()
                } else {
                    format!("Synced; {} queued.", self.queue.len())
                });
                self.drain_next(config);
            }
            Err(error) => {
                logging::warn(&format!("sync push failed: {error}"));
                self.queue.push(update);
                self.save_queue();
                self.status = Some(format!(
                    "Sync failed ({} queued): {error}",
                    self.queue.len()
                ));
            }
        }
    }

    /// After a successful call, retry the oldest queued update, if any.
    pub fn drain_next(&mut self, config: &SyncConfig) {
        if self.in_flight > 0 {
            return;
        }
        let Some(credentials) = self.credentials.clone() else {
            return;
        };
        self.queue.expire();
        let Some(item) = self.queue.items().front().cloned() else {
            return;
        };
        self.spawn_push(&config.server_url, credentials, item.update, false);
    }

    fn queue_remove(&mut self, document: &str) {
        if self.queue.remove_document(document) {
            self.save_queue();
        }
    }

    fn save_queue(&self) {
        if let Some(path) = &self.queue_path {
            if let Err(error) = self.queue.save(path) {
                logging::warn(&format!("could not persist sync queue: {error}"));
            }
        }
    }

    /// Send any deferred pushes immediately and wait briefly for in-flight
    /// calls, so push-on-quit completes before the process exits.
    pub fn flush(&mut self, config: &SyncConfig, timeout: Duration) {
        if let Some(credentials) = self.credentials.clone() {
            for update in std::mem::take(&mut self.deferred) {
                self.spawn_push(&config.server_url, credentials.clone(), update, false);
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
                result: Err(_),
                ..
            } = event
            {
                // Persist failed pushes so the queue drains next session.
                self.queue.push(update);
                self.save_queue();
            }
        }
    }
}

impl SyncController {
    /// A controller with an empty queue and no persistence, for unit tests.
    #[cfg(test)]
    fn for_tests() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            credentials: None,
            queue: ProgressQueue::default(),
            queue_path: None,
            last_call: None,
            deferred: Vec::new(),
            in_flight: 0,
            status: None,
        }
    }
}

impl Default for SyncController {
    fn default() -> Self {
        Self::new()
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

/// Locate the block a pulled xpointer refers to within its chapter.
///
/// Returns `(block_index, byte_offset)`. Falls back from an exact path match
/// to matching the final element step.
#[must_use]
pub fn block_for_pointer(blocks: &[SourcedBlock], pointer: &XPointer) -> Option<(usize, usize)> {
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
    let index = exact.or_else(|| {
        let target = wanted.last()?;
        blocks.iter().position(|block| {
            block
                .source_path
                .last()
                .is_some_and(|step| step.name == target.name && step.ordinal == target.ordinal)
        })
    })?;
    let offset = blocks
        .get(index)
        .and_then(|block| block_text(&block.block))
        .map_or(0, |text| byte_offset(text, pointer.offset));
    Some((index, offset))
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
#[allow(clippy::unwrap_used)]
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
        controller.defer(update("doc-a", 0.1));
        controller.defer(update("doc-b", 0.2));
        controller.defer(update("doc-a", 0.5));
        assert_eq!(controller.deferred.len(), 2);
        let doc_a = controller
            .deferred
            .iter()
            .find(|deferred| deferred.document == "doc-a")
            .unwrap();
        assert!((doc_a.percentage - 0.5).abs() < 1e-12, "newest update wins");
    }

    #[test]
    fn queue_remove_keeps_other_documents_in_order() {
        let mut controller = SyncController::for_tests();
        controller.queue.push(update("doc-a", 0.1));
        controller.queue.push(update("doc-b", 0.2));
        controller.queue.push(update("doc-c", 0.3));
        controller.queue_remove("doc-b");
        let remaining: Vec<&str> = controller
            .queue
            .items()
            .iter()
            .map(|item| item.update.document.as_str())
            .collect();
        assert_eq!(remaining, vec!["doc-a", "doc-c"]);
    }
}
