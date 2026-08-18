//! Application state, persistent reader positions, and EPUB library scanning.

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "config_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub library: LibraryConfig,
}

fn config_schema_version() -> u32 {
    1
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: config_schema_version(),
            library: LibraryConfig::default(),
        }
    }
}

impl Config {
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

#[must_use]
pub fn scan_library(root: &Path) -> Vec<LibraryBook> {
    let mut books = Vec::new();
    scan_directory(root, &mut books);
    books.sort_by(|left, right| left.metadata.title.cmp(&right.metadata.title));
    books
}

fn scan_directory(directory: &Path, books: &mut Vec<LibraryBook>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_directory(&path, books);
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("epub"))
        {
            if let Ok(book) = EpubBook::open(&path) {
                books.push(LibraryBook {
                    path,
                    metadata: book.metadata,
                });
            }
        }
    }
}

fn state_file(name: &str) -> Result<PathBuf, CoreError> {
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
        };
        let text = toml::to_string_pretty(&config)?;
        let parsed: Config = toml::from_str(&text)?;
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.library.book_dirs, vec![PathBuf::from("C:/Books")]);
        Ok(())
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
