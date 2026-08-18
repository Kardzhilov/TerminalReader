use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use tr_core::{Config, PositionStore, RecentsStore, scan_library};
use tr_epub::EpubBook;
use tr_kosync::{filename_md5, partial_md5};

mod app;
mod text_input;

use app::App;

#[derive(Debug, Parser)]
#[command(
    name = "terminalreader",
    about = "A fullscreen EPUB reader for the terminal"
)]
struct Cli {
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Dump { book }) => dump(&book),
        Some(Command::Library { directory }) => {
            library(&directory);
            Ok(())
        }
        Some(Command::AddLibrary { directory }) => add_library(&directory),
        Some(Command::Hash { book }) => hash(&book),
        Some(Command::Doctor { book }) => doctor(book.as_deref()),
        Some(Command::Read { book }) => run_tui(Some(book)),
        None => run_tui(None),
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
    for book in scan_library(directory) {
        println!(
            "{}\t{}\t{}",
            book.metadata.title,
            book.metadata.authors.join(", "),
            book.path.display()
        );
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
    if let Some(book) = book {
        println!("Book: {}", if book.is_file() { "OK" } else { "missing" });
        if book.is_file() {
            hash(book)?;
        }
    }
    Ok(())
}
