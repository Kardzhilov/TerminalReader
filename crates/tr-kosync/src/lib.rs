//! `KOReader` kosync document matching, HTTP protocol client, and offline queue.

pub mod xpointer;

use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use md5::{Digest, Md5};
use reqwest::{
    StatusCode,
    blocking::{Client, Response},
    header::{ACCEPT, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

pub const OFFICIAL_SERVER: &str = "https://sync.koreader.rocks";
const PROGRESS_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const QUEUE_MAX_ITEMS: usize = 200;
const QUEUE_MAX_AGE_SECONDS: u64 = 28 * 24 * 60 * 60;
const RETRY_BASE_SECONDS: u64 = 30;
const RETRY_MAX_SECONDS: u64 = 30 * 60;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("document I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("sync server URL is invalid: {0}")]
    Url(#[from] url::ParseError),
    #[error("credentials contain an invalid HTTP header value")]
    Header(#[from] reqwest::header::InvalidHeaderValue),
    #[error("sync server returned HTTP {0}")]
    Http(StatusCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumMethod {
    #[default]
    Binary,
    Filename,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentMetadata {
    pub filename: String,
    pub title: String,
    pub authors: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgressUpdate {
    pub document: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<DocumentMetadata>,
    pub progress: String,
    pub percentage: f64,
    pub device: String,
    pub device_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct ProgressRecord {
    pub document: Option<String>,
    pub progress: Option<String>,
    pub percentage: Option<f64>,
    pub device: Option<String>,
    pub device_id: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub userkey: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("username", &self.username)
            .field("userkey", &"[redacted]")
            .finish()
    }
}

#[derive(Debug)]
pub struct KOSyncClient {
    base_url: Url,
    credentials: Credentials,
}

impl KOSyncClient {
    pub fn new(base_url: &str, credentials: Credentials) -> Result<Self, SyncError> {
        let mut base_url = Url::parse(base_url)?;
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        Ok(Self {
            base_url,
            credentials,
        })
    }

    pub fn register(base_url: &str, username: &str, password: &str) -> Result<(), SyncError> {
        let endpoint = endpoint(Url::parse(base_url)?, "users/create")?;
        let response = auth_client(AUTH_TIMEOUT, None)?
            .post(endpoint)
            .json(&serde_json::json!({"username": username, "password": password_hash(password)}))
            .send()?;
        match response.status() {
            StatusCode::CREATED => Ok(()),
            status => Err(SyncError::Http(status)),
        }
    }

    pub fn authorize(&self) -> Result<(), SyncError> {
        let response = self
            .authenticated_client(AUTH_TIMEOUT)?
            .get(self.endpoint("users/auth")?)
            .send()?;
        match response.status() {
            StatusCode::OK => Ok(()),
            status => Err(SyncError::Http(status)),
        }
    }

    pub fn push(&self, update: &ProgressUpdate) -> Result<ProgressRecord, SyncError> {
        let response = self
            .authenticated_client(PROGRESS_TIMEOUT)?
            .put(self.endpoint("syncs/progress")?)
            .json(update)
            .send()?;
        parse_progress_response(response, &[StatusCode::OK, StatusCode::ACCEPTED])
    }

    /// Fetch the server's progress record; `Ok(None)` when the document has
    /// never been synced (servers answer 404, or 200 with an empty record).
    pub fn pull(&self, document: &str) -> Result<Option<ProgressRecord>, SyncError> {
        let response = self
            .authenticated_client(PROGRESS_TIMEOUT)?
            .get(self.endpoint(&format!("syncs/progress/{document}"))?)
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let record = parse_progress_response(response, &[StatusCode::OK])?;
        // No position payload means the server has never seen this document.
        if record.progress.is_none() && record.percentage.is_none() {
            return Ok(None);
        }
        Ok(Some(record))
    }

    fn endpoint(&self, path: &str) -> Result<Url, SyncError> {
        endpoint(self.base_url.clone(), path)
    }

    fn authenticated_client(&self, timeout: Duration) -> Result<Client, SyncError> {
        auth_client(timeout, Some(&self.credentials))
    }
}

fn endpoint(mut base_url: Url, path: &str) -> Result<Url, SyncError> {
    if !base_url.path().ends_with('/') {
        base_url.set_path(&format!("{}/", base_url.path()));
    }
    Ok(base_url.join(path)?)
}

fn auth_client(timeout: Duration, credentials: Option<&Credentials>) -> Result<Client, SyncError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.koreader.v1+json"),
    );
    if let Some(credentials) = credentials {
        headers.insert("x-auth-user", HeaderValue::from_str(&credentials.username)?);
        headers.insert("x-auth-key", HeaderValue::from_str(&credentials.userkey)?);
    }
    Ok(Client::builder()
        .connect_timeout(timeout.min(Duration::from_secs(2)))
        .timeout(timeout)
        .default_headers(headers)
        .build()?)
}

fn parse_progress_response(
    response: Response,
    expected: &[StatusCode],
) -> Result<ProgressRecord, SyncError> {
    let status = response.status();
    if !expected.contains(&status) {
        return Err(SyncError::Http(status));
    }
    Ok(record_from_body(&response.text()?))
}

/// Decode a progress body leniently: some servers answer with an empty body,
/// `{}`, or `null` where others answer 404.
fn record_from_body(body: &str) -> ProgressRecord {
    serde_json::from_str(body).unwrap_or_default()
}

#[must_use]
pub fn password_hash(password: &str) -> String {
    hex::encode(Md5::digest(password.as_bytes()))
}

pub fn partial_md5(path: &Path) -> Result<String, SyncError> {
    let mut file = File::open(path)?;
    let mut hasher = Md5::new();
    let mut buffer = [0_u8; 1024];
    for exponent in -1_i32..=10 {
        // KOReader's `lshift(1024, -2)` wraps to 0 in LuaJIT's 32-bit shift,
        // so the first sample is the file's first kibibyte.
        let offset = if exponent < 0 {
            0
        } else {
            1024_u64 << (2 * exponent)
        };
        file.seek(SeekFrom::Start(offset))?;
        let count = read_up_to(&mut file, &mut buffer)?;
        if count == 0 {
            break;
        }
        if let Some(sample) = buffer.get(..count) {
            hasher.update(sample);
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

#[must_use]
pub fn filename_md5(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| hex::encode(Md5::digest(name.to_string_lossy().as_bytes())))
}

/// Document identifier for the configured matching method.
pub fn document_digest(path: &Path, method: ChecksumMethod) -> Result<String, SyncError> {
    match method {
        ChecksumMethod::Binary => partial_md5(path),
        ChecksumMethod::Filename => filename_md5(path).ok_or_else(|| {
            SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "document path has no file name",
            ))
        }),
    }
}

fn read_up_to(reader: &mut File, buffer: &mut [u8]) -> Result<usize, SyncError> {
    let mut count = 0;
    while count < buffer.len() {
        let Some(remaining) = buffer.get_mut(count..) else {
            break;
        };
        let read = reader.read(remaining)?;
        if read == 0 {
            break;
        }
        count += read;
    }
    Ok(count)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProgressQueue {
    #[serde(default)]
    items: VecDeque<QueuedProgress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedProgress {
    pub update: ProgressUpdate,
    pub queued_at: u64,
    /// Normalized local book identity, absent in legacy queue entries.
    #[serde(default)]
    pub book_path: Option<PathBuf>,
    /// Monotonic local generation for stale completion protection.
    #[serde(default)]
    pub generation: u64,
    /// Number of failed delivery attempts.
    #[serde(default)]
    pub attempts: u32,
    /// Earliest Unix time at which another automatic attempt is allowed.
    #[serde(default)]
    pub next_attempt_at: u64,
}

impl ProgressQueue {
    /// Load the persisted queue; missing or corrupt files yield an empty queue.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), SyncError> {
        let temporary = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| SyncError::Io(std::io::Error::other(error)))?;
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn push(&mut self, update: ProgressUpdate) {
        self.push_with_book(update, None);
    }

    /// Queue progress with the local book identity used for exclusion checks.
    pub fn push_with_book(&mut self, update: ProgressUpdate, book_path: Option<PathBuf>) {
        self.push_with_generation(update, book_path, 0);
    }

    pub fn push_with_generation(
        &mut self,
        update: ProgressUpdate,
        book_path: Option<PathBuf>,
        generation: u64,
    ) -> u64 {
        self.expire();
        self.items
            .retain(|item| item.update.document != update.document);
        let generation = if generation == 0 {
            self.items
                .iter()
                .map(|item| item.generation)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
        } else {
            generation
        };
        self.items.push_back(QueuedProgress {
            update,
            queued_at: unix_timestamp(),
            book_path,
            generation,
            attempts: 0,
            next_attempt_at: 0,
        });
        while self.items.len() > QUEUE_MAX_ITEMS {
            let _ = self.items.pop_front();
        }
        generation
    }

    #[must_use]
    pub fn items(&self) -> &VecDeque<QueuedProgress> {
        &self.items
    }

    pub fn pop_front(&mut self) -> Option<QueuedProgress> {
        self.items.pop_front()
    }

    /// Drop any queued update for `document`, leaving other items — and
    /// their original `queued_at` timestamps — untouched.
    pub fn remove_document(&mut self, document: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.update.document != document);
        self.items.len() != before
    }

    pub fn remove_generation(&mut self, document: &str, generation: u64) -> bool {
        let before = self.items.len();
        self.items
            .retain(|item| !(item.update.document == document && item.generation == generation));
        self.items.len() != before
    }

    #[must_use]
    pub fn has_document(&self, document: &str) -> bool {
        self.items
            .iter()
            .any(|item| item.update.document == document)
    }

    pub fn mark_failed(&mut self, document: &str, generation: u64) -> bool {
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.update.document == document && item.generation == generation)
        else {
            return false;
        };
        item.attempts = item.attempts.saturating_add(1);
        let exponent = item.attempts.saturating_sub(1).min(6);
        let delay = RETRY_BASE_SECONDS
            .saturating_mul(1_u64 << exponent)
            .min(RETRY_MAX_SECONDS);
        item.next_attempt_at = unix_timestamp().saturating_add(delay);
        true
    }

    #[must_use]
    pub fn retry_due(item: &QueuedProgress) -> bool {
        item.next_attempt_at <= unix_timestamp()
    }

    pub fn expire(&mut self) {
        let cutoff = unix_timestamp().saturating_sub(QUEUE_MAX_AGE_SECONDS);
        self.items.retain(|item| item.queued_at >= cutoff);
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn update(document: &str) -> ProgressUpdate {
        ProgressUpdate {
            document: document.to_owned(),
            metadata: None,
            progress: "1".to_owned(),
            percentage: 0.1,
            device: "test".to_owned(),
            device_id: "device".to_owned(),
        }
    }

    #[test]
    fn password_hash_matches_md5_vector() {
        assert_eq!(
            password_hash("koreader"),
            "90af4ab23bb923fc935ee9997e45b134"
        );
    }

    #[test]
    fn record_from_body_tolerates_empty_and_null_responses() {
        assert_eq!(record_from_body(""), ProgressRecord::default());
        assert_eq!(record_from_body("{}"), ProgressRecord::default());
        assert_eq!(record_from_body("null"), ProgressRecord::default());
        assert_eq!(
            record_from_body("<html>err</html>"),
            ProgressRecord::default()
        );
        let record = record_from_body(r#"{"progress":"/body/p[1].0","percentage":0.5}"#);
        assert_eq!(record.progress.as_deref(), Some("/body/p[1].0"));
        assert_eq!(record.percentage, Some(0.5));
    }

    #[test]
    fn queue_deduplicates_newest_progress() {
        let mut queue = ProgressQueue::default();
        queue.push(update("a"));
        queue.push(ProgressUpdate {
            progress: "2".to_owned(),
            ..update("a")
        });
        assert_eq!(queue.items().len(), 1);
        assert_eq!(queue.items().front().expect("entry").update.progress, "2");
    }

    #[test]
    fn queue_preserves_book_identity_and_reads_legacy_entries() -> Result<(), SyncError> {
        let mut queue = ProgressQueue::default();
        queue.push_with_book(update("a"), Some(PathBuf::from("C:/Books/a.epub")));
        let encoded = serde_json::to_vec(&queue)
            .map_err(|error| SyncError::Io(std::io::Error::other(error)))?;
        let decoded: ProgressQueue = serde_json::from_slice(&encoded)
            .map_err(|error| SyncError::Io(std::io::Error::other(error)))?;
        assert_eq!(
            decoded
                .items()
                .front()
                .and_then(|item| item.book_path.as_deref()),
            Some(Path::new("C:/Books/a.epub"))
        );

        let legacy = br#"{"items":[{"update":{"document":"a","progress":"1","percentage":0.1,"device":"test","device_id":"device"},"queued_at":1}]}"#;
        let decoded: ProgressQueue = serde_json::from_slice(legacy)
            .map_err(|error| SyncError::Io(std::io::Error::other(error)))?;
        assert!(
            decoded
                .items()
                .front()
                .and_then(|item| item.book_path.as_ref())
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn queue_completion_only_removes_matching_generation() {
        let mut queue = ProgressQueue::default();
        let old = queue.push_with_generation(update("a"), None, 7);
        assert!(queue.remove_generation("a", old));
        let newer = queue.push_with_generation(update("a"), None, 8);
        assert!(!queue.remove_generation("a", old));
        assert_eq!(
            queue.items().front().map(|item| item.generation),
            Some(newer)
        );
    }

    #[test]
    fn queue_failure_schedules_bounded_retry() {
        let mut queue = ProgressQueue::default();
        let generation = queue.push_with_generation(update("a"), None, 3);
        assert!(queue.mark_failed("a", generation));
        let item = queue.items().front().expect("queued item");
        assert_eq!(item.attempts, 1);
        assert!(item.next_attempt_at > unix_timestamp());
    }

    #[test]
    fn queue_remove_document_keeps_other_items_and_timestamps() {
        let mut queue = ProgressQueue::default();
        queue.push(update("a"));
        queue.push(update("b"));
        let kept_at = queue.items().back().expect("queued item").queued_at;
        assert!(queue.remove_document("a"));
        assert!(!queue.remove_document("missing"));
        assert_eq!(queue.items().len(), 1);
        let remaining = queue.items().front().expect("remaining item");
        assert_eq!(remaining.update.document, "b");
        assert_eq!(remaining.queued_at, kept_at);
    }

    fn temp_file(name: &str, contents: &[u8]) -> Result<std::path::PathBuf, SyncError> {
        let path =
            std::env::temp_dir().join(format!("terminalreader-{}-{name}", std::process::id()));
        let mut file = File::create(&path)?;
        file.write_all(contents)?;
        Ok(path)
    }

    #[test]
    fn digest_matches_koreader_partial_md5() -> Result<(), SyncError> {
        // Reference layout: MD5 over 1 KiB samples at offsets 0, 1024,
        // 4096, ..., stopping at the first empty read.
        let contents: Vec<u8> = (0..6000_u32)
            .map(|index| u8::try_from(index % 251).unwrap_or(0))
            .collect();
        let path = temp_file("digest-vector.bin", &contents)?;
        let mut hasher = Md5::new();
        for offset in [0_usize, 1024, 4096] {
            let end = (offset + 1024).min(contents.len());
            if let Some(sample) = contents.get(offset..end) {
                hasher.update(sample);
            }
        }
        let expected = hex::encode(hasher.finalize());
        assert_eq!(partial_md5(&path)?, expected);
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn digest_includes_first_kibibyte() -> Result<(), SyncError> {
        let mut contents = vec![b'x'; 5000];
        let first = temp_file("digest-first-a.bin", &contents)?;
        if let Some(byte) = contents.first_mut() {
            *byte = b'y';
        }
        let second = temp_file("digest-first-b.bin", &contents)?;
        assert_ne!(partial_md5(&first)?, partial_md5(&second)?);
        std::fs::remove_file(first)?;
        std::fs::remove_file(second)?;
        Ok(())
    }

    #[test]
    fn digest_selection_follows_checksum_method() -> Result<(), SyncError> {
        let path = temp_file("digest-method.epub", &[b'x'; 3000])?;
        assert_eq!(
            document_digest(&path, ChecksumMethod::Binary)?,
            partial_md5(&path)?
        );
        assert_eq!(
            Some(document_digest(&path, ChecksumMethod::Filename)?),
            filename_md5(&path)
        );
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn queue_persists_across_load_and_save() -> Result<(), SyncError> {
        let path = std::env::temp_dir().join(format!(
            "terminalreader-{}-queue-roundtrip.json",
            std::process::id()
        ));
        let mut queue = ProgressQueue::default();
        queue.push(update("doc-a"));
        queue.push(update("doc-b"));
        queue.save(&path)?;
        let restored = ProgressQueue::load(&path);
        assert_eq!(restored.len(), 2);
        assert_eq!(
            restored
                .items()
                .front()
                .map(|item| item.update.document.as_str()),
            Some("doc-a")
        );
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn debug_output_redacts_userkey() {
        let credentials = Credentials {
            username: "reader".to_owned(),
            userkey: "deadbeef".to_owned(),
        };
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("deadbeef"));
        assert!(debug.contains("[redacted]"));
    }
}
