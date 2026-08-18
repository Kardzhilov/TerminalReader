//! `KOReader` kosync document matching, HTTP protocol client, and offline queue.

use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProgressRecord {
    pub document: Option<String>,
    pub progress: Option<String>,
    pub percentage: Option<f64>,
    pub device: Option<String>,
    pub device_id: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub userkey: String,
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

    pub fn pull(&self, document: &str) -> Result<ProgressRecord, SyncError> {
        let response = self
            .authenticated_client(PROGRESS_TIMEOUT)?
            .get(self.endpoint(&format!("syncs/progress/{document}"))?)
            .send()?;
        parse_progress_response(response, &[StatusCode::OK])
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
    Ok(response.json()?)
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
        let offset = if exponent < 0 {
            1024_u64 >> 2
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
}

impl ProgressQueue {
    pub fn push(&mut self, update: ProgressUpdate) {
        self.expire();
        self.items
            .retain(|item| item.update.document != update.document);
        self.items.push_back(QueuedProgress {
            update,
            queued_at: unix_timestamp(),
        });
        while self.items.len() > QUEUE_MAX_ITEMS {
            let _ = self.items.pop_front();
        }
    }

    #[must_use]
    pub fn items(&self) -> &VecDeque<QueuedProgress> {
        &self.items
    }

    pub fn pop_front(&mut self) -> Option<QueuedProgress> {
        self.items.pop_front()
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
    fn digest_reads_exponentially_spaced_samples() -> Result<(), SyncError> {
        let path = std::env::temp_dir().join("terminalreader-partial-md5-test.bin");
        let mut file = File::create(&path)?;
        file.write_all(&vec![b'x'; 5000])?;
        drop(file);
        assert!(!partial_md5(&path)?.is_empty());
        std::fs::remove_file(path)?;
        Ok(())
    }
}
