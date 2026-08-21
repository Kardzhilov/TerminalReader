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

/// Newest saved positions kept on disk; the stalest are evicted beyond this.
const MAX_POSITIONS: usize = 500;

/// A saved position plus bookkeeping that only the store cares about.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPosition {
    #[serde(flatten)]
    position: SavedPosition,
    /// Unix time of the last save.
    #[serde(default)]
    updated: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PositionStore {
    positions: HashMap<PathBuf, StoredPosition>,
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
        self.positions
            .get(path)
            .map(|stored| stored.position.clone())
            .unwrap_or_default()
    }

    pub fn save_position(
        &mut self,
        path: PathBuf,
        position: SavedPosition,
    ) -> Result<(), CoreError> {
        self.positions.insert(
            path,
            StoredPosition {
                position,
                updated: unix_timestamp(),
            },
        );
        while self.positions.len() > MAX_POSITIONS {
            let Some(stalest) = self
                .positions
                .iter()
                .min_by_key(|(_, stored)| stored.updated)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.positions.remove(&stalest);
        }
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
    /// Rows per text line: 1 = single spacing, 2 = double spacing.
    #[serde(default = "default_line_spacing")]
    pub line_spacing: u16,
    /// Blank rows between blocks (0-3).
    #[serde(default = "default_paragraph_spacing")]
    pub paragraph_spacing: u16,
    /// First-line paragraph indent in columns (0-8).
    #[serde(default)]
    pub indent: u16,
}

fn default_line_spacing() -> u16 {
    1
}

fn default_paragraph_spacing() -> u16 {
    1
}

impl Default for ReadingConfig {
    fn default() -> Self {
        Self {
            max_width: Some(100),
            justify: false,
            ascii_only: false,
            line_spacing: default_line_spacing(),
            paragraph_spacing: default_paragraph_spacing(),
            indent: 0,
        }
    }
}

/// Color and light/dark preferences for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    /// Preset name (e.g. gruvbox, dracula, nord); "custom" uses `accent`/`light`.
    #[serde(default = "default_preset")]
    pub preset: String,
    /// Accent color name: cyan, blue, green, magenta, red, yellow, white, or gray.
    #[serde(default = "default_accent")]
    pub accent: String,
    /// Adjust secondary colors for light terminal backgrounds.
    #[serde(default)]
    pub light: bool,
}

fn default_preset() -> String {
    "custom".to_owned()
}

fn default_accent() -> String {
    "cyan".to_owned()
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            preset: default_preset(),
            accent: default_accent(),
            light: false,
        }
    }
}

/// Optional overrides for the reader's single-character keys.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeyBindings {
    #[serde(default)]
    pub contents: Option<char>,
    #[serde(default)]
    pub search: Option<char>,
    #[serde(default)]
    pub next_match: Option<char>,
    #[serde(default)]
    pub previous_match: Option<char>,
    #[serde(default)]
    pub bookmark_add: Option<char>,
    #[serde(default)]
    pub bookmarks: Option<char>,
    #[serde(default)]
    pub sync_push: Option<char>,
    #[serde(default)]
    pub sync_pull: Option<char>,
    #[serde(default)]
    pub sync_toggle: Option<char>,
    #[serde(default)]
    pub quit: Option<char>,
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
    /// Push after this many minutes of reading; `None` disables timed pushes.
    #[serde(default)]
    pub minutes_before_update: Option<u32>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    /// Books that never sync, regardless of other settings.
    #[serde(default)]
    pub excluded_books: Vec<PathBuf>,
}

