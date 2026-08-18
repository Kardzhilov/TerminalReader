//! Application state, persistent reader positions, and EPUB library scanning.

pub mod credentials;
pub mod logging;

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tr_epub::{BookMetadata, EpubBook};

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("unable to access application state: {0}")]
    Io(#[from] std::io::Error),
    #[error("unable to parse application state: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unable to parse application config: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("unable to write application config: {0}")]
    TomlSer(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SavedPosition {
    pub chapter_index: usize,
    pub block_index: usize,
    /// Byte offset of the first visible word within the block.
    #[serde(default, alias = "block_line")]
    pub char_offset: usize,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PositionStore {
    positions: HashMap<PathBuf, SavedPosition>,
}

impl PositionStore {
    pub fn load() -> Result<Self, CoreError> {
        let path = state_file("positions.json")?;
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    #[must_use]
    pub fn get(&self, path: &Path) -> SavedPosition {
        self.positions.get(path).cloned().unwrap_or_default()
    }

    pub fn save_position(
        &mut self,
        path: PathBuf,
        position: SavedPosition,
    ) -> Result<(), CoreError> {
        self.positions.insert(path, position);
        let destination = state_file("positions.json")?;
        write_json_atomic(&destination, self)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibraryConfig {
    #[serde(default)]
    pub book_dirs: Vec<PathBuf>,
}

/// Reading preferences applied to the reader layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadingConfig {
    /// Maximum content width in columns; `None` uses the full terminal width.
    #[serde(default)]
    pub max_width: Option<u16>,
    /// Pad spaces between words so paragraph lines fill the content width.
    #[serde(default)]
    pub justify: bool,
    /// Restrict decorations to ASCII characters.
    #[serde(default)]
    pub ascii_only: bool,
}

impl Default for ReadingConfig {
    fn default() -> Self {
        Self {
            max_width: Some(100),
            justify: false,
            ascii_only: false,
        }
    }
}

/// How documents are matched against the sync server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchingMethod {
    #[default]
    Binary,
    Filename,
}

/// What to do when the server has a different position than this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncStrategy {
    #[default]
    Prompt,
    Silent,
    Disable,
}

impl SyncStrategy {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Silent => "silent",
            Self::Disable => "disable",
        }
    }

    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Prompt => Self::Silent,
            Self::Silent => Self::Disable,
            Self::Disable => Self::Prompt,
        }
    }
}

/// Persisted `KOReader` progress-sync settings (credentials live in the keyring).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    #[serde(default = "default_sync_server")]
    pub server_url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub matching: MatchingMethod,
    #[serde(default)]
    pub sync_forward: SyncStrategy,
    #[serde(default = "default_sync_backward")]
    pub sync_backward: SyncStrategy,
    /// Pull on open and push on close without manual action.
    #[serde(default = "default_true")]
    pub auto_sync: bool,
    /// Push after this many page turns; `None` disables interval pushes.
    #[serde(default)]
    pub pages_before_update: Option<u32>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
}

fn default_sync_server() -> String {
    "https://sync.koreader.rocks".to_owned()
}

fn default_sync_backward() -> SyncStrategy {
    SyncStrategy::Disable
}

fn default_true() -> bool {
    true
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            server_url: default_sync_server(),
            username: None,
            matching: MatchingMethod::default(),
            sync_forward: SyncStrategy::default(),
            sync_backward: default_sync_backward(),
            auto_sync: true,
            pages_before_update: None,
            device_name: None,
            device_id: None,
        }
    }
}

/// File logging configuration; logging is off unless enabled.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoggingConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Log destination; `None` uses `terminalreader.log` in the state directory.
    #[serde(default)]
    pub file: Option<PathBuf>,
    #[serde(default)]
    pub level: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "config_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub library: LibraryConfig,
    #[serde(default)]
    pub reading: ReadingConfig,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

