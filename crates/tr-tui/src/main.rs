use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use tr_core::{Config, PositionStore, RecentsStore, ScanCache, logging, scan_library_cached};
use tr_epub::EpubBook;
use tr_kosync::{Credentials, KOSyncClient, ProgressQueue, filename_md5, partial_md5};

mod app;
mod sync;
mod text_input;
mod update;

use app::App;

#[derive(Debug, Parser)]
#[command(
    name = "terminalreader",
    about = "A fullscreen EPUB reader for the terminal",
    version = update::current_version()
)]
struct Cli {
    /// Write logs to this file (implies logging even if disabled in config).
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
    /// Log level: error, warn, info, or debug.
    #[arg(long, global = true)]
    log_level: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print EPUB metadata, spine items, and parsed block paths.
    Dump { book: PathBuf },
    /// Open an EPUB in the fullscreen reader.
    Read { book: PathBuf },
    /// Scan a directory recursively and list EPUBs.
    Library { directory: PathBuf },
    /// Add a book directory to the persistent library configuration.
    #[command(name = "addLibrary")]
    AddLibrary { directory: PathBuf },
    /// Print KOReader-compatible binary and filename document hashes.
    Hash { book: PathBuf },
    /// Verify local configuration, state, and optional book matching data.
    Doctor { book: Option<PathBuf> },
    /// Check for a newer release and install it.
    Update {
        /// Only check; do not install.
        #[arg(long)]
        check: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_file, cli.log_level.as_deref());
    match cli.command {
        Some(Command::Dump { book }) => dump(&book),
        Some(Command::Library { directory }) => {
            library(&directory);
            Ok(())
        }
        Some(Command::AddLibrary { directory }) => add_library(&directory),
        Some(Command::Hash { book }) => hash(&book),
        Some(Command::Doctor { book }) => doctor(book.as_deref()),
        Some(Command::Update { check }) => self_update(check),
        Some(Command::Read { book }) => run_tui(Some(book)),
        None => run_tui(None),
    }
}

fn self_update(check_only: bool) -> Result<()> {
    update::clean_stale_backup();
    let status = update::check()?;
    println!("Current version: {}", status.current);
    println!("Latest release:  {}", status.latest);
    if !status.available {
        println!("Already up to date.");
        return Ok(());
    }
    if check_only {
        println!("Update available. Run 'terminalreader update' to install it.");
        return Ok(());
    }
    println!("Downloading and installing {}…", status.latest);
    update::apply(&status.latest)?;
    println!(
        "Updated to {}. Restart terminalreader to use it.",
        status.latest
    );
    Ok(())
}

/// Enable file logging from CLI flags or the persisted config.
fn init_logging(cli_file: Option<PathBuf>, cli_level: Option<&str>) {
    let config = Config::load().unwrap_or_default();
    let enabled = cli_file.is_some() || config.logging.enabled;
    if !enabled {
        return;
    }
    let level = cli_level
        .map(str::to_owned)
        .or(config.logging.level)
        .unwrap_or_else(|| "info".to_owned());
    let path = cli_file.or(config.logging.file);
    match logging::init(path, logging::Level::parse(&level)) {
        Ok(path) => logging::info(&format!("logging started at {}", path.display())),
        Err(error) => eprintln!("warning: could not open log file: {error}"),
    }
}

fn run_tui(initial_book: Option<PathBuf>) -> Result<()> {
    let terminal = ratatui::init();
    let mouse_result = execute!(std::io::stdout(), EnableMouseCapture);
    let result = match mouse_result {
        Ok(()) => App::new(initial_book).and_then(|mut app| app.run(terminal)),
        Err(error) => Err(error.into()),
    };
    let mouse_result = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    mouse_result?;
    result
}

fn dump(path: &Path) -> Result<()> {
    let mut book = EpubBook::open(path).with_context(|| format!("opening {}", path.display()))?;
    println!("Title: {}", book.metadata.title);
    println!("Authors: {}", book.metadata.authors.join(", "));
    println!("Spine items: {}", book.spine.len());
    for (chapter_index, item) in book.spine.clone().iter().enumerate() {
        println!("\nChapter {}: {}", chapter_index + 1, item.path);
        for (block_index, block) in book.chapter_blocks(chapter_index)?.iter().enumerate() {
            println!(
                "  {:>4} {:?} {:?}",
                block_index + 1,
                block.source_path,
                block.block
            );
        }
    }
    Ok(())
}

fn library(directory: &Path) {
    let mut cache = ScanCache::load();
    for book in scan_library_cached(directory, &mut cache) {
        println!(
            "{}\t{}\t{}",
            book.metadata.title,
            book.metadata.authors.join(", "),
            book.path.display()
        );
    }
    if let Err(error) = cache.save() {
        eprintln!("warning: could not save scan cache: {error}");
    }
}

fn add_library(directory: &Path) -> Result<()> {
    let mut config = Config::load()?;
    if config.add_book_dir(directory)? {
        println!("Library added: {}", directory.canonicalize()?.display());
    } else {
        println!(
            "Library already configured: {}",
            directory.canonicalize()?.display()
        );
    }
    Ok(())
}

fn hash(path: &Path) -> Result<()> {
    println!("Binary: {}", partial_md5(path)?);
    if let Some(digest) = filename_md5(path) {
        println!("Filename: {digest}");
    }
    Ok(())
}

fn doctor(book: Option<&Path>) -> Result<()> {
    let config = Config::load()?;
    let _ = PositionStore::load()?;
    let _ = RecentsStore::load()?;
    println!("Config: OK");
    if config.library.book_dirs.is_empty() {
        println!("Libraries: none configured");
    } else {
        for directory in &config.library.book_dirs {
            let status = if directory.is_dir() { "OK" } else { "missing" };
            println!("Library: {status} ({})", directory.display());
        }
    }
    doctor_sync(&config);
    if let Some(book) = book {
        println!("Book: {}", if book.is_file() { "OK" } else { "missing" });
        if book.is_file() {
            hash(book)?;
            let method = sync::checksum_method(config.sync.matching);
            if let Ok(digest) = tr_kosync::document_digest(book, method) {
                println!("Sync document id ({:?}): {digest}", config.sync.matching);
            }
        }
    }
    Ok(())
}

fn doctor_sync(config: &Config) {
    match url::Url::parse(&config.sync.server_url) {
        Ok(_) => println!("Sync server URL: OK ({})", config.sync.server_url),
        Err(error) => {
            println!("Sync server URL: INVALID ({error})");
            return;
        }
    }
    let queue_len =
        tr_core::state_file("sync_queue.json").map_or(0, |path| ProgressQueue::load(&path).len());
    println!("Sync queue: {queue_len} pending");
    let Some(username) = &config.sync.username else {
        println!("Sync account: not configured");
        return;
    };
    println!("Sync account: {username}");
    let userkey = match tr_core::credentials::load_userkey(&config.sync.server_url, username) {
        Ok(Some(userkey)) => {
            println!("Sync credentials: found in keyring");
            userkey
        }
        Ok(None) => {
            println!("Sync credentials: MISSING from keyring (sign in again)");
            return;
        }
        Err(error) => {
            println!("Sync credentials: keyring unavailable ({error})");
            return;
        }
    };
    let client = KOSyncClient::new(
        &config.sync.server_url,
        Credentials {
            username: username.clone(),
            userkey,
        },
    );
    match client.and_then(|client| client.authorize()) {
        Ok(()) => println!("Sync server: reachable, authentication OK"),
        Err(error) => println!("Sync server: FAILED ({error})"),
    }
}