fn default_sync_server() -> String {
    "https://kosync.eu".to_owned()
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
            minutes_before_update: None,
            device_name: None,
            device_id: None,
            excluded_books: Vec::new(),
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
    pub theme: ThemeConfig,
    #[serde(default)]
    pub keys: KeyBindings,
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
            theme: ThemeConfig::default(),
            keys: KeyBindings::default(),
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

    /// Load the config; a corrupt file is renamed to `config.toml.bad` and
    /// replaced with defaults so the app can still start.
    pub fn load_or_backup() -> Result<(Self, Option<PathBuf>), CoreError> {
        match Self::load() {
            Ok(config) => Ok((config, None)),
            Err(CoreError::TomlDe(_)) => {
                let path = config_file("config.toml")?;
                let backup = path.with_extension("toml.bad");
                let _ = fs::remove_file(&backup);
                fs::rename(&path, &backup)?;
                Ok((Self::default(), Some(backup)))
            }
            Err(error) => Err(error),
        }
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

/// A saved location inside a book, with a human-readable label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bookmark {
    pub chapter_index: usize,
    pub block_index: usize,
    pub char_offset: usize,
    pub label: String,
    #[serde(default)]
    pub created: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BookmarksFile {
    #[serde(default = "config_schema_version")]
    version: u32,
    #[serde(default)]
    books: HashMap<PathBuf, Vec<Bookmark>>,
}

/// Local per-book bookmarks stored in `bookmarks.json`.
#[derive(Debug, Default)]
pub struct BookmarkStore {
    books: HashMap<PathBuf, Vec<Bookmark>>,
}

impl BookmarkStore {
    pub fn load() -> Result<Self, CoreError> {
        let path = state_file("bookmarks.json")?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let file: BookmarksFile = serde_json::from_slice(&fs::read(path)?)?;
        Ok(Self { books: file.books })
    }

    #[must_use]
    pub fn list(&self, path: &Path) -> &[Bookmark] {
        self.books.get(path).map_or(&[], Vec::as_slice)
    }

    pub fn add(&mut self, path: &Path, mut bookmark: Bookmark) -> Result<(), CoreError> {
        bookmark.created = unix_timestamp();
        let entries = self.books.entry(path.to_path_buf()).or_default();
        entries.push(bookmark);
        entries.sort_by_key(|entry| (entry.chapter_index, entry.block_index, entry.char_offset));
        self.save()
    }

    pub fn remove(&mut self, path: &Path, index: usize) -> Result<bool, CoreError> {
        let Some(entries) = self.books.get_mut(path) else {
            return Ok(false);
        };
        if index >= entries.len() {
            return Ok(false);
        }
        entries.remove(index);
        if entries.is_empty() {
            self.books.remove(path);
        }
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> Result<(), CoreError> {
        let destination = state_file("bookmarks.json")?;
        write_json_atomic(
            &destination,
            &BookmarksFile {
                version: config_schema_version(),
                books: self.books.clone(),
            },
        )
    }
}

/// Accumulated reading totals for one book.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct BookStats {
    pub seconds: u64,
    pub pages: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StatsFile {
    #[serde(default = "config_schema_version")]
    version: u32,
    #[serde(default)]
    books: HashMap<PathBuf, BookStats>,
}

/// Reading statistics stored in `stats.json`.
#[derive(Debug, Default)]
pub struct StatsStore {
    books: HashMap<PathBuf, BookStats>,
}

impl StatsStore {
    pub fn load() -> Result<Self, CoreError> {
        let path = state_file("stats.json")?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let file: StatsFile = serde_json::from_slice(&fs::read(path)?)?;
        Ok(Self { books: file.books })
    }

    #[must_use]
    pub fn get(&self, path: &Path) -> BookStats {
        self.books.get(path).copied().unwrap_or_default()
    }

    /// Add a finished reading session to the book's totals.
    pub fn record(&mut self, path: &Path, seconds: u64, pages: u64) -> Result<(), CoreError> {
        if seconds == 0 && pages == 0 {
            return Ok(());
        }
        let stats = self.books.entry(path.to_path_buf()).or_default();
        stats.seconds = stats.seconds.saturating_add(seconds);
        stats.pages = stats.pages.saturating_add(pages);
        self.save()
    }

    fn save(&self) -> Result<(), CoreError> {
        let destination = state_file("stats.json")?;
        write_json_atomic(
            &destination,
            &StatsFile {
                version: config_schema_version(),
                books: self.books.clone(),
            },
        )
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
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
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
                line_spacing: 2,
                paragraph_spacing: 0,
                indent: 4,
            },
            theme: ThemeConfig {
                preset: "gruvbox".to_owned(),
                accent: "green".to_owned(),
                light: true,
            },
            keys: KeyBindings {
                contents: Some('c'),
                ..KeyBindings::default()
            },
            sync: SyncConfig {
                server_url: "https://kosync.eu".to_owned(),
                username: Some("reader".to_owned()),
                matching: MatchingMethod::Filename,
                sync_forward: SyncStrategy::Silent,
                sync_backward: SyncStrategy::Disable,
                auto_sync: false,
                pages_before_update: Some(10),
                minutes_before_update: Some(5),
                device_name: Some("laptop".to_owned()),
                device_id: Some("abc123".to_owned()),
                excluded_books: vec![PathBuf::from("C:/Books/private.epub")],
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
        assert_eq!(parsed.reading.line_spacing, 2);
        assert_eq!(parsed.reading.paragraph_spacing, 0);
        assert_eq!(parsed.reading.indent, 4);
        assert_eq!(parsed.theme.preset, "gruvbox");
        assert_eq!(parsed.theme.accent, "green");
        assert!(parsed.theme.light);
        assert_eq!(parsed.keys.contents, Some('c'));
        assert_eq!(parsed.keys.search, None);
        assert_eq!(parsed.sync.server_url, "https://kosync.eu");
        assert_eq!(parsed.sync.matching, MatchingMethod::Filename);
        assert_eq!(parsed.sync.sync_forward, SyncStrategy::Silent);
        assert_eq!(parsed.sync.pages_before_update, Some(10));
        assert_eq!(parsed.sync.minutes_before_update, Some(5));
        assert_eq!(
            parsed.sync.excluded_books,
            vec![PathBuf::from("C:/Books/private.epub")]
        );
        Ok(())
    }

    #[test]
    fn legacy_config_defaults_new_sections() -> Result<(), CoreError> {
        let parsed: Config = toml::from_str("schema_version = 1\n[library]\nbook_dirs = []\n")?;
        assert_eq!(parsed.reading.max_width, Some(100));
        assert_eq!(parsed.reading.line_spacing, 1);
        assert_eq!(parsed.reading.paragraph_spacing, 1);
        assert_eq!(parsed.reading.indent, 0);
        assert_eq!(parsed.theme.preset, "custom");
        assert_eq!(parsed.theme.accent, "cyan");
        assert!(!parsed.theme.light);
        assert_eq!(parsed.keys.contents, None);
        assert!(parsed.sync.excluded_books.is_empty());
        assert_eq!(parsed.sync.server_url, "https://kosync.eu");
        assert_eq!(parsed.sync.sync_backward, SyncStrategy::Disable);
        assert!(parsed.sync.auto_sync);
        assert!(!parsed.logging.enabled);
        Ok(())
    }

    #[test]
    fn position_store_reads_legacy_entries() -> Result<(), CoreError> {
        // Pre-timestamp entry with the old `block_line` field name.
        let json =
            r#"{"positions":{"C:/b.epub":{"chapter_index":2,"block_index":3,"block_line":4}}}"#;
        let store: PositionStore = serde_json::from_str(json)?;
        let position = store.get(Path::new("C:/b.epub"));
        assert_eq!(position.chapter_index, 2);
        assert_eq!(position.block_index, 3);
        assert_eq!(position.char_offset, 4);
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