fn config_schema_version() -> u32 {
    1
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: config_schema_version(),
            library: LibraryConfig::default(),
            reading: ReadingConfig::default(),
            sync: SyncConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

impl Config {
    /// Whether a config file has been written before (`false` on first run).
    #[must_use]
    pub fn exists() -> bool {
        config_file("config.toml").is_ok_and(|path| path.exists())
    }

    pub fn load() -> Result<Self, CoreError> {
        let path = config_file("config.toml")?;
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(toml::from_str(&fs::read_to_string(path)?)?)
    }

    pub fn save(&self) -> Result<(), CoreError> {
        let destination = config_file("config.toml")?;
        write_text_atomic(&destination, toml::to_string_pretty(self)?)
    }

    pub fn add_book_dir(&mut self, path: &Path) -> Result<bool, CoreError> {
        let path = normalize_book_dir(path)?;
        if self
            .library
            .book_dirs
            .iter()
            .any(|existing| existing == &path)
        {
            return Ok(false);
        }
        self.library.book_dirs.push(path);
        self.save()?;
        Ok(true)
    }

    pub fn remove_book_dir(&mut self, path: &Path) -> Result<bool, CoreError> {
        let original_len = self.library.book_dirs.len();
        self.library.book_dirs.retain(|existing| existing != path);
        let changed = self.library.book_dirs.len() != original_len;
        if changed {
            self.save()?;
        }
        Ok(changed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentBook {
    pub path: PathBuf,
    pub title: String,
    pub authors: String,
    pub spine_count: usize,
    pub last_chapter: usize,
    pub last_opened: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentsFile {
    #[serde(default = "config_schema_version")]
    version: u32,
    #[serde(default)]
    items: Vec<RecentBook>,
}

#[derive(Debug, Default)]
pub struct RecentsStore {
    items: Vec<RecentBook>,
}

impl RecentsStore {
    pub fn load() -> Result<Self, CoreError> {
        let path = state_file("recents.json")?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let file: RecentsFile = serde_json::from_slice(&fs::read(path)?)?;
        Ok(Self { items: file.items })
    }

    #[must_use]
    pub fn list(&self) -> &[RecentBook] {
        &self.items
    }

    #[must_use]
    pub fn most_recent(&self) -> Option<&RecentBook> {
        self.items.first()
    }

    pub fn touch(&mut self, mut recent: RecentBook) -> Result<(), CoreError> {
        recent.last_opened = unix_timestamp();
        self.insert_recent(recent);
        self.save()
    }

    pub fn remove(&mut self, path: &Path) -> Result<bool, CoreError> {
        let original_len = self.items.len();
        self.items.retain(|item| item.path != path);
        let changed = self.items.len() != original_len;
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    fn save(&self) -> Result<(), CoreError> {
        let destination = state_file("recents.json")?;
        write_json_atomic(
            &destination,
            &RecentsFile {
                version: config_schema_version(),
                items: self.items.clone(),
            },
        )
    }

    fn insert_recent(&mut self, recent: RecentBook) {
        self.items.retain(|item| item.path != recent.path);
        self.items.insert(0, recent);
        self.items.truncate(20);
    }
}

#[derive(Debug, Clone)]
pub struct LibraryBook {
    pub path: PathBuf,
    pub metadata: BookMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScanCacheEntry {
    size: u64,
    mtime: u64,
    title: String,
    authors: Vec<String>,
}

/// Library metadata cache keyed by path and validated by size and mtime.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ScanCache {
    #[serde(default)]
    entries: HashMap<PathBuf, ScanCacheEntry>,
    #[serde(skip)]
    dirty: bool,
}

impl ScanCache {
    /// Load the cache; missing or corrupt files yield an empty cache.
    #[must_use]
    pub fn load() -> Self {
        state_file("scan_cache.json")
            .ok()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&mut self) -> Result<(), CoreError> {
        if !self.dirty {
            return Ok(());
        }
        let destination = state_file("scan_cache.json")?;
        write_json_atomic(&destination, self)?;
        self.dirty = false;
        Ok(())
    }

    fn lookup(&self, path: &Path, size: u64, mtime: u64) -> Option<BookMetadata> {
        let entry = self.entries.get(path)?;
        (entry.size == size && entry.mtime == mtime).then(|| BookMetadata {
            title: entry.title.clone(),
            authors: entry.authors.clone(),
        })
    }

    fn store(&mut self, path: PathBuf, size: u64, mtime: u64, metadata: &BookMetadata) {
        self.entries.insert(
            path,
            ScanCacheEntry {
                size,
                mtime,
                title: metadata.title.clone(),
                authors: metadata.authors.clone(),
            },
        );
        self.dirty = true;
    }

    fn prune(&mut self, root: &Path, seen: &[PathBuf]) {
        let before = self.entries.len();
        self.entries
            .retain(|path, _| !path.starts_with(root) || seen.contains(path));
        if self.entries.len() != before {
            self.dirty = true;
        }
    }
}

#[must_use]
pub fn scan_library(root: &Path) -> Vec<LibraryBook> {
    let mut cache = ScanCache::default();
    scan_library_cached(root, &mut cache)
}

/// Scan `root`, reusing cached metadata for unchanged files.
#[must_use]
pub fn scan_library_cached(root: &Path, cache: &mut ScanCache) -> Vec<LibraryBook> {
    let mut books = Vec::new();
    let mut seen = Vec::new();
    scan_directory(root, cache, &mut books, &mut seen);
    cache.prune(root, &seen);
    books.sort_by(|left, right| left.metadata.title.cmp(&right.metadata.title));
    books
}

fn scan_directory(
    directory: &Path,
    cache: &mut ScanCache,
    books: &mut Vec<LibraryBook>,
    seen: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_directory(&path, cache, books, seen);
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("epub"))
        {
            let Some((size, mtime)) = file_signature(&path) else {
                continue;
            };
            seen.push(path.clone());
            if let Some(metadata) = cache.lookup(&path, size, mtime) {
                books.push(LibraryBook { path, metadata });
            } else if let Ok(book) = EpubBook::open(&path) {
                cache.store(path.clone(), size, mtime, &book.metadata);
                books.push(LibraryBook {
                    path,
                    metadata: book.metadata,
                });
            }
        }
    }
}

fn file_signature(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((metadata.len(), mtime))
}

pub fn state_file(name: &str) -> Result<PathBuf, CoreError> {
    let dirs = ProjectDirs::from("", "", "TerminalReader")
        .ok_or_else(|| std::io::Error::other("could not determine state directory"))?;
    let directory = dirs.state_dir().unwrap_or_else(|| dirs.data_local_dir());
    fs::create_dir_all(directory)?;
    Ok(directory.join(name))
}

fn config_file(name: &str) -> Result<PathBuf, CoreError> {
    let dirs = ProjectDirs::from("", "", "TerminalReader")
        .ok_or_else(|| std::io::Error::other("could not determine config directory"))?;
    fs::create_dir_all(dirs.config_dir())?;
    Ok(dirs.config_dir().join(name))
}

/// Path of the main configuration file.
pub fn config_path() -> Result<PathBuf, CoreError> {
    config_file("config.toml")
}

fn write_json_atomic<T: Serialize>(destination: &Path, value: &T) -> Result<(), CoreError> {
    write_bytes_atomic(destination, serde_json::to_vec_pretty(value)?)
}

fn write_text_atomic(destination: &Path, value: String) -> Result<(), CoreError> {
    write_bytes_atomic(destination, value.into_bytes())
}

fn write_bytes_atomic(destination: &Path, bytes: Vec<u8>) -> Result<(), CoreError> {
    let temporary = destination.with_extension("tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, destination)?;
    Ok(())
}

fn normalize_book_dir(path: &Path) -> Result<PathBuf, CoreError> {
    if !path.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "book directory does not exist or is not a directory",
        )
        .into());
    }
    let path = path.canonicalize()?;
    #[cfg(windows)]
    let path = {
        let display = path.to_string_lossy().into_owned();
        display.strip_prefix(r"\\?\").map_or(path, PathBuf::from)
    };
    Ok(path)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn recent(path: &str) -> RecentBook {
        RecentBook {
            path: PathBuf::from(path),
            title: path.to_owned(),
            authors: String::new(),
            spine_count: 10,
            last_chapter: 0,
            last_opened: 0,
        }
    }

    #[test]
    fn config_round_trips_as_toml() -> Result<(), CoreError> {
        let config = Config {
            schema_version: 1,
            library: LibraryConfig {
                book_dirs: vec![PathBuf::from("C:/Books")],
            },
            reading: ReadingConfig {
                max_width: Some(88),
                justify: true,
                ascii_only: true,
            },
            sync: SyncConfig {
                server_url: "https://kosync.eu".to_owned(),
                username: Some("reader".to_owned()),
                matching: MatchingMethod::Filename,
                sync_forward: SyncStrategy::Silent,
                sync_backward: SyncStrategy::Disable,
                auto_sync: false,
                pages_before_update: Some(10),
                device_name: Some("laptop".to_owned()),
                device_id: Some("abc123".to_owned()),
            },
            logging: LoggingConfig::default(),
        };
        let text = toml::to_string_pretty(&config)?;
        let parsed: Config = toml::from_str(&text)?;
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.library.book_dirs, vec![PathBuf::from("C:/Books")]);
        assert_eq!(parsed.reading.max_width, Some(88));
        assert!(parsed.reading.justify);
        assert!(parsed.reading.ascii_only);
        assert_eq!(parsed.sync.server_url, "https://kosync.eu");
        assert_eq!(parsed.sync.matching, MatchingMethod::Filename);
        assert_eq!(parsed.sync.sync_forward, SyncStrategy::Silent);
        assert_eq!(parsed.sync.pages_before_update, Some(10));
        Ok(())
    }

    #[test]
    fn legacy_config_defaults_new_sections() -> Result<(), CoreError> {
        let parsed: Config = toml::from_str("schema_version = 1\n[library]\nbook_dirs = []\n")?;
        assert_eq!(parsed.reading.max_width, Some(100));
        assert_eq!(parsed.sync.server_url, "https://sync.koreader.rocks");
        assert_eq!(parsed.sync.sync_backward, SyncStrategy::Disable);
        assert!(parsed.sync.auto_sync);
        assert!(!parsed.logging.enabled);
        Ok(())
    }

    #[test]
    fn scan_cache_reuses_unchanged_entries_and_prunes_deleted() {
        let mut cache = ScanCache::default();
        let root = PathBuf::from("/library");
        let kept = root.join("kept.epub");
        let deleted = root.join("deleted.epub");
        let metadata = BookMetadata {
            title: "Kept".to_owned(),
            authors: vec!["Author".to_owned()],
        };
        cache.store(kept.clone(), 10, 20, &metadata);
        cache.store(deleted.clone(), 1, 2, &metadata);

        assert_eq!(
            cache.lookup(&kept, 10, 20).map(|found| found.title),
            Some("Kept".to_owned())
        );
        assert_eq!(cache.lookup(&kept, 10, 21), None, "mtime change misses");
        assert_eq!(cache.lookup(&kept, 11, 20), None, "size change misses");

        cache.prune(&root, std::slice::from_ref(&kept));
        assert!(cache.lookup(&deleted, 1, 2).is_none());
        assert!(cache.lookup(&kept, 10, 20).is_some());
    }

    #[test]
    fn recents_move_existing_book_to_front_and_cap_at_twenty() {
        let mut store = RecentsStore::default();
        for index in 0..21 {
            store.insert_recent(recent(&format!("book-{index}.epub")));
        }
        assert_eq!(store.items.len(), 20);
        assert_eq!(store.items[0].path, PathBuf::from("book-20.epub"));
        store.insert_recent(recent("book-5.epub"));
        assert_eq!(store.items[0].path, PathBuf::from("book-5.epub"));
        assert_eq!(
            store
                .items
                .iter()
                .filter(|item| item.path == Path::new("book-5.epub"))
                .count(),
            1
        );
    }
}
