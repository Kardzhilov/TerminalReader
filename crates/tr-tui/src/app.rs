use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender, channel},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line as UiLine, Span, Text},
    widgets::{
        Block as TuiBlock, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};
use sha2::{Digest, Sha256};
use tr_core::{
    Bookmark, BookmarkStore, Config, LibraryBook, PositionStore, RecentBook, RecentsStore,
    SavedPosition, ScanCache, StatsStore, SyncStrategy, credentials, logging, scan_library_cached,
};
use tr_epub::{EpubBook, InlineKind, InlineSpan};
use tr_kosync::{Credentials, ProgressRecord, ProgressUpdate, xpointer::XPointer};
use tr_render::{LayoutOptions, Line, layout_with, line_for_anchor, line_inline_ranges};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    sync::{self, SyncController, SyncEvent},
    text_input::TextInput,
    update::{UpdateController, UpdateEvent, current_version},
};

#[cfg(feature = "inline-images")]
mod images;

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 16;
/// How long footer status messages stay on screen.
const STATUS_TTL: Duration = Duration::from_secs(5);
const PROMPT_GO_LABEL: &str = "[Go to position]";
const PROMPT_STAY_LABEL: &str = "[Stay here]";
const LINK_OPEN_LABEL: &str = "[Open]";
const LINK_CANCEL_LABEL: &str = "[Cancel]";
/// Fraction of the book read at which it counts as finished.
const FINISHED_PERCENT: f64 = 0.98;
/// Selection jump for PageUp/PageDown in lists.
const LIST_PAGE_JUMP: usize = 10;

/// Styling that honors the `NO_COLOR` convention and the theme config.
#[derive(Debug, Clone, Copy)]
struct Palette {
    color: bool,
    accent: Color,
    status: Color,
    dim: Color,
    missing: Color,
}

/// A named colorway for the UI chrome.
struct ThemePreset {
    name: &'static str,
    accent: Color,
    status: Color,
    dim: Color,
    missing: Color,
}

/// Sentinel preset that uses the `accent`/`light` values from config.toml.
const CUSTOM_THEME: &str = "custom";

/// Popular colorways selectable from the settings screen.
const THEME_PRESETS: &[ThemePreset] = &[
    ThemePreset {
        name: "gruvbox",
        accent: Color::Rgb(0xfe, 0x80, 0x19),
        status: Color::Rgb(0xfa, 0xbd, 0x2f),
        dim: Color::Rgb(0x92, 0x83, 0x74),
        missing: Color::Rgb(0xfb, 0x49, 0x34),
    },
    ThemePreset {
        name: "gruvbox-light",
        accent: Color::Rgb(0xaf, 0x3a, 0x03),
        status: Color::Rgb(0xb5, 0x76, 0x14),
        dim: Color::Rgb(0x92, 0x83, 0x74),
        missing: Color::Rgb(0x9d, 0x00, 0x06),
    },
    ThemePreset {
        name: "dracula",
        accent: Color::Rgb(0xbd, 0x93, 0xf9),
        status: Color::Rgb(0xf1, 0xfa, 0x8c),
        dim: Color::Rgb(0x62, 0x72, 0xa4),
        missing: Color::Rgb(0xff, 0x55, 0x55),
    },
    ThemePreset {
        name: "nord",
        accent: Color::Rgb(0x88, 0xc0, 0xd0),
        status: Color::Rgb(0xeb, 0xcb, 0x8b),
        dim: Color::Rgb(0x61, 0x6e, 0x88),
        missing: Color::Rgb(0xbf, 0x61, 0x6a),
    },
    ThemePreset {
        name: "solarized",
        accent: Color::Rgb(0x26, 0x8b, 0xd2),
        status: Color::Rgb(0xb5, 0x89, 0x00),
        dim: Color::Rgb(0x58, 0x6e, 0x75),
        missing: Color::Rgb(0xdc, 0x32, 0x2f),
    },
    ThemePreset {
        name: "solarized-light",
        accent: Color::Rgb(0x26, 0x8b, 0xd2),
        status: Color::Rgb(0xb5, 0x89, 0x00),
        dim: Color::Rgb(0x93, 0xa1, 0xa1),
        missing: Color::Rgb(0xdc, 0x32, 0x2f),
    },
    ThemePreset {
        name: "catppuccin",
        accent: Color::Rgb(0xcb, 0xa6, 0xf7),
        status: Color::Rgb(0xf9, 0xe2, 0xaf),
        dim: Color::Rgb(0x6c, 0x70, 0x86),
        missing: Color::Rgb(0xf3, 0x8b, 0xa8),
    },
    ThemePreset {
        name: "tokyo-night",
        accent: Color::Rgb(0x7a, 0xa2, 0xf7),
        status: Color::Rgb(0xe0, 0xaf, 0x68),
        dim: Color::Rgb(0x56, 0x5f, 0x89),
        missing: Color::Rgb(0xf7, 0x76, 0x8e),
    },
    ThemePreset {
        name: "one-dark",
        accent: Color::Rgb(0x61, 0xaf, 0xef),
        status: Color::Rgb(0xe5, 0xc0, 0x7b),
        dim: Color::Rgb(0x5c, 0x63, 0x70),
        missing: Color::Rgb(0xe0, 0x6c, 0x75),
    },
];

/// The preset after `current` in the cycle: custom -> presets… -> custom.
fn next_theme_preset(current: &str) -> &'static str {
    let index = THEME_PRESETS
        .iter()
        .position(|preset| preset.name.eq_ignore_ascii_case(current));
    match index {
        None => THEME_PRESETS
            .first()
            .map_or(CUSTOM_THEME, |preset| preset.name),
        Some(index) => THEME_PRESETS
            .get(index + 1)
            .map_or(CUSTOM_THEME, |preset| preset.name),
    }
}

impl Palette {
    fn detect(theme: &tr_core::ThemeConfig) -> Self {
        let no_color = env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        let (accent, status, dim, missing) = THEME_PRESETS
            .iter()
            .find(|preset| preset.name.eq_ignore_ascii_case(&theme.preset))
            .map_or_else(
                || {
                    (
                        accent_color(&theme.accent),
                        // Yellow is unreadable on light backgrounds.
                        if theme.light {
                            Color::Blue
                        } else {
                            Color::Yellow
                        },
                        if theme.light {
                            Color::Gray
                        } else {
                            Color::DarkGray
                        },
                        Color::Red,
                    )
                },
                |preset| (preset.accent, preset.status, preset.dim, preset.missing),
            );
        Self {
            color: !no_color,
            accent,
            status,
            dim,
            missing,
        }
    }

    fn title(self) -> Style {
        if self.color {
            Style::new().fg(self.accent)
        } else {
            Style::new()
        }
    }

    fn status(self) -> Style {
        if self.color {
            Style::new().fg(self.status)
        } else {
            Style::new().add_modifier(Modifier::ITALIC)
        }
    }

    fn hover(self) -> Style {
        if self.color {
            Style::new().fg(Color::Black).bg(self.accent)
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        }
    }

    fn heading(self) -> Style {
        if self.color {
            Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        }
    }

    fn dim(self) -> Style {
        if self.color {
            Style::new().fg(self.dim)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    fn missing(self) -> Style {
        if self.color {
            Style::new().fg(self.missing)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    /// Full-row list selection.
    fn selection(self) -> Style {
        if self.color {
            Style::new().fg(Color::Black).bg(self.accent)
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        }
    }

    /// Highlighted search match in the reader text.
    fn search_mark(self) -> Style {
        if self.color {
            Style::new().fg(Color::Black).bg(self.status)
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        }
    }

    /// Footnote-reference marker in the reader text.
    fn noteref(self) -> Style {
        if self.color {
            Style::new()
                .fg(self.accent)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::new().add_modifier(Modifier::UNDERLINED)
        }
    }
}

/// Map a configured accent color name to a terminal color.
fn accent_color(name: &str) -> Color {
    match name.to_ascii_lowercase().as_str() {
        "blue" => Color::Blue,
        "green" => Color::Green,
        "magenta" => Color::Magenta,
        "red" => Color::Red,
        "yellow" => Color::Yellow,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        _ => Color::Cyan,
    }
}

#[derive(Debug)]
enum Screen {
    Home(HomeScreen),
    Library(LibraryScreen),
    Settings(SettingsScreen),
    Reader(Box<ReaderScreen>),
    Wizard(WizardScreen),
}

#[derive(Debug, Default)]
struct HomeScreen {
    selection: usize,
}

#[derive(Debug)]
struct WizardScreen {
    input: TextInput,
    message: Option<String>,
}

#[derive(Debug)]
struct LibraryScreen {
    title: String,
    books: Vec<LibraryBook>,
    filter: TextInput,
    selection: usize,
    top: usize,
    sort: LibrarySort,
}

/// Sort order of the library list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum LibrarySort {
    #[default]
    Title,
    Author,
    Newest,
}

impl LibrarySort {
    fn label(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Author => "author",
            Self::Newest => "newest",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Title => Self::Author,
            Self::Author => Self::Newest,
            Self::Newest => Self::Title,
        }
    }
}

fn sort_books(books: &mut [LibraryBook], sort: LibrarySort) {
    match sort {
        LibrarySort::Title => {
            books.sort_by(|left, right| left.metadata.title.cmp(&right.metadata.title));
        }
        LibrarySort::Author => books.sort_by_key(|book| {
            (
                book.metadata.authors.join(", ").to_lowercase(),
                book.metadata.title.clone(),
            )
        }),
        LibrarySort::Newest => books.sort_by(|left, right| {
            right
                .mtime
                .cmp(&left.mtime)
                .then_with(|| left.metadata.title.cmp(&right.metadata.title))
        }),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
enum SettingsMode {
    #[default]
    Browse,
    AddingLibrary,
    ConfirmRemove,
    EditWidth,
    EditServer,
    EditPages,
    EditMinutes,
    EditDevice,
    LoginUser {
        register: bool,
    },
    LoginPass {
        register: bool,
        username: String,
    },
}

#[derive(Debug, Default)]
struct SettingsScreen {
    selection: usize,
    mode: SettingsMode,
    input: TextInput,
    message: Option<String>,
    /// Rows scrolled off the top of the settings list.
    scroll: usize,
    /// Total content rows from the previous draw, for clamping the scroll.
    row_count: usize,
}

#[derive(Debug)]
struct TocState {
    filter: TextInput,
    selection: usize,
    top: usize,
}

/// Selection state of the bookmarks popup.
#[derive(Debug, Default)]
struct BookmarkState {
    selection: usize,
    top: usize,
    /// Label editor for the selected bookmark, when renaming.
    rename: Option<TextInput>,
}

/// Selection state of a scrollable popup list.
#[derive(Debug, Default)]
struct PopupList {
    selection: usize,
    top: usize,
}

/// A pulled server position awaiting the user's decision.
#[derive(Debug, Clone)]
struct SyncPrompt {
    position: SavedPosition,
    remote_percent: f64,
    local_percent: f64,
    device: Option<String>,
}

#[derive(Debug, Clone)]
struct LinkPrompt {
    url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TextPoint {
    line: usize,
    byte: usize,
}

#[derive(Debug, Clone, Copy)]
struct TextSelection {
    anchor: TextPoint,
    head: TextPoint,
    dragging: bool,
}

// Independent reader-state flags, not a state machine in disguise.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct ReaderScreen {
    path: PathBuf,
    book: EpubBook,
    chapter_index: usize,
    top_line: usize,
    anchor: (usize, usize),
    blocks: Vec<tr_epub::Block>,
    source_paths: Vec<Vec<tr_epub::SourcePathStep>>,
    blocks_loaded: bool,
    lines: Vec<Line>,
    selection: Option<TextSelection>,
    width: u16,
    height: u16,
    toc: Option<TocState>,
    document_digest: Option<String>,
    page_turns: u32,
    /// When the last push happened, for the timed-push interval.
    last_push: Instant,
    /// Progress string of the last push, to skip timed pushes when idle.
    last_pushed_progress: Option<String>,
    sync_prompt: Option<SyncPrompt>,
    link_prompt: Option<LinkPrompt>,
    /// Open the next laid-out chapter at its last page (backward page turn).
    open_at_end: bool,
    /// In-book search input popup, when open.
    search: Option<TextInput>,
    /// Positions of all matches from the last search.
    search_matches: Vec<SavedPosition>,
    search_index: usize,
    /// The active query, kept for highlighting matches on the page.
    search_query: Option<String>,
    /// Context snippets for the results popup, parallel to `search_matches`.
    search_snippets: Vec<String>,
    /// Search results popup, when open.
    results_open: Option<PopupList>,
    /// Go-to page/percent input popup, when open.
    goto_input: Option<TextInput>,
    /// Footnote text popup, when open.
    footnote: Option<String>,
    /// Reading statistics popup.
    stats_open: bool,
    /// Hide all chrome for distraction-free reading.
    zen: bool,
    /// Inline emphasis/noteref spans, parallel to `blocks`.
    inline: Vec<Vec<InlineSpan>>,
    /// Anchor ids, parallel to `blocks`, for footnote lookup.
    ids: Vec<Vec<String>>,
    /// Bookmarks popup, when open.
    bookmarks_open: Option<BookmarkState>,
    /// Start of the current reading session, for statistics.
    session_start: Instant,
    /// Pages turned this session, for statistics.
    session_pages: u64,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    HomeOpen(usize),
    HomeSettings,
    HomeQuit,
    LibraryOpen(usize),
    LibraryHome,
    SettingsAdd,
    SettingsRemove,
    SettingsHome,
    SelectLibraryDir(usize),
    ReaderContents,
    ReaderPrevious,
    ReaderNext,
    ReaderHome,
    TocSelect(usize),
    /// Open the image of this block index in the system viewer.
    OpenImage(usize),
    /// Show the footnote behind inline span `1` of block `0`.
    Footnote(usize, usize),
    /// Confirm opening the link behind inline span `1` of block `0`.
    OpenLink(usize, usize),
    /// Behave as if this key was pressed on the current screen.
    Key(KeyCode),
    /// Move the focused text input's cursor to the clicked column.
    InputClick,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct App {
    config: Config,
    positions: PositionStore,
    recents: RecentsStore,
    scan_cache: ScanCache,
    scanner: LibraryScanner,
    bookmarks: BookmarkStore,
    stats: StatsStore,
    sync: SyncController,
    update: UpdateController,
    palette: Palette,
    /// Never touch the network; sync is fully disabled.
    offline: bool,
    screen: Screen,
    hit_targets: Vec<(Rect, Action)>,
    hover: Option<Position>,
    /// Scroll offset of the screen currently being drawn, for hit targets.
    scroll_offset: usize,
    status: Option<String>,
    /// The displayed status and when it appeared, for auto-expiry.
    status_seen: Option<(String, Instant)>,
    pending_status: Option<String>,
    session_summary_visible: bool,
    temp_image_paths: Vec<PathBuf>,
    needs_redraw: bool,
    help: bool,
    should_exit: bool,
    next_screen: Option<Screen>,
    #[cfg(feature = "inline-images")]
    inline_images: images::InlineImages,
}

impl App {
    pub fn new(
        mut config: Config,
        config_backup: Option<PathBuf>,
        initial_book: Option<PathBuf>,
        offline: bool,
    ) -> Result<Self> {
        cleanup_stale_temp_images(Duration::from_secs(24 * 60 * 60));
        let first_run = !Config::exists();
        let mut sync = SyncController::new();
        if offline {
            // Leave the controller signed out so nothing touches the network.
        } else if let Some(username) = &config.sync.username {
            match credentials::load_userkey(&config.sync.server_url, username) {
                Ok(Some(userkey)) => {
                    sync.set_credentials(Some(Credentials {
                        username: username.clone(),
                        userkey,
                    }));
                    // Retry pushes queued while offline in a previous session.
                    sync.drain_next(&config.sync);
                }
                Ok(None) => logging::warn("sync account configured but keyring has no userkey"),
                Err(error) => logging::warn(&format!("keyring unavailable: {error}")),
            }
        }
        // Configs from older versions or hand edits may lack a device id;
        // without one, pushes would carry an empty `device_id`.
        if config.sync.username.is_some() && config.sync.device_id.is_none() {
            config.sync.device_id = Some(sync::generate_device_id());
            if let Err(error) = config.save() {
                logging::warn(&format!("could not save config: {error}"));
            }
        }
        let show_wizard =
            first_run && config.library.book_dirs.is_empty() && initial_book.is_none();
        let palette = Palette::detect(&config.theme);
        let mut app = Self {
            config,
            positions: PositionStore::load()?,
            recents: RecentsStore::load()?,
            scan_cache: ScanCache::load(),
            scanner: LibraryScanner::new(),
            bookmarks: BookmarkStore::load()?,
            stats: StatsStore::load()?,
            sync,
            update: UpdateController::new(),
            palette,
            offline,
            screen: if show_wizard {
                Screen::Wizard(WizardScreen {
                    input: TextInput::new(default_library_suggestion()),
                    message: None,
                })
            } else {
                Screen::Home(HomeScreen::default())
            },
            hit_targets: Vec::new(),
            hover: None,
            scroll_offset: 0,
            status: None,
            status_seen: None,
            pending_status: None,
            session_summary_visible: false,
            temp_image_paths: Vec::new(),
            needs_redraw: true,
            help: false,
            should_exit: false,
            next_screen: None,
            #[cfg(feature = "inline-images")]
            inline_images: images::InlineImages::detect(),
        };
        if let Some(backup) = config_backup {
            app.status = Some(format!(
                "Config file was invalid and was reset; backup: {}",
                backup.display()
            ));
        } else if offline {
            app.status = Some("Offline mode — sync is disabled.".to_owned());
        }
        if let Some(path) = initial_book {
            app.open_book(path)?;
            app.apply_screen_transition();
        }
        Ok(app)
    }

    pub fn run(&mut self, mut terminal: DefaultTerminal) -> Result<()> {
        crate::update::clean_stale_backup();
        while !self.should_exit {
            let previous_status = self.status_line();
            self.process_sync_events();
            self.process_update_events();
            self.process_scan_events();
            self.maybe_timed_push();
            self.expire_status();
            if previous_status != self.status_line() {
                self.needs_redraw = true;
            }
            if self.needs_redraw {
                terminal.draw(|frame| self.draw(frame))?;
                self.needs_redraw = false;
            }
            let timeout = self.next_poll_timeout();
            if crossterm::event::poll(timeout)? {
                let event = crossterm::event::read()?;
                self.handle_event(&event);
                while crossterm::event::poll(Duration::ZERO)? {
                    let event = crossterm::event::read()?;
                    self.handle_event(&event);
                }
            } else if self.caret_visible() {
                self.needs_redraw = true;
            }
        }
        // Give push-on-quit and queued retries a moment to finish.
        self.sync.flush(&self.config.sync, Duration::from_secs(3));
        for path in self.temp_image_paths.drain(..) {
            if let Err(error) = fs::remove_file(&path) {
                logging::debug(&format!(
                    "could not remove temp image {}: {error}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, event: &Event) {
        self.needs_redraw = true;
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key.code),
            Event::Mouse(mouse) => {
                self.hover = Some(Position::new(mouse.column, mouse.row));
                self.handle_mouse(*mouse);
                // Keys apply transitions in handle_key; clicks need it too.
                self.apply_screen_transition();
            }
            Event::Resize(_, _) => {
                if let Screen::Reader(reader) = &mut self.screen {
                    reader.invalidate_layout();
                }
            }
            _ => {}
        }
    }

    fn caret_visible(&self) -> bool {
        if !self.config.theme.caret_blink {
            return false;
        }
        match &self.screen {
            Screen::Wizard(_) | Screen::Library(_) => true,
            Screen::Settings(settings) => settings.mode != SettingsMode::Browse,
            Screen::Reader(reader) => {
                reader.search.is_some()
                    || reader.goto_input.is_some()
                    || reader.toc.is_some()
                    || reader
                        .bookmarks_open
                        .as_ref()
                        .is_some_and(|bookmarks| bookmarks.rename.is_some())
            }
            Screen::Home(_) => false,
        }
    }

    fn next_poll_timeout(&self) -> Duration {
        let mut timeout = Duration::from_secs(1);
        if self.scanner.busy || self.sync.busy() || self.update.busy {
            timeout = Duration::from_millis(100);
        }
        if self.caret_visible() {
            timeout = timeout.min(Duration::from_millis(500));
        }
        if let Some((_, since)) = &self.status_seen {
            timeout = timeout.min(STATUS_TTL.saturating_sub(since.elapsed()));
        }
        timeout.max(Duration::from_millis(10))
    }

    fn handle_key(&mut self, key: KeyCode) {
        if key == KeyCode::F(1) {
            self.help = !self.help;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        let mut screen = std::mem::replace(&mut self.screen, Screen::Home(HomeScreen::default()));
        match &mut screen {
            Screen::Home(home) => self.handle_home_key(home, key),
            Screen::Library(library) => self.handle_library_key(library, key),
            Screen::Settings(settings) => self.handle_settings_key(settings, key),
            Screen::Reader(reader) => self.handle_reader_key(reader, key),
            Screen::Wizard(wizard) => self.handle_wizard_key(wizard, key),
        }
        self.screen = screen;
        self.apply_screen_transition();
    }

    fn handle_wizard_key(&mut self, wizard: &mut WizardScreen, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                // Persist the (empty) config so the wizard runs only once.
                if let Err(error) = self.config.save() {
                    logging::warn(&format!("could not save config: {error}"));
                }
                self.next_screen = Some(Screen::Home(HomeScreen::default()));
            }
            KeyCode::Enter => {
                let path = PathBuf::from(wizard.input.value().trim());
                match self.config.add_book_dir(&path) {
                    Ok(_) => self.next_screen = Some(Screen::Home(HomeScreen::default())),
                    Err(error) => wizard.message = Some(error.to_string()),
                }
            }
            _ => {
                let _ = wizard.input.handle_key(key);
            }
        }
    }

    fn handle_home_key(&mut self, home: &mut HomeScreen, key: KeyCode) {
        let count = self.home_item_count();
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_exit = true,
            KeyCode::Char('s') => {
                self.next_screen = Some(Screen::Settings(SettingsScreen::default()));
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Up => home.selection = home.selection.saturating_sub(1),
            KeyCode::Down => home.selection = (home.selection + 1).min(count.saturating_sub(1)),
            KeyCode::PageUp => home.selection = home.selection.saturating_sub(LIST_PAGE_JUMP),
            KeyCode::PageDown => {
                home.selection = (home.selection + LIST_PAGE_JUMP).min(count.saturating_sub(1));
            }
            KeyCode::Home => home.selection = 0,
            KeyCode::End => home.selection = count.saturating_sub(1),
            KeyCode::Enter => self.activate_home_item(home.selection),
            KeyCode::Delete | KeyCode::Backspace => self.remove_home_recent(home),
            _ => {}
        }
    }

    fn handle_library_key(&mut self, library: &mut LibraryScreen, key: KeyCode) {
        let count = Self::filtered_books(library).len();
        match key {
            KeyCode::Esc => self.next_screen = Some(Screen::Home(HomeScreen::default())),
            KeyCode::Up => library.selection = library.selection.saturating_sub(1),
            KeyCode::Down => {
                library.selection = (library.selection + 1).min(count.saturating_sub(1));
            }
            KeyCode::PageUp => {
                library.selection = library.selection.saturating_sub(LIST_PAGE_JUMP);
            }
            KeyCode::PageDown => {
                library.selection =
                    (library.selection + LIST_PAGE_JUMP).min(count.saturating_sub(1));
            }
            KeyCode::Home if library.filter.value().is_empty() => library.selection = 0,
            KeyCode::End if library.filter.value().is_empty() => {
                library.selection = count.saturating_sub(1);
            }
            KeyCode::Tab => {
                library.sort = library.sort.next();
                sort_books(&mut library.books, library.sort);
                library.selection = 0;
                library.top = 0;
            }
            KeyCode::Enter => {
                if let Some(book) = Self::filtered_books(library).get(library.selection) {
                    let path = book.path.clone();
                    self.open_book_or_status(path);
                }
            }
            _ => {
                if library.filter.handle_key(key) {
                    library.selection = 0;
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_settings_key(&mut self, settings: &mut SettingsScreen, key: KeyCode) {
        match std::mem::take(&mut settings.mode) {
            SettingsMode::Browse => self.handle_settings_browse_key(settings, key),
            SettingsMode::AddingLibrary => match key {
                KeyCode::Esc => settings.input.clear(),
                KeyCode::Enter => {
                    let path = PathBuf::from(settings.input.value());
                    match self.config.add_book_dir(&path) {
                        Ok(true) => {
                            settings.message = Some("Library added.".to_owned());
                            settings.input.clear();
                        }
                        Ok(false) => {
                            settings.message = Some("Library is already configured.".to_owned());
                            settings.mode = SettingsMode::AddingLibrary;
                        }
                        Err(error) => {
                            settings.message = Some(error.to_string());
                            settings.mode = SettingsMode::AddingLibrary;
                        }
                    }
                }
                _ => {
                    let _ = settings.input.handle_key(key);
                    settings.mode = SettingsMode::AddingLibrary;
                }
            },
            SettingsMode::ConfirmRemove => match key {
                KeyCode::Enter => {
                    if let Some(path) = self
                        .config
                        .library
                        .book_dirs
                        .get(settings.selection)
                        .cloned()
                    {
                        let _ = self.config.remove_book_dir(&path);
                        settings.selection = settings.selection.saturating_sub(1);
                        settings.message = Some("Library removed.".to_owned());
                    }
                }
                _ => {
                    if key != KeyCode::Esc {
                        settings.mode = SettingsMode::ConfirmRemove;
                    }
                }
            },
            SettingsMode::EditWidth => {
                self.handle_settings_edit(settings, key, SettingsMode::EditWidth, |app, value| {
                    if value.is_empty() {
                        app.config.reading.max_width = None;
                        return Ok("Max width: full terminal width.".to_owned());
                    }
                    let width: u16 = value
                        .parse()
                        .map_err(|_| "Enter a number of columns, or leave empty.".to_owned())?;
                    app.config.reading.max_width = Some(width.clamp(20, 400));
                    Ok(format!("Max width set to {}.", width.clamp(20, 400)))
                });
            }
            SettingsMode::EditServer => {
                self.handle_settings_edit(settings, key, SettingsMode::EditServer, |app, value| {
                    let value = value.trim_end_matches('/').to_owned();
                    url::Url::parse(&value).map_err(|error| format!("Invalid URL: {error}"))?;
                    if app.config.sync.server_url != value {
                        app.config.sync.server_url = value;
                        app.sync.set_credentials(None);
                        return Ok("Server changed; sign in again.".to_owned());
                    }
                    Ok("Server unchanged.".to_owned())
                });
            }
            SettingsMode::EditPages => {
                self.handle_settings_edit(settings, key, SettingsMode::EditPages, |app, value| {
                    if value.is_empty() || value == "0" {
                        app.config.sync.pages_before_update = None;
                        return Ok("Interval pushes disabled.".to_owned());
                    }
                    let pages: u32 = value
                        .parse()
                        .map_err(|_| "Enter a page count, 0, or leave empty.".to_owned())?;
                    app.config.sync.pages_before_update = Some(pages);
                    Ok(format!("Pushing every {pages} pages."))
                });
            }
            SettingsMode::EditMinutes => {
                self.handle_settings_edit(
                    settings,
                    key,
                    SettingsMode::EditMinutes,
                    |app, value| {
                        if value.is_empty() || value == "0" {
                            app.config.sync.minutes_before_update = None;
                            return Ok("Timed pushes disabled.".to_owned());
                        }
                        let minutes: u32 = value
                            .parse()
                            .map_err(|_| "Enter minutes, 0, or leave empty.".to_owned())?;
                        app.config.sync.minutes_before_update = Some(minutes);
                        Ok(format!("Pushing every {minutes} minutes."))
                    },
                );
            }
            SettingsMode::EditDevice => {
                self.handle_settings_edit(settings, key, SettingsMode::EditDevice, |app, value| {
                    app.config.sync.device_name = (!value.is_empty()).then(|| value.to_owned());
                    Ok("Device name updated.".to_owned())
                });
            }
            SettingsMode::LoginUser { register } => match key {
                KeyCode::Esc => settings.input.clear(),
                KeyCode::Enter => {
                    let username = settings.input.value().trim().to_owned();
                    if username.is_empty() {
                        settings.message = Some("Username must not be empty.".to_owned());
                        settings.mode = SettingsMode::LoginUser { register };
                    } else {
                        settings.input = TextInput::masked();
                        settings.mode = SettingsMode::LoginPass { register, username };
                    }
                }
                _ => {
                    let _ = settings.input.handle_key(key);
                    settings.mode = SettingsMode::LoginUser { register };
                }
            },
            SettingsMode::LoginPass { register, username } => match key {
                KeyCode::Esc => settings.input.clear(),
                KeyCode::Enter => {
                    let password = settings.input.value().to_owned();
                    if password.is_empty() {
                        settings.message = Some("Password must not be empty.".to_owned());
                        settings.mode = SettingsMode::LoginPass { register, username };
                    } else {
                        self.sync
                            .login(&self.config.sync, username, &password, register);
                        settings.message = Some(if register {
                            "Registering…".to_owned()
                        } else {
                            "Signing in…".to_owned()
                        });
                        settings.input.clear();
                    }
                }
                _ => {
                    let _ = settings.input.handle_key(key);
                    settings.mode = SettingsMode::LoginPass { register, username };
                }
            },
        }
    }

    fn handle_settings_edit(
        &mut self,
        settings: &mut SettingsScreen,
        key: KeyCode,
        mode: SettingsMode,
        commit: impl FnOnce(&mut Self, &str) -> Result<String, String>,
    ) {
        // The caller took the mode; restore it only when staying in edit mode.
        match key {
            KeyCode::Esc => settings.input.clear(),
            KeyCode::Enter => {
                let value = settings.input.value().trim().to_owned();
                match commit(self, &value) {
                    Ok(message) => {
                        settings.message = Some(self.save_config_with(message));
                        settings.input.clear();
                    }
                    Err(message) => {
                        settings.message = Some(message);
                        settings.mode = mode;
                    }
                }
            }
            _ => {
                let _ = settings.input.handle_key(key);
                settings.mode = mode;
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_settings_browse_key(&mut self, settings: &mut SettingsScreen, key: KeyCode) {
        match key {
            KeyCode::Esc => self.next_screen = Some(Screen::Home(HomeScreen::default())),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('a') => {
                settings.mode = SettingsMode::AddingLibrary;
                settings.input = TextInput::default();
                settings.message = None;
            }
            KeyCode::Char('d') if !self.config.library.book_dirs.is_empty() => {
                settings.mode = SettingsMode::ConfirmRemove;
            }
            KeyCode::Char('w') => {
                let current = self
                    .config
                    .reading
                    .max_width
                    .map(|width| width.to_string())
                    .unwrap_or_default();
                settings.mode = SettingsMode::EditWidth;
                settings.input = TextInput::new(current);
            }
            KeyCode::Char('j') => {
                self.config.reading.justify = !self.config.reading.justify;
                settings.message = Some(self.save_config_with(format!(
                    "Justification {}.",
                    on_off(self.config.reading.justify)
                )));
            }
            KeyCode::Char('m') => {
                self.config.reading.ascii_only = !self.config.reading.ascii_only;
                settings.message = Some(self.save_config_with(format!(
                    "ASCII mode {}.",
                    on_off(self.config.reading.ascii_only)
                )));
            }
            KeyCode::Char('h') => {
                self.config.theme.preset = next_theme_preset(&self.config.theme.preset).to_owned();
                self.palette = Palette::detect(&self.config.theme);
                settings.message =
                    Some(self.save_config_with(format!("Theme: {}.", self.config.theme.preset)));
            }
            KeyCode::Char('k') => {
                self.config.theme.caret_blink = !self.config.theme.caret_blink;
                settings.message = Some(self.save_config_with(format!(
                    "Caret blink {}.",
                    on_off(self.config.theme.caret_blink)
                )));
            }
            KeyCode::Char('1') => {
                self.config.navigation.line_scroll = !self.config.navigation.line_scroll;
                settings.message = Some(self.save_config_with(format!(
                    "Line scrolling {}.",
                    on_off(self.config.navigation.line_scroll)
                )));
            }
            KeyCode::Char('2') => {
                self.config.navigation.wheel_scroll = !self.config.navigation.wheel_scroll;
                settings.message = Some(self.save_config_with(format!(
                    "Wheel scrolling {}.",
                    on_off(self.config.navigation.wheel_scroll)
                )));
            }
            KeyCode::Char('3') => {
                self.config.navigation.wheel_step = match self.config.navigation.wheel_step {
                    1 => 3,
                    3 => 5,
                    5 => 10,
                    _ => 1,
                };
                settings.message = Some(self.save_config_with(format!(
                    "Wheel step: {} lines.",
                    self.config.navigation.wheel_step
                )));
            }
            KeyCode::Char('4') => {
                self.config.navigation.click_to_turn = !self.config.navigation.click_to_turn;
                settings.message = Some(self.save_config_with(format!(
                    "Click to turn {}.",
                    on_off(self.config.navigation.click_to_turn)
                )));
            }
            KeyCode::Char('5') => {
                self.config.navigation.invert_click_zones =
                    !self.config.navigation.invert_click_zones;
                settings.message = Some(self.save_config_with(format!(
                    "Inverted click zones {}.",
                    on_off(self.config.navigation.invert_click_zones)
                )));
            }
            KeyCode::Char('u') => {
                settings.mode = SettingsMode::EditServer;
                settings.input = TextInput::new(self.config.sync.server_url.clone());
            }
            KeyCode::Char('c') => {
                self.config.sync.matching = match self.config.sync.matching {
                    tr_core::MatchingMethod::Binary => tr_core::MatchingMethod::Filename,
                    tr_core::MatchingMethod::Filename => tr_core::MatchingMethod::Binary,
                };
                settings.message = Some(self.save_config_with(format!(
                    "Matching method: {}.",
                    matching_label(self.config.sync.matching)
                )));
            }
            KeyCode::Char('f') => {
                self.config.sync.sync_forward = self.config.sync.sync_forward.next();
                settings.message = Some(self.save_config_with(format!(
                    "Forward sync: {}.",
                    self.config.sync.sync_forward.label()
                )));
            }
            KeyCode::Char('b') => {
                self.config.sync.sync_backward = self.config.sync.sync_backward.next();
                settings.message = Some(self.save_config_with(format!(
                    "Backward sync: {}.",
                    self.config.sync.sync_backward.label()
                )));
            }
            KeyCode::Char('t') => {
                self.config.sync.auto_sync = !self.config.sync.auto_sync;
                settings.message = Some(self.save_config_with(format!(
                    "Auto sync {}.",
                    on_off(self.config.sync.auto_sync)
                )));
            }
            KeyCode::Char('g') => {
                let current = self
                    .config
                    .sync
                    .pages_before_update
                    .map(|pages| pages.to_string())
                    .unwrap_or_default();
                settings.mode = SettingsMode::EditPages;
                settings.input = TextInput::new(current);
            }
            KeyCode::Char('e') => {
                let current = self
                    .config
                    .sync
                    .minutes_before_update
                    .map(|minutes| minutes.to_string())
                    .unwrap_or_default();
                settings.mode = SettingsMode::EditMinutes;
                settings.input = TextInput::new(current);
            }
            KeyCode::Char('n') => {
                settings.mode = SettingsMode::EditDevice;
                settings.input =
                    TextInput::new(self.config.sync.device_name.clone().unwrap_or_default());
            }
            KeyCode::Char('l') => {
                settings.mode = SettingsMode::LoginUser { register: false };
                settings.input =
                    TextInput::new(self.config.sync.username.clone().unwrap_or_default());
                settings.message = None;
            }
            KeyCode::Char('r') => {
                settings.mode = SettingsMode::LoginUser { register: true };
                settings.input = TextInput::default();
                settings.message = None;
            }
            KeyCode::Char('o') => {
                if let Some(username) = self.config.sync.username.take() {
                    if let Err(error) =
                        credentials::delete_userkey(&self.config.sync.server_url, &username)
                    {
                        logging::warn(&format!("keyring delete failed: {error}"));
                    }
                    self.sync.set_credentials(None);
                    settings.message = Some(self.save_config_with("Signed out.".to_owned()));
                } else {
                    settings.message = Some("Not signed in.".to_owned());
                }
            }
            KeyCode::Char('v') if !self.update.busy => {
                self.update.check_in_background();
                settings.message = Some("Checking for updates…".to_owned());
            }
            KeyCode::Char('i') => {
                if self.update.available.is_some() {
                    if !self.update.busy {
                        self.update.apply_in_background();
                        settings.message = Some("Downloading and installing update…".to_owned());
                    }
                } else {
                    settings.message =
                        Some("No update available; check first with 'v'.".to_owned());
                }
            }
            KeyCode::Up => settings.selection = settings.selection.saturating_sub(1),
            KeyCode::Down => {
                settings.selection = (settings.selection + 1)
                    .min(self.config.library.book_dirs.len().saturating_sub(1));
            }
            KeyCode::PageUp => settings.scroll = settings.scroll.saturating_sub(LIST_PAGE_JUMP),
            // Clamped against the row count on the next draw.
            KeyCode::PageDown => settings.scroll += LIST_PAGE_JUMP,
            KeyCode::Home => settings.scroll = 0,
            KeyCode::End => settings.scroll = settings.row_count,
            _ => {}
        }
    }

    /// Save the config and return `message`, or the save error.
    fn save_config_with(&mut self, message: String) -> String {
        match self.config.save() {
            Ok(()) => message,
            Err(error) => format!("Could not save settings: {error}"),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_reader_key(&mut self, reader: &mut ReaderScreen, key: KeyCode) {
        if reader.stats_open {
            reader.stats_open = false;
            return;
        }
        if reader.link_prompt.is_some() {
            match key {
                KeyCode::Enter | KeyCode::Char('y') => {
                    if let Some(prompt) = reader.link_prompt.take() {
                        self.status = Some(open_external_url(&prompt.url));
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') => reader.link_prompt = None,
                _ => {}
            }
            return;
        }
        if reader.footnote.is_some() {
            reader.footnote = None;
            return;
        }
        if reader.sync_prompt.is_some() {
            match key {
                KeyCode::Enter => {
                    if let Some(prompt) = reader.sync_prompt.take() {
                        reader.apply_position(&prompt.position);
                        self.sync.status = Some("Position synced from server.".to_owned());
                    }
                }
                KeyCode::Esc => reader.sync_prompt = None,
                _ => {}
            }
            return;
        }
        if reader.search.is_some() {
            self.handle_search_key(reader, key);
            return;
        }
        if reader.goto_input.is_some() {
            self.handle_goto_key(reader, key);
            return;
        }
        if reader.results_open.is_some() {
            self.handle_results_key(reader, key);
            return;
        }
        if reader.bookmarks_open.is_some() {
            self.handle_bookmarks_key(reader, key);
            return;
        }
        if reader.toc.is_some() {
            Self::handle_toc_key(reader, key);
            return;
        }
        if reader.selection.is_some() {
            match key {
                KeyCode::Esc => reader.selection = None,
                KeyCode::Enter | KeyCode::Char('y') => {
                    let text = reader.selected_text();
                    if text.is_empty() {
                        self.status = Some("Nothing selected.".to_owned());
                    } else {
                        self.status = Some(copy_osc52(&text));
                    }
                    reader.selection = None;
                }
                KeyCode::Left => reader.move_selection(-1, 0),
                KeyCode::Right => reader.move_selection(1, 0),
                KeyCode::Up => reader.move_selection(0, -1),
                KeyCode::Down => reader.move_selection(0, 1),
                KeyCode::PageUp => reader.move_selection(0, -reader.content_height_isize()),
                KeyCode::PageDown => reader.move_selection(0, reader.content_height_isize()),
                _ => {}
            }
            return;
        }
        match key {
            KeyCode::Esc => {
                if reader.search_query.is_some() || !reader.search_matches.is_empty() {
                    reader.search_query = None;
                    reader.search_matches.clear();
                    reader.search_snippets.clear();
                    reader.search_index = 0;
                    self.status = Some("Search cleared.".to_owned());
                } else {
                    self.leave_reader(reader);
                }
            }
            KeyCode::Char(' ') | KeyCode::PageDown | KeyCode::Right => {
                reader.next_page();
                Self::note_page_turn(&self.config, &mut self.sync, reader);
            }
            KeyCode::PageUp | KeyCode::Left => {
                reader.previous_page();
                Self::note_page_turn(&self.config, &mut self.sync, reader);
            }
            KeyCode::Char('v') => reader.start_selection(),
            KeyCode::Up if self.config.navigation.line_scroll => reader.scroll_lines(-1),
            KeyCode::Down if self.config.navigation.line_scroll => reader.scroll_lines(1),
            KeyCode::Char(character) => self.handle_reader_char(reader, character),
            _ => {}
        }
    }

    /// Keys for the chapters popup.
    fn handle_toc_key(reader: &mut ReaderScreen, key: KeyCode) {
        let count = reader.filtered_toc().len();
        let rows = reader.toc_visible_rows();
        match key {
            KeyCode::Esc => {
                reader.toc = None;
                return;
            }
            KeyCode::Up => {
                if let Some(toc) = &mut reader.toc {
                    toc.selection = toc.selection.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(toc) = &mut reader.toc {
                    toc.selection = (toc.selection + 1).min(count.saturating_sub(1));
                }
            }
            KeyCode::PageUp => {
                if let Some(toc) = &mut reader.toc {
                    toc.selection = toc.selection.saturating_sub(rows);
                }
            }
            KeyCode::PageDown => {
                if let Some(toc) = &mut reader.toc {
                    toc.selection = (toc.selection + rows).min(count.saturating_sub(1));
                }
            }
            KeyCode::Home
                if reader
                    .toc
                    .as_ref()
                    .is_some_and(|toc| toc.filter.value().is_empty()) =>
            {
                if let Some(toc) = &mut reader.toc {
                    toc.selection = 0;
                }
            }
            KeyCode::End
                if reader
                    .toc
                    .as_ref()
                    .is_some_and(|toc| toc.filter.value().is_empty()) =>
            {
                if let Some(toc) = &mut reader.toc {
                    toc.selection = count.saturating_sub(1);
                }
            }
            KeyCode::Enter => {
                reader.select_toc();
                return;
            }
            _ => {
                if let Some(toc) = &mut reader.toc {
                    if toc.filter.handle_key(key) {
                        toc.selection = 0;
                        toc.top = 0;
                    }
                }
            }
        }
        reader.ensure_toc_visible();
    }

    /// Dispatch a reader character key through the configurable bindings.
    fn handle_reader_char(&mut self, reader: &mut ReaderScreen, character: char) {
        let keys = reader_keys(&self.config.keys);
        if character == keys.quit {
            self.leave_reader(reader);
            self.should_exit = true;
        } else if character == keys.contents {
            reader.open_toc();
        } else if character == '?' {
            self.help = true;
        } else if character == keys.search {
            reader.search = Some(TextInput::default());
        } else if character == keys.next_match {
            self.goto_match(reader, true);
        } else if character == keys.previous_match {
            self.goto_match(reader, false);
        } else if character == keys.bookmark_add {
            self.add_bookmark(reader);
        } else if character == keys.bookmarks {
            reader.bookmarks_open = Some(BookmarkState::default());
        } else if character == keys.sync_push {
            if self.offline {
                self.status = Some("Offline mode — sync is disabled.".to_owned());
            } else {
                Self::push_progress(&self.config, &mut self.sync, reader, true);
            }
        } else if character == keys.sync_pull {
            if self.offline {
                self.status = Some("Offline mode — sync is disabled.".to_owned());
            } else if let Some(document) = reader.document_digest.clone() {
                self.sync.pull(&self.config.sync, document, true);
            } else {
                self.sync.status = Some("Not signed in.".to_owned());
            }
        } else if character == keys.sync_toggle {
            self.toggle_sync_exclusion(reader);
        } else if character == 'g' {
            reader.goto_input = Some(TextInput::default());
        } else if character == 'i' {
            reader.stats_open = true;
        } else if character == 'z' {
            reader.zen = !reader.zen;
        } else if character == ']' {
            reader.next_chapter();
        } else if character == '[' {
            reader.previous_chapter();
        }
    }

    /// Keys for the search input popup.
    fn handle_search_key(&mut self, reader: &mut ReaderScreen, key: KeyCode) {
        match key {
            KeyCode::Esc => reader.search = None,
            KeyCode::Enter => {
                let query = reader
                    .search
                    .as_ref()
                    .map(|input| input.value().trim().to_owned())
                    .unwrap_or_default();
                reader.search = None;
                if query.is_empty() {
                    // Re-open the results of the previous search, if any.
                    if !reader.search_matches.is_empty() {
                        reader.results_open = Some(PopupList {
                            selection: reader.search_index,
                            top: 0,
                        });
                    }
                } else {
                    self.run_reader_search(reader, &query);
                }
            }
            _ => {
                if let Some(input) = &mut reader.search {
                    let _ = input.handle_key(key);
                }
            }
        }
    }

    /// Keys for the go-to page/percent popup.
    fn handle_goto_key(&mut self, reader: &mut ReaderScreen, key: KeyCode) {
        match key {
            KeyCode::Esc => reader.goto_input = None,
            KeyCode::Enter => {
                let value = reader
                    .goto_input
                    .as_ref()
                    .map(|input| input.value().trim().to_owned())
                    .unwrap_or_default();
                reader.goto_input = None;
                if !value.is_empty() {
                    self.goto_target(reader, &value);
                }
            }
            _ => {
                if let Some(input) = &mut reader.goto_input {
                    let _ = input.handle_key(key);
                }
            }
        }
    }

    /// Jump to "42%" (book percentage) or "42" (page of this chapter).
    fn goto_target(&mut self, reader: &mut ReaderScreen, value: &str) {
        if let Some(percent) = value.strip_suffix('%') {
            match percent.trim().parse::<f64>() {
                Ok(number) if (0.0..=100.0).contains(&number) => {
                    let position = reader.position_for_percentage(number / 100.0);
                    reader.apply_position(&position);
                    self.status = Some(format!("Jumped to {number:.0}%."));
                }
                _ => self.status = Some("Enter a percentage between 0 and 100.".to_owned()),
            }
            return;
        }
        match value.parse::<usize>() {
            Ok(page) if page >= 1 => {
                let (_, count) = reader.page_numbers();
                let page = page.min(count);
                reader.top_line = (page - 1) * reader.content_height();
                reader.clamp_top();
                reader.update_anchor();
                self.status = Some(format!("Page {page}/{count} of this chapter."));
            }
            _ => {
                self.status = Some("Enter a page number, or a percentage like 42%.".to_owned());
            }
        }
    }

    /// Keys for the search results popup.
    fn handle_results_key(&mut self, reader: &mut ReaderScreen, key: KeyCode) {
        let count = reader.search_matches.len();
        let rows = reader.popup_visible_rows();
        match key {
            KeyCode::Esc => reader.results_open = None,
            KeyCode::Up => {
                if let Some(results) = &mut reader.results_open {
                    results.selection = results.selection.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(results) = &mut reader.results_open {
                    results.selection = (results.selection + 1).min(count.saturating_sub(1));
                }
            }
            KeyCode::PageUp => {
                if let Some(results) = &mut reader.results_open {
                    results.selection = results.selection.saturating_sub(rows);
                }
            }
            KeyCode::PageDown => {
                if let Some(results) = &mut reader.results_open {
                    results.selection = (results.selection + rows).min(count.saturating_sub(1));
                }
            }
            KeyCode::Home => {
                if let Some(results) = &mut reader.results_open {
                    results.selection = 0;
                }
            }
            KeyCode::End => {
                if let Some(results) = &mut reader.results_open {
                    results.selection = count.saturating_sub(1);
                }
            }
            KeyCode::Enter => {
                let selection = reader
                    .results_open
                    .take()
                    .map_or(0, |results| results.selection);
                self.goto_search_match(reader, selection);
            }
            _ => {}
        }
    }

    /// Jump to search match `index` and report it in the footer.
    fn goto_search_match(&mut self, reader: &mut ReaderScreen, index: usize) {
        let count = reader.search_matches.len();
        if count == 0 {
            return;
        }
        reader.search_index = index.min(count - 1);
        if let Some(position) = reader.search_matches.get(reader.search_index).cloned() {
            reader.apply_position(&position);
        }
        self.status = Some(format!("Match {}/{}", reader.search_index + 1, count));
    }

    /// Search all chapters and show the results popup.
    fn run_reader_search(&mut self, reader: &mut ReaderScreen, query: &str) {
        reader.run_search(query);
        if reader.search_matches.is_empty() {
            reader.search_query = None;
            self.status = Some(format!("No matches for \"{query}\"."));
            return;
        }
        reader.search_query = Some(query.to_owned());
        let current = (reader.chapter_index, reader.anchor.0, reader.anchor.1);
        let start = reader
            .search_matches
            .iter()
            .position(|hit| (hit.chapter_index, hit.block_index, hit.char_offset) > current)
            .unwrap_or(0);
        reader.search_index = start;
        reader.results_open = Some(PopupList {
            selection: start,
            top: 0,
        });
    }

    /// Jump to the next or previous search match, wrapping around.
    fn goto_match(&mut self, reader: &mut ReaderScreen, forward: bool) {
        let count = reader.search_matches.len();
        if count == 0 {
            self.status = Some("No search matches — press / to search.".to_owned());
            return;
        }
        reader.search_index = if forward {
            (reader.search_index + 1) % count
        } else {
            (reader.search_index + count - 1) % count
        };
        if let Some(position) = reader.search_matches.get(reader.search_index).cloned() {
            reader.apply_position(&position);
        }
        self.status = Some(format!("Match {}/{}", reader.search_index + 1, count));
    }

    /// Bookmark the current reading position.
    fn add_bookmark(&mut self, reader: &mut ReaderScreen) {
        let chapter_label = reader.chapter_label().map_or_else(
            || format!("Chapter {}", reader.chapter_index + 1),
            str::to_owned,
        );
        let snippet: String = reader
            .lines
            .get(reader.top_line..)
            .and_then(|rest| rest.iter().find(|line| !line.is_separator()))
            .map(|line| line.text.trim().chars().take(40).collect())
            .unwrap_or_default();
        let label = if snippet.is_empty() {
            chapter_label
        } else {
            format!("{chapter_label} — {snippet}")
        };
        let bookmark = Bookmark {
            chapter_index: reader.chapter_index,
            block_index: reader.anchor.0,
            char_offset: reader.anchor.1,
            label,
            created: 0,
        };
        self.status = Some(match self.bookmarks.add(&reader.path, bookmark) {
            Ok(()) => "Bookmark added.".to_owned(),
            Err(error) => format!("Could not save bookmark: {error}"),
        });
    }

    /// Keys for the bookmarks popup.
    fn handle_bookmarks_key(&mut self, reader: &mut ReaderScreen, key: KeyCode) {
        let count = self.bookmarks.list(&reader.path).len();
        let rows = reader.popup_visible_rows();
        if self.handle_bookmark_rename_key(reader, key) {
            return;
        }
        match key {
            KeyCode::Esc => reader.bookmarks_open = None,
            KeyCode::Up => {
                if let Some(state) = &mut reader.bookmarks_open {
                    state.selection = state.selection.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(state) = &mut reader.bookmarks_open {
                    state.selection = (state.selection + 1).min(count.saturating_sub(1));
                }
            }
            KeyCode::PageUp => {
                if let Some(state) = &mut reader.bookmarks_open {
                    state.selection = state.selection.saturating_sub(rows);
                }
            }
            KeyCode::PageDown => {
                if let Some(state) = &mut reader.bookmarks_open {
                    state.selection = (state.selection + rows).min(count.saturating_sub(1));
                }
            }
            KeyCode::Home => {
                if let Some(state) = &mut reader.bookmarks_open {
                    state.selection = 0;
                }
            }
            KeyCode::End => {
                if let Some(state) = &mut reader.bookmarks_open {
                    state.selection = count.saturating_sub(1);
                }
            }
            KeyCode::Char('r') if count > 0 => {
                let selection = reader
                    .bookmarks_open
                    .as_ref()
                    .map_or(0, |state| state.selection);
                let label = self
                    .bookmarks
                    .list(&reader.path)
                    .get(selection)
                    .map(|bookmark| bookmark.label.clone())
                    .unwrap_or_default();
                if let Some(state) = &mut reader.bookmarks_open {
                    state.rename = Some(TextInput::new(label));
                }
            }
            KeyCode::Enter => {
                let selection = reader
                    .bookmarks_open
                    .as_ref()
                    .map_or(0, |state| state.selection);
                if let Some(bookmark) = self.bookmarks.list(&reader.path).get(selection) {
                    let position = SavedPosition {
                        chapter_index: bookmark.chapter_index,
                        block_index: bookmark.block_index,
                        char_offset: bookmark.char_offset,
                        ..SavedPosition::default()
                    };
                    reader.bookmarks_open = None;
                    reader.apply_position(&position);
                }
            }
            KeyCode::Delete | KeyCode::Char('d') => {
                let selection = reader
                    .bookmarks_open
                    .as_ref()
                    .map_or(0, |state| state.selection);
                match self.bookmarks.remove(&reader.path, selection) {
                    Ok(true) => {
                        if let Some(state) = &mut reader.bookmarks_open {
                            state.selection = state.selection.saturating_sub(1);
                        }
                        self.status = Some("Bookmark removed.".to_owned());
                    }
                    Ok(false) => {}
                    Err(error) => {
                        self.status = Some(format!("Could not update bookmarks: {error}"));
                    }
                }
            }
            _ => {}
        }
    }

    /// Route keys into the bookmark label editor; `true` when consumed.
    fn handle_bookmark_rename_key(&mut self, reader: &mut ReaderScreen, key: KeyCode) -> bool {
        let Some(state) = &mut reader.bookmarks_open else {
            return false;
        };
        let Some(input) = &mut state.rename else {
            return false;
        };
        match key {
            KeyCode::Esc => state.rename = None,
            KeyCode::Enter => {
                let label = input.value().trim().to_owned();
                let selection = state.selection;
                state.rename = None;
                if !label.is_empty() {
                    self.status = Some(
                        match self.bookmarks.rename(&reader.path, selection, label) {
                            Ok(true) => "Bookmark renamed.".to_owned(),
                            Ok(false) => "No bookmark selected.".to_owned(),
                            Err(error) => format!("Could not update bookmarks: {error}"),
                        },
                    );
                }
            }
            _ => {
                let _ = input.handle_key(key);
            }
        }
        true
    }

    /// Toggle this book on or off the sync exclusion list.
    fn toggle_sync_exclusion(&mut self, reader: &mut ReaderScreen) {
        let path = reader.path.clone();
        if let Some(index) = self
            .config
            .sync
            .excluded_books
            .iter()
            .position(|excluded| excluded == &path)
        {
            self.config.sync.excluded_books.remove(index);
            if self.sync.logged_in() {
                match sync::digest_for(&path, self.config.sync.matching) {
                    Ok(document) => reader.document_digest = Some(document),
                    Err(error) => logging::warn(&format!("could not hash document: {error}")),
                }
            }
            self.status = Some(self.save_config_with("Sync enabled for this book.".to_owned()));
        } else {
            self.config.sync.excluded_books.push(path);
            reader.document_digest = None;
            self.status = Some(self.save_config_with("Sync disabled for this book.".to_owned()));
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.help {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.help = false;
            }
            return;
        }
        if let Screen::Reader(reader) = &mut self.screen {
            if reader.stats_open {
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    reader.stats_open = false;
                }
                return;
            }
            if reader.link_prompt.is_some() {
                if let Some(message) = Self::handle_link_prompt_mouse(reader, mouse) {
                    self.status = Some(message);
                }
                return;
            }
            if reader.footnote.is_some() {
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    reader.footnote = None;
                }
                return;
            }
            if reader.sync_prompt.is_some() {
                Self::handle_sync_prompt_mouse(reader, mouse, &mut self.sync);
                return;
            }
            if reader.search.is_some() {
                if matches!(mouse.kind, MouseEventKind::Down(_))
                    && !reader
                        .search_area()
                        .contains(Position::new(mouse.column, mouse.row))
                {
                    reader.search = None;
                }
                return;
            }
            if reader.goto_input.is_some() {
                if matches!(mouse.kind, MouseEventKind::Down(_))
                    && !reader
                        .search_area()
                        .contains(Position::new(mouse.column, mouse.row))
                {
                    reader.goto_input = None;
                }
                return;
            }
            if reader.results_open.is_some() {
                self.handle_results_mouse(mouse);
                return;
            }
            if reader.bookmarks_open.is_some() {
                self.handle_bookmarks_mouse(mouse);
                return;
            }
            if reader.toc.is_some() {
                reader.handle_toc_mouse(mouse);
                return;
            }
            if reader.selection.is_some() {
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left)
                    | MouseEventKind::Up(MouseButton::Left) => {
                        reader.handle_selection_mouse(mouse);
                        return;
                    }
                    _ => {}
                }
            }
        }
        if self.handle_screen_mouse(mouse) {
            return;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        if let Some((rect, action)) = self
            .hit_targets
            .iter()
            .find(|(rect, _)| rect.contains(Position::new(mouse.column, mouse.row)))
            .copied()
        {
            match action {
                Action::InputClick => {
                    self.click_focused_input(mouse.column.saturating_sub(rect.x));
                }
                Action::Key(code) => self.handle_key(code),
                other => self.activate(other),
            }
        }
    }

    /// Modal: only the prompt's buttons react; clicking outside dismisses.
    fn handle_sync_prompt_mouse(
        reader: &mut ReaderScreen,
        mouse: MouseEvent,
        sync: &mut SyncController,
    ) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let area = reader.sync_prompt_area();
        let position = Position::new(mouse.column, mouse.row);
        if !area.contains(position) {
            reader.sync_prompt = None;
            return;
        }
        let buttons_row = area.y + 5;
        if mouse.row != buttons_row {
            return;
        }
        let go_start = area.x + 1;
        let go_end = go_start + u16::try_from(PROMPT_GO_LABEL.len()).unwrap_or(0);
        if (go_start..go_end).contains(&mouse.column) {
            if let Some(prompt) = reader.sync_prompt.take() {
                reader.apply_position(&prompt.position);
                sync.status = Some("Position synced from server.".to_owned());
            }
        } else {
            reader.sync_prompt = None;
        }
    }

    fn handle_link_prompt_mouse(reader: &mut ReaderScreen, mouse: MouseEvent) -> Option<String> {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        let area = reader.link_prompt_area();
        let position = Position::new(mouse.column, mouse.row);
        if !area.contains(position) {
            reader.link_prompt = None;
            return None;
        }
        let buttons_row = area.y + area.height.saturating_sub(2);
        if mouse.row != buttons_row {
            return None;
        }
        let open_start = area.x + 1;
        let open_end = open_start + u16::try_from(LINK_OPEN_LABEL.len()).unwrap_or(0);
        if (open_start..open_end).contains(&mouse.column) {
            return reader
                .link_prompt
                .take()
                .map(|prompt| open_external_url(&prompt.url));
        }
        reader.link_prompt = None;
        None
    }

    /// Mouse in the bookmarks popup: wheel moves, click jumps, clicking
    /// outside closes.
    fn handle_bookmarks_mouse(&mut self, mouse: MouseEvent) {
        let mut screen = std::mem::replace(&mut self.screen, Screen::Home(HomeScreen::default()));
        if let Screen::Reader(reader) = &mut screen {
            let area = reader.toc_area();
            let count = self.bookmarks.list(&reader.path).len();
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(state) = &mut reader.bookmarks_open {
                        state.selection = state.selection.saturating_sub(1);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(state) = &mut reader.bookmarks_open {
                        state.selection = (state.selection + 1).min(count.saturating_sub(1));
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let position = Position::new(mouse.column, mouse.row);
                    if !area.contains(position) {
                        reader.bookmarks_open = None;
                    } else if mouse.row > area.y
                        && mouse.row < area.y + area.height.saturating_sub(1)
                    {
                        let top = reader.bookmarks_open.as_ref().map_or(0, |state| state.top);
                        let index = top + usize::from(mouse.row - area.y - 1);
                        if let Some(bookmark) = self.bookmarks.list(&reader.path).get(index) {
                            let position = SavedPosition {
                                chapter_index: bookmark.chapter_index,
                                block_index: bookmark.block_index,
                                char_offset: bookmark.char_offset,
                                ..SavedPosition::default()
                            };
                            reader.bookmarks_open = None;
                            reader.apply_position(&position);
                        }
                    }
                }
                _ => {}
            }
        }
        self.screen = screen;
    }

    /// Mouse in the search results popup: wheel moves, click jumps,
    /// clicking outside closes.
    fn handle_results_mouse(&mut self, mouse: MouseEvent) {
        let mut screen = std::mem::replace(&mut self.screen, Screen::Home(HomeScreen::default()));
        if let Screen::Reader(reader) = &mut screen {
            let area = reader.toc_area();
            let count = reader.search_matches.len();
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(results) = &mut reader.results_open {
                        results.selection = results.selection.saturating_sub(1);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(results) = &mut reader.results_open {
                        results.selection = (results.selection + 1).min(count.saturating_sub(1));
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let position = Position::new(mouse.column, mouse.row);
                    if !area.contains(position) {
                        reader.results_open = None;
                    } else if mouse.row > area.y
                        && mouse.row < area.y + area.height.saturating_sub(1)
                    {
                        let top = reader
                            .results_open
                            .as_ref()
                            .map_or(0, |results| results.top);
                        let index = top + usize::from(mouse.row - area.y - 1);
                        if index < count {
                            reader.results_open = None;
                            self.goto_search_match(reader, index);
                        }
                    }
                }
                _ => {}
            }
        }
        self.screen = screen;
    }

    /// Route a click at `offset` columns into whichever input has focus.
    fn click_focused_input(&mut self, offset: u16) {
        match &mut self.screen {
            Screen::Wizard(wizard) => wizard.input.click(offset),
            Screen::Library(library) => library.filter.click(offset),
            Screen::Settings(settings) => settings.input.click(offset),
            Screen::Home(_) | Screen::Reader(_) => {}
        }
    }

    fn handle_screen_mouse(&mut self, mouse: MouseEvent) -> bool {
        let home_item_count = self.home_item_count();
        match &mut self.screen {
            Screen::Home(home) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => home.selection = home.selection.saturating_sub(1),
                    MouseEventKind::ScrollDown => {
                        home.selection =
                            (home.selection + 1).min(home_item_count.saturating_sub(1));
                    }
                    _ => return false,
                }
                true
            }
            Screen::Library(library) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    library.selection = library.selection.saturating_sub(1);
                    true
                }
                MouseEventKind::ScrollDown => {
                    library.selection = (library.selection + 1)
                        .min(Self::filtered_books(library).len().saturating_sub(1));
                    true
                }
                _ => false,
            },
            Screen::Settings(settings) => match mouse.kind {
                MouseEventKind::ScrollUp if settings.mode == SettingsMode::Browse => {
                    settings.scroll = settings.scroll.saturating_sub(1);
                    true
                }
                MouseEventKind::ScrollDown if settings.mode == SettingsMode::Browse => {
                    // Clamped against the row count on the next draw.
                    settings.scroll += 1;
                    true
                }
                _ => false,
            },
            Screen::Reader(reader) => match mouse.kind {
                MouseEventKind::ScrollUp if self.config.navigation.wheel_scroll => {
                    let step = isize::try_from(self.config.navigation.wheel_step.clamp(1, 10))
                        .unwrap_or(3);
                    reader.scroll_lines(-step);
                    true
                }
                MouseEventKind::ScrollDown if self.config.navigation.wheel_scroll => {
                    let step = isize::try_from(self.config.navigation.wheel_step.clamp(1, 10))
                        .unwrap_or(3);
                    reader.scroll_lines(step);
                    true
                }
                _ => false,
            },
            Screen::Wizard(_) => false,
        }
    }

    fn activate(&mut self, action: Action) {
        match action {
            Action::HomeOpen(index) => self.activate_home_item(index),
            Action::HomeSettings => {
                self.next_screen = Some(Screen::Settings(SettingsScreen::default()));
            }
            Action::HomeQuit => self.should_exit = true,
            Action::LibraryOpen(index) => {
                let path = if let Screen::Library(library) = &self.screen {
                    Self::filtered_books(library)
                        .get(index)
                        .map(|book| book.path.clone())
                } else {
                    None
                };
                if let Some(path) = path {
                    self.open_book_or_status(path);
                }
            }
            Action::LibraryHome | Action::SettingsHome => {
                self.next_screen = Some(Screen::Home(HomeScreen::default()));
            }
            Action::SettingsAdd => {
                if let Screen::Settings(settings) = &mut self.screen {
                    settings.mode = SettingsMode::AddingLibrary;
                    settings.input = TextInput::default();
                }
            }
            Action::SettingsRemove => {
                if let Screen::Settings(settings) = &mut self.screen {
                    if !self.config.library.book_dirs.is_empty() {
                        settings.mode = SettingsMode::ConfirmRemove;
                    }
                }
            }
            Action::ReaderContents => {
                if let Screen::Reader(reader) = &mut self.screen {
                    reader.open_toc();
                }
            }
            Action::ReaderPrevious => {
                if let Screen::Reader(reader) = &mut self.screen {
                    reader.previous_page();
                    Self::note_page_turn(&self.config, &mut self.sync, reader);
                }
            }
            Action::ReaderNext => {
                if let Screen::Reader(reader) = &mut self.screen {
                    reader.next_page();
                    Self::note_page_turn(&self.config, &mut self.sync, reader);
                }
            }
            Action::ReaderHome => {
                if let Screen::Reader(reader) = &mut self.screen {
                    if self.config.sync.auto_sync {
                        Self::push_progress(&self.config, &mut self.sync, reader, false);
                    }
                    self.session_summary_visible =
                        Self::record_session(&mut self.stats, &mut self.status, reader);
                    let position = reader.position();
                    let recent = RecentBook {
                        path: reader.path.clone(),
                        title: reader.book.metadata.title.clone(),
                        authors: reader.book.metadata.authors.join(", "),
                        spine_count: reader.book.spine.len(),
                        last_chapter: reader.chapter_index,
                        last_opened: 0,
                    };
                    let _ = self.positions.save_position(reader.path.clone(), position);
                    let _ = self.recents.touch(recent);
                }
                self.next_screen = Some(Screen::Home(HomeScreen::default()));
            }
            Action::TocSelect(index) => {
                if let Screen::Reader(reader) = &mut self.screen {
                    reader.select_filtered_toc(index);
                }
            }
            Action::OpenImage(block) => {
                if let Screen::Reader(reader) = &mut self.screen {
                    let (message, path) = open_reader_image(reader, block);
                    if let Some(path) = path {
                        self.temp_image_paths.push(path);
                    }
                    self.status = Some(message);
                }
            }
            Action::Footnote(block, span) => self.open_footnote(block, span),
            Action::OpenLink(block, span) => self.open_link(block, span),
            Action::SelectLibraryDir(index) => {
                if let Screen::Settings(settings) = &mut self.screen {
                    settings.selection = index;
                }
            }
            Action::Key(code) => self.handle_key(code),
            Action::InputClick => {}
        }
    }

    /// Look up and open the note behind a clicked footnote marker.
    fn open_footnote(&mut self, block: usize, span: usize) {
        let Screen::Reader(reader) = &mut self.screen else {
            return;
        };
        let href = match reader.inline.get(block).and_then(|spans| spans.get(span)) {
            Some(InlineSpan {
                kind: InlineKind::Noteref(href),
                ..
            }) => href.clone(),
            _ => return,
        };
        if is_external_link(&href) {
            reader.link_prompt = Some(LinkPrompt { url: href });
            return;
        }
        match reader.footnote_text(&href) {
            Some(text) => reader.footnote = Some(text),
            None => self.status = Some("Could not find the footnote text.".to_owned()),
        }
    }

    fn open_link(&mut self, block: usize, span: usize) {
        let Screen::Reader(reader) = &mut self.screen else {
            return;
        };
        let href = match reader.inline.get(block).and_then(|spans| spans.get(span)) {
            Some(InlineSpan {
                kind: InlineKind::Link(href),
                ..
            }) => href.clone(),
            _ => return,
        };
        if is_external_link(&href) {
            reader.link_prompt = Some(LinkPrompt { url: href });
        } else {
            self.status = Some("In-book link navigation is not supported yet.".to_owned());
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.hit_targets.clear();
        self.scroll_offset = 0;
        let area = frame.area();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            frame.render_widget(Paragraph::new("TerminalReader needs a terminal of at least 60 x 16.\nResize the window to continue."), area);
            return;
        }
        let mut screen = std::mem::replace(&mut self.screen, Screen::Home(HomeScreen::default()));
        match &mut screen {
            Screen::Home(home) => self.draw_home(frame, home),
            Screen::Library(library) => self.draw_library(frame, library),
            Screen::Settings(settings) => self.draw_settings(frame, settings),
            Screen::Reader(reader) => self.draw_reader(frame, reader),
            Screen::Wizard(wizard) => self.draw_wizard(frame, wizard),
        }
        if self.help {
            self.draw_help(frame, &screen);
        }
        self.screen = screen;
    }

    fn draw_wizard(&mut self, frame: &mut Frame, wizard: &WizardScreen) {
        let area = frame.area();
        let mut rows = vec![
            String::new(),
            "Welcome to TerminalReader!".to_owned(),
            String::new(),
            "Choose a folder that contains your EPUB files.".to_owned(),
            "It will be scanned recursively when you open it.".to_owned(),
            String::new(),
        ];
        self.push_input_row(area, &mut rows, &wizard.input, "Directory: ");
        rows.push(String::new());
        self.push_hint_row(
            area,
            &mut rows,
            "Enter: save | Esc: skip for now (add libraries in Settings later)",
        );
        if let Some(message) = &wizard.message {
            rows.push(String::new());
            rows.push(message.clone());
        }
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(" First-run setup ")
            .title_style(self.palette.title());
        frame.render_widget(
            Paragraph::new(self.styled_rows(area, rows)).block(block),
            area,
        );
    }

    fn draw_help(&self, frame: &mut Frame, screen: &Screen) {
        let area = frame.area();
        let mut rows: Vec<String> = vec![
            "Global".to_owned(),
            "  F1 / ?      toggle this help".to_owned(),
            "  Esc         back / close".to_owned(),
            String::new(),
        ];
        rows.extend(self.help_rows(screen));
        let width = area.width.saturating_sub(8).clamp(44, 60);
        let height = u16::try_from(rows.len() + 2)
            .unwrap_or(u16::MAX)
            .min(area.height.saturating_sub(2));
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        let widget = Paragraph::new(rows.join("\n")).block(
            TuiBlock::default()
                .borders(Borders::ALL)
                .title(" Help — any key to close "),
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(widget, popup);
    }

    /// The screen-specific section of the help popup.
    fn help_rows(&self, screen: &Screen) -> Vec<String> {
        match screen {
            Screen::Home(_) => [
                "Home",
                "  arrows      move selection",
                "  Enter       open selection",
                "  Del         remove selection from Recent",
                "  s           settings",
                "  q           quit",
            ]
            .map(String::from)
            .to_vec(),
            Screen::Library(_) => [
                "Library",
                "  type        filter books",
                "  arrows      move selection",
                "  PgUp/PgDn   page through the list",
                "  Tab         sort by title / author / newest",
                "  Enter       open book",
            ]
            .map(String::from)
            .to_vec(),
            Screen::Settings(_) => [
                "Settings",
                "  a/d         add / remove library",
                "  w j m       max width / justify / ASCII mode",
                "  h k         color theme / caret blink",
                "  1-5         navigation preferences",
                "  u c         sync server / matching method",
                "  f b t       forward / backward / auto sync",
                "  g e         push every N pages / N minutes",
                "  l r o n     login / register / logout / device name",
                "  v i         check for updates / install update",
            ]
            .map(String::from)
            .to_vec(),
            Screen::Reader(_) => {
                let keys = reader_keys(&self.config.keys);
                vec![
                    "Reader".to_owned(),
                    "  Space PgDn →   next page".to_owned(),
                    "  PgUp ←         previous page".to_owned(),
                    "  ↑ ↓ / wheel    scroll by line".to_owned(),
                    "  [ ]            previous / next chapter".to_owned(),
                    format!("  {}              table of contents", keys.contents),
                    format!("  {}              search in this book", keys.search),
                    format!(
                        "  {} / {}          next / previous match",
                        keys.next_match, keys.previous_match
                    ),
                    format!(
                        "  {} / {}          add bookmark / show bookmarks",
                        keys.bookmark_add, keys.bookmarks
                    ),
                    "  g              go to page or percent".to_owned(),
                    "  i              reading statistics".to_owned(),
                    "  z              zen mode (hide chrome)".to_owned(),
                    "  v              select text; arrows extend, Enter copies".to_owned(),
                    format!(
                        "  {} {}            push / pull progress now",
                        keys.sync_push, keys.sync_pull
                    ),
                    format!(
                        "  {}              toggle sync for this book",
                        keys.sync_toggle
                    ),
                    "  click sides    turn page; click notes/images/links".to_owned(),
                    "  Esc            clear search / save and go home".to_owned(),
                    format!("  {}              save and quit", keys.quit),
                ]
            }
            Screen::Wizard(_) => [
                "Setup",
                "  Enter       save library directory",
                "  Esc         skip",
            ]
            .map(String::from)
            .to_vec(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn draw_home(&mut self, frame: &mut Frame, home: &mut HomeScreen) {
        let area = frame.area();
        let content_width = area.width.saturating_sub(2);
        let ascii = self.config.reading.ascii_only;
        let continue_book = self
            .recents
            .most_recent()
            .filter(|recent| recent.path.exists())
            .cloned();
        let continue_row = continue_book.as_ref().map(|recent| {
            let stats = self.stats.get(&recent.path);
            let read = if stats.seconds > 0 {
                format!(" — {} read", format_duration(stats.seconds))
            } else {
                String::new()
            };
            format!(
                "{} — ch. {}/{}{read}",
                recent.title,
                recent.last_chapter + 1,
                recent.spine_count
            )
        });
        let recent_rows: Vec<(String, bool)> = self
            .recents
            .list()
            .iter()
            .map(|recent| {
                let finished = self.positions.get(&recent.path).percent >= FINISHED_PERCENT;
                (recent_row(recent, finished), !recent.path.exists())
            })
            .collect();
        let dir_rows: Vec<String> = self
            .config
            .library
            .book_dirs
            .iter()
            .map(|directory| directory.display().to_string())
            .collect();

        let items = self.home_items();
        let mut item_iter = items.iter().copied().enumerate().peekable();
        let mut rows = Vec::new();
        let mut selected_row = None;
        let mut muted_rows = Vec::new();
        let mut status_rows = Vec::new();
        if matches!(item_iter.peek(), Some((_, HomeItem::Continue))) {
            let index = item_iter.next().map_or(0, |(index, _)| index);
            rows.push("Continue reading".to_owned());
            self.register_row(area, rows.len(), Action::HomeOpen(index));
            if index == home.selection {
                selected_row = Some(rows.len());
            }
            rows.push(format!("  {}", continue_row.unwrap_or_default()));
            // Mini progress gauge with a time-left estimate.
            if let Some(recent) = &continue_book {
                let percent = self.positions.get(&recent.path).percent;
                if percent > 0.0 {
                    let left = estimate_time_left(self.stats.get(&recent.path).seconds, percent)
                        .map(|seconds| format!(" · ~{} left", format_duration(seconds)))
                        .unwrap_or_default();
                    muted_rows.push(rows.len());
                    rows.push(format!(
                        "  {} {:.0}%{left}",
                        progress_gauge(percent, 10, ascii),
                        percent * 100.0
                    ));
                }
            }
        } else {
            rows.push("Open a library or run terminalreader read <file>".to_owned());
        }
        rows.push(String::new());
        rows.push("Recent".to_owned());
        let recent_start = rows.len();
        let mut missing_rows = Vec::new();
        while let Some((index, HomeItem::Recent(list_index))) = item_iter.peek().copied() {
            item_iter.next();
            let Some((text, missing)) = recent_rows.get(list_index) else {
                continue;
            };
            self.register_row(area, rows.len(), Action::HomeOpen(index));
            if *missing {
                missing_rows.push(rows.len());
            }
            if index == home.selection {
                selected_row = Some(rows.len());
            }
            rows.push(format!("  {text}"));
        }
        if rows.len() == recent_start {
            rows.push("  (nothing yet)".to_owned());
        }
        rows.push(String::new());
        rows.push("Libraries".to_owned());
        for (index, item) in item_iter {
            let text = match item {
                HomeItem::AllBooks => format!(
                    "All books ({} libraries)",
                    self.config.library.book_dirs.len()
                ),
                HomeItem::Library(dir_index) => {
                    dir_rows.get(dir_index).cloned().unwrap_or_default()
                }
                HomeItem::AddLibrary => "+ Add a library…".to_owned(),
                HomeItem::Continue | HomeItem::Recent(_) => continue,
            };
            self.register_row(area, rows.len(), Action::HomeOpen(index));
            if index == home.selection {
                selected_row = Some(rows.len());
            }
            rows.push(format!("  {text}"));
        }
        // Sync summary and update hint.
        let mut extras: Vec<(String, bool)> = Vec::new();
        if self.offline {
            extras.push(("Sync: offline mode (--offline)".to_owned(), false));
        } else if let Some(username) = &self.config.sync.username {
            let queued = self.sync.queue_len();
            let text = if !self.sync.logged_in() {
                format!("Sync: {username} — keyring locked or signed out")
            } else if queued > 0 {
                format!("Sync: signed in as {username} — {queued} queued")
            } else {
                format!("Sync: signed in as {username}")
            };
            extras.push((text, false));
        }
        if let Some(tag) = &self.update.available {
            extras.push((
                format!("Update {tag} available — install in Settings (s, then i)"),
                true,
            ));
        }
        if !extras.is_empty() {
            rows.push(String::new());
            for (text, highlight) in extras {
                if highlight {
                    status_rows.push(rows.len());
                } else {
                    muted_rows.push(rows.len());
                }
                rows.push(text);
            }
        }
        // Pad the selected row so its highlight spans the whole line.
        if let Some(row) = selected_row {
            if let Some(text) = rows.get_mut(row) {
                *text = pad_row(text, content_width);
            }
        }
        home.selection = home.selection.min(items.len().saturating_sub(1));
        let footer = " [Settings] [Quit]  Enter: open | arrows: move | ?: help | q: quit ";
        self.register_footer(
            area,
            footer,
            &[
                ("[Settings]", Action::HomeSettings),
                ("[Quit]", Action::HomeQuit),
            ],
        );
        let mut block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(" TerminalReader ")
            .title_style(self.palette.title())
            .title_bottom(self.styled_footer(area, footer));
        if let Some(status) = self.status_line() {
            block = block.title_bottom(
                UiLine::from(format!(" {status} "))
                    .style(self.palette.status())
                    .right_aligned(),
            );
        }
        let mut text = self.styled_rows(area, rows);
        for row in missing_rows {
            if let Some(line) = text.lines.get_mut(row) {
                line.style = self.palette.missing();
            }
        }
        for row in muted_rows {
            if let Some(line) = text.lines.get_mut(row) {
                line.style = self.palette.dim();
            }
        }
        for row in status_rows {
            if let Some(line) = text.lines.get_mut(row) {
                line.style = self.palette.status();
            }
        }
        if let Some(row) = selected_row {
            if let Some(line) = text.lines.get_mut(row) {
                line.style = self.palette.selection();
            }
        }
        frame.render_widget(Paragraph::new(text).block(block), area);
    }

    fn draw_library(&mut self, frame: &mut Frame, library: &mut LibraryScreen) {
        let area = frame.area();
        let books = Self::filtered_books(library)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        library.selection = library.selection.min(books.len().saturating_sub(1));
        let visible = usize::from(area.height.saturating_sub(5)).max(1);
        // Keep the selection inside the visible window.
        if library.selection < library.top {
            library.top = library.selection;
        } else if library.selection >= library.top + visible {
            library.top = library.selection + 1 - visible;
        }
        library.top = library.top.min(books.len().saturating_sub(1));
        let content_width = area.width.saturating_sub(2);
        // Column layout: title, authors, progress.
        let progress_width = 7;
        let text_width = usize::from(content_width)
            .saturating_sub(2 + progress_width + 4)
            .max(10);
        let title_width = text_width * 3 / 5;
        let author_width = text_width - title_width;
        let ascii = self.config.reading.ascii_only;
        let blink = self.config.theme.caret_blink;
        let mut rows = vec![
            library.filter.render_caret("Search: ", blink),
            String::new(),
        ];
        self.register_input_row(area, 0, "Search: ");
        let mut selected_row = None;
        for (index, book) in books.iter().enumerate().skip(library.top).take(visible) {
            self.register_row(area, rows.len(), Action::LibraryOpen(index));
            let progress =
                book_progress_cell(&self.positions.get(&book.path), book.spine_count, ascii);
            let row = format!(
                "  {}  {}  {progress}",
                fit(&book.metadata.title, title_width),
                fit(&book.metadata.authors.join(", "), author_width),
            );
            if index == library.selection {
                selected_row = Some(rows.len());
                rows.push(pad_row(&row, content_width));
            } else {
                rows.push(row);
            }
        }
        if books.is_empty() {
            rows.push("No matching EPUBs.".to_owned());
        }
        let footer = " [Home]  type: filter | Enter: open | Tab: sort | Esc: home ";
        self.register_footer(area, footer, &[("[Home]", Action::LibraryHome)]);
        let counts = if library.filter.value().is_empty() {
            format!("({})", books.len())
        } else {
            format!("({}/{})", books.len(), library.books.len())
        };
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(format!(
                " Library: {} — by {} {counts} ",
                library.title,
                library.sort.label()
            ))
            .title_style(self.palette.title())
            .title_bottom(self.styled_footer(area, footer));
        let mut text = self.styled_rows(area, rows);
        if let Some(row) = selected_row {
            if let Some(line) = text.lines.get_mut(row) {
                line.style = self.palette.selection();
            }
        }
        frame.render_widget(Paragraph::new(text).block(block), area);
        self.render_popup_scrollbar(frame, area, books.len(), visible, library.top);
    }

    /// Render a text input row and make its value area clickable.
    fn push_input_row(
        &mut self,
        area: Rect,
        rows: &mut Vec<String>,
        input: &TextInput,
        label: &str,
    ) {
        self.register_input_row(area, rows.len(), label);
        rows.push(input.render_caret(label, self.config.theme.caret_blink));
    }

    /// Render a modal hint row with clickable Enter/Esc segments.
    fn push_hint_row(&mut self, area: Rect, rows: &mut Vec<String>, hint: &str) {
        self.register_row_labels(
            area,
            rows.len(),
            hint,
            &[
                ("Enter:", Action::Key(KeyCode::Enter)),
                ("Esc:", Action::Key(KeyCode::Esc)),
            ],
        );
        rows.push(hint.to_owned());
    }

    #[allow(clippy::too_many_lines)]
    fn draw_settings(&mut self, frame: &mut Frame, settings: &mut SettingsScreen) {
        let area = frame.area();
        let visible = usize::from(area.height.saturating_sub(2)).max(1);
        // Clamp against the previous draw's row count; edit modes pin the
        // input rows (appended last) into view so you never type blind.
        let max_scroll = settings.row_count.saturating_sub(visible);
        settings.scroll = if settings.mode == SettingsMode::Browse {
            settings.scroll.min(max_scroll)
        } else {
            max_scroll
        };
        self.scroll_offset = settings.scroll;
        let dir_rows: Vec<String> = self
            .config
            .library
            .book_dirs
            .iter()
            .map(|directory| directory.display().to_string())
            .collect();
        let mut rows: Vec<String> = Vec::new();
        let header = "Libraries  (a: add, d: remove)".to_owned();
        self.register_row_labels(
            area,
            rows.len(),
            &header,
            &[
                ("a: add", Action::Key(KeyCode::Char('a'))),
                ("d: remove", Action::Key(KeyCode::Char('d'))),
            ],
        );
        rows.push(header);
        if dir_rows.is_empty() {
            rows.push("  (none configured)".to_owned());
        }
        let mut selected_row = None;
        for (index, directory) in dir_rows.iter().enumerate() {
            self.register_row(area, rows.len(), Action::SelectLibraryDir(index));
            if index == settings.selection {
                selected_row = Some(rows.len());
                rows.push(pad_row(
                    &format!("  {directory}"),
                    area.width.saturating_sub(2),
                ));
            } else {
                rows.push(format!("  {directory}"));
            }
        }
        rows.push(String::new());
        rows.push("Reading".to_owned());
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('w')));
        rows.push(format!(
            "  [w] Max width: {} — widest text column, in characters",
            self.config
                .reading
                .max_width
                .map_or("full".to_owned(), |width| width.to_string())
        ));
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('j')));
        rows.push(format!(
            "  [j] Justify: {} — stretch lines so both margins are even",
            on_off(self.config.reading.justify)
        ));
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('m')));
        rows.push(format!(
            "  [m] ASCII mode: {} — swap curly quotes/dashes for plain ones",
            on_off(self.config.reading.ascii_only)
        ));
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('h')));
        let theme_row = rows.len();
        rows.push(format!(
            "  [h] Theme: {} — cycle presets; custom uses [theme] in config.toml",
            self.config.theme.preset
        ));
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('k')));
        rows.push(format!(
            "  [k] Caret blink: {} — a steady caret reduces motion",
            on_off(self.config.theme.caret_blink)
        ));
        rows.push(String::new());
        rows.push("Navigation".to_owned());
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('1')));
        rows.push(format!(
            "  [1] Line scrolling: {} — use up/down to move one line",
            on_off(self.config.navigation.line_scroll)
        ));
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('2')));
        rows.push(format!(
            "  [2] Wheel scrolling: {} — use the mouse wheel in the reader",
            on_off(self.config.navigation.wheel_scroll)
        ));
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('3')));
        rows.push(format!(
            "  [3] Wheel step: {} lines — cycle 1, 3, 5, or 10",
            self.config.navigation.wheel_step
        ));
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('4')));
        rows.push(format!(
            "  [4] Click to turn: {} — click the left or right page half",
            on_off(self.config.navigation.click_to_turn)
        ));
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('5')));
        rows.push(format!(
            "  [5] Invert click zones: {} — swap previous and next halves",
            on_off(self.config.navigation.invert_click_zones)
        ));
        rows.push(String::new());
        rows.push("Progress sync (KOReader-compatible)".to_owned());
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('u')));
        rows.push(format!("  [u] Server: {}", self.config.sync.server_url));
        let account = match (&self.config.sync.username, self.sync.logged_in()) {
            (Some(username), true) => format!("{username} (signed in)"),
            (Some(username), false) => format!("{username} (keyring locked or signed out)"),
            (None, _) => "not signed in".to_owned(),
        };
        let account_row = format!("  [l] Login  [r] Register  [o] Logout — account: {account}");
        self.register_row_labels(
            area,
            rows.len(),
            &account_row,
            &[
                ("[l]", Action::Key(KeyCode::Char('l'))),
                ("[r]", Action::Key(KeyCode::Char('r'))),
                ("[o]", Action::Key(KeyCode::Char('o'))),
            ],
        );
        rows.push(account_row);
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('c')));
        rows.push(format!(
            "  [c] Matching: {} — how books are identified on the server",
            matching_label(self.config.sync.matching)
        ));
        let strategies_row = format!(
            "  [f] Forward: {}   [b] Backward: {} — when the server is ahead / behind",
            self.config.sync.sync_forward.label(),
            self.config.sync.sync_backward.label()
        );
        self.register_row_labels(
            area,
            rows.len(),
            &strategies_row,
            &[
                ("[f]", Action::Key(KeyCode::Char('f'))),
                ("[b]", Action::Key(KeyCode::Char('b'))),
            ],
        );
        rows.push(strategies_row);
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('t')));
        rows.push(format!(
            "  [t] Auto sync: {} — pull position on open, push on close and quit",
            on_off(self.config.sync.auto_sync)
        ));
        let cadence_row = format!(
            "  [g] Push every: {}   [e] Push every: {} — also push while reading",
            self.config
                .sync
                .pages_before_update
                .map_or("off".to_owned(), |pages| format!("{pages} pages")),
            self.config
                .sync
                .minutes_before_update
                .map_or("off".to_owned(), |minutes| format!("{minutes} min"))
        );
        self.register_row_labels(
            area,
            rows.len(),
            &cadence_row,
            &[
                ("[g]", Action::Key(KeyCode::Char('g'))),
                ("[e]", Action::Key(KeyCode::Char('e'))),
            ],
        );
        rows.push(cadence_row);
        self.register_row(area, rows.len(), Action::Key(KeyCode::Char('n')));
        rows.push(format!(
            "  [n] Device: {}",
            self.config
                .sync
                .device_name
                .as_deref()
                .unwrap_or("(default)")
        ));
        if self.sync.queue_len() > 0 {
            rows.push(format!(
                "  Offline queue: {} pending",
                self.sync.queue_len()
            ));
        }
        rows.push(String::new());
        rows.push("Application".to_owned());
        let update_state = if self.update.busy {
            " — working…".to_owned()
        } else {
            self.update
                .available
                .as_ref()
                .map(|tag| format!(" — {tag} available, press i"))
                .unwrap_or_default()
        };
        let update_row = format!(
            "  [v] Check for updates  [i] Install update — version {}{update_state}",
            current_version()
        );
        self.register_row_labels(
            area,
            rows.len(),
            &update_row,
            &[
                ("[v]", Action::Key(KeyCode::Char('v'))),
                ("[i]", Action::Key(KeyCode::Char('i'))),
            ],
        );
        rows.push(update_row);
        rows.push(String::new());
        match &settings.mode {
            SettingsMode::AddingLibrary => {
                self.push_input_row(area, &mut rows, &settings.input, "Add directory: ");
                self.push_hint_row(area, &mut rows, "Enter: save | Esc: cancel");
            }
            SettingsMode::ConfirmRemove => {
                self.push_hint_row(
                    area,
                    &mut rows,
                    "Remove selected library? Enter: confirm | Esc: cancel",
                );
            }
            SettingsMode::EditWidth => {
                self.push_input_row(
                    area,
                    &mut rows,
                    &settings.input,
                    "Max width (empty = full): ",
                );
                self.push_hint_row(area, &mut rows, "Enter: save | Esc: cancel");
            }
            SettingsMode::EditServer => {
                self.push_input_row(area, &mut rows, &settings.input, "Server URL: ");
                self.push_hint_row(area, &mut rows, "Enter: save | Esc: cancel");
            }
            SettingsMode::EditPages => {
                self.push_input_row(
                    area,
                    &mut rows,
                    &settings.input,
                    "Pages between pushes (0 = off): ",
                );
                self.push_hint_row(area, &mut rows, "Enter: save | Esc: cancel");
            }
            SettingsMode::EditMinutes => {
                self.push_input_row(
                    area,
                    &mut rows,
                    &settings.input,
                    "Minutes between pushes (0 = off): ",
                );
                self.push_hint_row(area, &mut rows, "Enter: save | Esc: cancel");
            }
            SettingsMode::EditDevice => {
                self.push_input_row(area, &mut rows, &settings.input, "Device name: ");
                self.push_hint_row(area, &mut rows, "Enter: save | Esc: cancel");
            }
            SettingsMode::LoginUser { register } => {
                let label = if *register {
                    "Register — username: "
                } else {
                    "Login — username: "
                };
                self.push_input_row(area, &mut rows, &settings.input, label);
                self.push_hint_row(area, &mut rows, "Enter: next | Esc: cancel");
            }
            SettingsMode::LoginPass { username, .. } => {
                let label = format!("Password for {username}: ");
                self.push_input_row(area, &mut rows, &settings.input, &label);
                self.push_hint_row(area, &mut rows, "Enter: sign in | Esc: cancel");
            }
            SettingsMode::Browse => {}
        }
        if let Some(message) = &settings.message {
            rows.push(message.clone());
        }
        if let Some(status) = &self.sync.status {
            rows.push(format!("Sync: {status}"));
        }
        let footer = " [Add] [Remove] [Home]  keys in brackets | ?: help | Esc: home ";
        self.register_footer(
            area,
            footer,
            &[
                ("[Add]", Action::SettingsAdd),
                ("[Remove]", Action::SettingsRemove),
                ("[Home]", Action::SettingsHome),
            ],
        );
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(" Settings ")
            .title_style(self.palette.title())
            .title_bottom(self.styled_footer(area, footer));
        settings.row_count = rows.len();
        let scroll = settings.scroll;
        let visible_rows: Vec<String> = rows.into_iter().skip(scroll).collect();
        let mut text = self.styled_rows(area, visible_rows);
        // Theme color swatches so cycling with h previews instantly.
        if self.palette.color {
            if let Some(line) = theme_row
                .checked_sub(scroll)
                .and_then(|row| text.lines.get_mut(row))
            {
                let swatch: &'static str = if self.config.reading.ascii_only {
                    "# "
                } else {
                    "■ "
                };
                line.spans.push(Span::raw("  "));
                for color in [
                    self.palette.accent,
                    self.palette.status,
                    self.palette.dim,
                    self.palette.missing,
                ] {
                    line.spans
                        .push(Span::styled(swatch, Style::new().fg(color)));
                }
            }
        }
        if let Some(row) = selected_row {
            if let Some(line) = row
                .checked_sub(scroll)
                .and_then(|row| text.lines.get_mut(row))
            {
                line.style = self.palette.selection();
            }
        }
        frame.render_widget(Paragraph::new(text).block(block), area);
    }

    #[allow(clippy::too_many_lines)]
    fn draw_reader(&mut self, frame: &mut Frame, reader: &mut ReaderScreen) {
        let area = frame.area();
        let options = LayoutOptions {
            ascii_only: self.config.reading.ascii_only,
            justify: self.config.reading.justify,
            max_width: self.config.reading.max_width,
            line_spacing: self.config.reading.line_spacing,
            paragraph_spacing: self.config.reading.paragraph_spacing,
            indent: self.config.reading.indent,
        };
        reader.ensure_layout(area.width, area.height, options);
        let inner = if reader.zen {
            area
        } else {
            area.inner(Margin::new(1, 1))
        };
        let content = self.reader_content(reader);
        self.register_reader_targets(reader, inner);
        if reader.zen {
            frame.render_widget(Paragraph::new(Text::from(content)), inner);
        } else {
            let (page, count) = reader.page_numbers();
            let percent = reader.percentage() * 100.0;
            let gauge = progress_gauge(reader.percentage(), 8, self.config.reading.ascii_only);
            let queued = self.sync.queue_len();
            let badge = if queued > 0
                && self.config.sync.auto_sync
                && self.config.sync.username.is_some()
            {
                format!("| {queued} queued ")
            } else {
                String::new()
            };
            let matches = if reader.search_matches.is_empty() {
                String::new()
            } else {
                format!(
                    "| match {}/{} ",
                    reader.search_index + 1,
                    reader.search_matches.len()
                )
            };
            let footer = format!(
                " [Contents] [Previous] [Next] [Home]  [/] chapter | ?: help | page {page}/{count} | {gauge} {percent:.0}% {matches}{badge}"
            );
            self.register_footer(
                area,
                &footer,
                &[
                    ("[Contents]", Action::ReaderContents),
                    ("[Previous]", Action::ReaderPrevious),
                    ("[Next]", Action::ReaderNext),
                    ("[Home]", Action::ReaderHome),
                ],
            );
            let title = reader.chapter_label().map_or_else(
                || {
                    format!(
                        " {} — chapter {}/{} ",
                        reader.book.metadata.title,
                        reader.chapter_index + 1,
                        reader.book.spine.len()
                    )
                },
                |label| {
                    format!(
                        " {} — {} ({}/{}) ",
                        reader.book.metadata.title,
                        label,
                        reader.chapter_index + 1,
                        reader.book.spine.len()
                    )
                },
            );
            let mut block = TuiBlock::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(self.palette.title())
                .title_bottom(self.styled_footer(area, &footer));
            if let Some(status) = self.status_line() {
                block = block.title_bottom(
                    UiLine::from(format!(" {status} "))
                        .style(self.palette.status())
                        .right_aligned(),
                );
            }
            frame.render_widget(Paragraph::new(Text::from(content)).block(block), area);
            if reader.lines.len() > reader.content_height() {
                let mut scroll_state =
                    ScrollbarState::new(reader.lines.len().saturating_sub(reader.content_height()))
                        .position(reader.top_line);
                frame.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .begin_symbol(None)
                        .end_symbol(None)
                        .style(self.palette.dim()),
                    area.inner(Margin::new(0, 1)),
                    &mut scroll_state,
                );
            }
        }
        if reader.toc.is_some() {
            self.draw_reader_toc(frame, reader);
        }
        #[cfg(feature = "inline-images")]
        if !reader.popup_open() {
            self.inline_images.render(frame, reader, inner);
        }
        if reader.search.is_some() {
            self.draw_search(frame, reader);
        }
        if reader.goto_input.is_some() {
            self.draw_goto(frame, reader);
        }
        if reader.results_open.is_some() {
            self.draw_results(frame, reader);
        }
        if reader.bookmarks_open.is_some() {
            self.draw_bookmarks(frame, reader);
        }
        if reader.stats_open {
            self.draw_reader_stats(frame, reader);
        }
        if reader.footnote.is_some() {
            Self::draw_footnote(frame, reader);
        }
        if reader.sync_prompt.is_some() {
            self.draw_sync_prompt(frame, reader);
        }
        if reader.link_prompt.is_some() {
            self.draw_link_prompt(frame, reader);
        }
    }

    /// Reader page lines with block styling, inline emphasis, and search
    /// match highlighting.
    fn reader_content(&self, reader: &ReaderScreen) -> Vec<UiLine<'static>> {
        reader
            .visible_lines()
            .iter()
            .enumerate()
            .map(|(row, line)| {
                let base = match reader.blocks.get(line.block) {
                    Some(tr_epub::Block::Heading { .. }) => self.palette.heading(),
                    Some(
                        tr_epub::Block::Quote(_)
                        | tr_epub::Block::Image { .. }
                        | tr_epub::Block::Rule,
                    ) => self.palette.dim(),
                    _ => Style::new(),
                };
                let layers = self.reader_line_layers(reader, reader.top_line + row, line);
                UiLine::from(styled_segments(&line.text, base, &layers))
            })
            .collect()
    }

    /// Style layers for one reader line: inline emphasis and note markers,
    /// plus highlighted search matches.
    fn reader_line_layers(
        &self,
        reader: &ReaderScreen,
        line_index: usize,
        line: &Line,
    ) -> Vec<(usize, usize, Style)> {
        let mut layers = Vec::new();
        if let (Some(block), Some(spans)) =
            (reader.blocks.get(line.block), reader.inline.get(line.block))
        {
            if let Some(text) = sync::block_text(block) {
                for (start, end, span) in line_inline_ranges(line, text, spans) {
                    let style = match spans.get(span).map(|span| &span.kind) {
                        Some(InlineKind::Emphasis) => Style::new().add_modifier(Modifier::ITALIC),
                        Some(InlineKind::Strong) => Style::new().add_modifier(Modifier::BOLD),
                        Some(InlineKind::Noteref(_)) => self.palette.noteref(),
                        Some(InlineKind::Link(_)) => {
                            self.palette.noteref().add_modifier(Modifier::UNDERLINED)
                        }
                        None => Style::new(),
                    };
                    layers.push((start, end, style));
                }
            }
        }
        if let Some(query) = &reader.search_query {
            for (start, end) in find_matches(&line.text, query) {
                layers.push((start, end, self.palette.search_mark()));
            }
        }
        if let Some((start, end)) = reader.selection_range(line_index) {
            layers.push((start, end, self.palette.selection()));
        }
        layers
    }

    /// Clickable regions of the reader page: image boxes, footnote markers,
    /// and — registered last so everything else wins — the page-turn halves.
    fn register_reader_targets(&mut self, reader: &ReaderScreen, inner: Rect) {
        for (row, line) in reader.visible_lines().iter().enumerate() {
            let Ok(row_offset) = u16::try_from(row) else {
                break;
            };
            if row_offset >= inner.height {
                break;
            }
            let y = inner.y + row_offset;
            if line.atomic {
                if let Some(tr_epub::Block::Image { href: Some(_), .. }) =
                    reader.blocks.get(line.block)
                {
                    self.hit_targets.push((
                        Rect::new(inner.x, y, inner.width, 1),
                        Action::OpenImage(line.block),
                    ));
                }
                continue;
            }
            let (Some(block), Some(spans)) =
                (reader.blocks.get(line.block), reader.inline.get(line.block))
            else {
                continue;
            };
            let Some(text) = sync::block_text(block) else {
                continue;
            };
            for (start, end, span) in line_inline_ranges(line, text, spans) {
                let action = match spans.get(span).map(|span| &span.kind) {
                    Some(InlineKind::Noteref(_)) => Action::Footnote(line.block, span),
                    Some(InlineKind::Link(_)) => Action::OpenLink(line.block, span),
                    _ => continue,
                };
                if start >= end {
                    continue;
                }
                let prefix = UnicodeWidthStr::width(line.text.get(..start).unwrap_or_default());
                let width = UnicodeWidthStr::width(line.text.get(start..end).unwrap_or_default());
                self.hit_targets.push((
                    Rect::new(
                        inner.x + u16::try_from(prefix).unwrap_or(0),
                        y,
                        u16::try_from(width).unwrap_or(1).max(1),
                        1,
                    ),
                    action,
                ));
            }
        }
        if !self.config.navigation.click_to_turn {
            return;
        }
        // Click the left/right half of the page to turn it.
        let half = inner.width / 2;
        let (left, right) = if self.config.navigation.invert_click_zones {
            (Action::ReaderNext, Action::ReaderPrevious)
        } else {
            (Action::ReaderPrevious, Action::ReaderNext)
        };
        self.hit_targets
            .push((Rect::new(inner.x, inner.y, half, inner.height), left));
        self.hit_targets.push((
            Rect::new(
                inner.x + half,
                inner.y,
                inner.width.saturating_sub(half),
                inner.height,
            ),
            right,
        ));
    }

    fn draw_search(&self, frame: &mut Frame, reader: &ReaderScreen) {
        let Some(input) = &reader.search else { return };
        let area = reader.search_area();
        let text = format!(
            "{}\nEnter: search all chapters | Esc: close",
            input.render_caret("Find: ", self.config.theme.caret_blink)
        );
        let widget =
            Paragraph::new(text).block(TuiBlock::default().borders(Borders::ALL).title(" Search "));
        frame.render_widget(Clear, area);
        frame.render_widget(widget, area);
    }

    fn draw_goto(&self, frame: &mut Frame, reader: &ReaderScreen) {
        let Some(input) = &reader.goto_input else {
            return;
        };
        let area = reader.search_area();
        let text = format!(
            "{}\nEnter: chapter page or book percent (42%) | Esc: close",
            input.render_caret("Go to: ", self.config.theme.caret_blink)
        );
        let widget =
            Paragraph::new(text).block(TuiBlock::default().borders(Borders::ALL).title(" Go to "));
        frame.render_widget(Clear, area);
        frame.render_widget(widget, area);
    }

    /// Search results list with context snippets.
    fn draw_results(&mut self, frame: &mut Frame, reader: &mut ReaderScreen) {
        let area = reader.toc_area();
        let visible = reader.popup_visible_rows();
        let count = reader.search_matches.len();
        let query = reader.search_query.clone().unwrap_or_default();
        let content_width = area.width.saturating_sub(2);
        let Some(results) = &mut reader.results_open else {
            return;
        };
        results.selection = results.selection.min(count.saturating_sub(1));
        if results.selection < results.top {
            results.top = results.selection;
        } else if results.selection >= results.top + visible {
            results.top = results.selection + 1 - visible;
        }
        let mut lines: Vec<UiLine<'static>> = Vec::new();
        for index in results.top..(results.top + visible).min(count) {
            let chapter = reader
                .search_matches
                .get(index)
                .map_or(0, |hit| hit.chapter_index);
            let snippet = reader.search_snippets.get(index).map_or("", String::as_str);
            let row = format!("  ch. {:>3}  {snippet}", chapter + 1);
            let mut line = UiLine::from(pad_row(&row, content_width));
            if index == results.selection {
                line.style = self.palette.selection();
            }
            lines.push(line);
        }
        let top = results.top;
        let widget = Paragraph::new(Text::from(lines)).block(
            TuiBlock::default().borders(Borders::ALL).title(format!(
                " Matches for \"{query}\" ({count}) — Enter: go, Esc: close ",
            )),
        );
        frame.render_widget(Clear, area);
        frame.render_widget(widget, area);
        self.render_popup_scrollbar(frame, area, count, visible, top);
    }

    /// Scrollbar along a popup's right border when the list overflows.
    fn render_popup_scrollbar(
        &self,
        frame: &mut Frame,
        area: Rect,
        count: usize,
        visible: usize,
        top: usize,
    ) {
        if count <= visible {
            return;
        }
        let mut state = ScrollbarState::new(count.saturating_sub(visible)).position(top);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .style(self.palette.dim()),
            area.inner(Margin::new(0, 1)),
            &mut state,
        );
    }

    /// Session and lifetime reading statistics for the open book.
    #[allow(clippy::cast_precision_loss)]
    fn draw_reader_stats(&self, frame: &mut Frame, reader: &ReaderScreen) {
        let totals = self.stats.get(&reader.path);
        let session_seconds = reader.session_start.elapsed().as_secs();
        let seconds = totals.seconds + session_seconds;
        let pages = totals.pages + reader.session_pages;
        let percent = reader.percentage();
        let (page, count) = reader.page_numbers();
        let speed = if seconds >= 60 && pages > 0 {
            format!("~{} pages/h", pages * 3600 / seconds)
        } else {
            "—".to_owned()
        };
        let left = estimate_time_left(seconds, percent).map_or_else(
            || "—".to_owned(),
            |left| format!("~{}", format_duration(left)),
        );
        let rows = [
            format!(
                "This session  {} · {} pages",
                format_duration(session_seconds),
                reader.session_pages
            ),
            format!("This book     {} · {pages} pages", format_duration(seconds)),
            format!(
                "Progress      {:.0}% · page {page}/{count} of chapter {}/{}",
                percent * 100.0,
                reader.chapter_index + 1,
                reader.book.spine.len()
            ),
            format!("Pace          {speed}"),
            format!("Time left     {left} (whole book)"),
        ];
        let width = reader.width.saturating_sub(8).clamp(44, 58);
        let height = u16::try_from(rows.len() + 2)
            .unwrap_or(7)
            .min(reader.height.saturating_sub(2));
        let popup = Rect::new(
            (reader.width.saturating_sub(width)) / 2,
            (reader.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        let widget = Paragraph::new(rows.join("\n")).block(
            TuiBlock::default()
                .borders(Borders::ALL)
                .title(" Reading statistics — any key to close "),
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(widget, popup);
    }

    fn draw_footnote(frame: &mut Frame, reader: &ReaderScreen) {
        let Some(note) = &reader.footnote else { return };
        let width = reader.width.saturating_sub(8).clamp(30, 64);
        let inner_width = usize::from(width.saturating_sub(2)).max(1);
        let text_rows = UnicodeWidthStr::width(note.as_str())
            .div_ceil(inner_width)
            .max(1);
        let height = u16::try_from(text_rows + 2)
            .unwrap_or(u16::MAX)
            .clamp(3, 12)
            .min(reader.height.saturating_sub(2));
        let popup = Rect::new(
            (reader.width.saturating_sub(width)) / 2,
            (reader.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        let widget = Paragraph::new(note.clone())
            .wrap(Wrap { trim: true })
            .block(
                TuiBlock::default()
                    .borders(Borders::ALL)
                    .title(" Footnote — any key to close "),
            );
        frame.render_widget(Clear, popup);
        frame.render_widget(widget, popup);
    }

    fn draw_bookmarks(&mut self, frame: &mut Frame, reader: &mut ReaderScreen) {
        let area = reader.toc_area();
        let entries = self.bookmarks.list(&reader.path);
        let visible = usize::from(area.height.saturating_sub(2)).max(1);
        let blink = self.config.theme.caret_blink;
        let content_width = area.width.saturating_sub(2);
        let Some(state) = &mut reader.bookmarks_open else {
            return;
        };
        state.selection = state.selection.min(entries.len().saturating_sub(1));
        if state.selection < state.top {
            state.top = state.selection;
        } else if state.selection >= state.top + visible {
            state.top = state.selection + 1 - visible;
        }
        let renaming = state.rename.is_some();
        let capacity = if renaming {
            visible.saturating_sub(1).max(1)
        } else {
            visible
        };
        let mut lines: Vec<UiLine<'static>> = Vec::new();
        if entries.is_empty() {
            lines.push(UiLine::from(
                "No bookmarks yet — press m in the reader to add one.".to_owned(),
            ));
        }
        for (index, bookmark) in entries.iter().enumerate().skip(state.top).take(capacity) {
            let date = if bookmark.created > 0 {
                format!(" · {}", relative_time(bookmark.created))
            } else {
                String::new()
            };
            let row = format!(
                "  ch. {:>3}{date} · {}",
                bookmark.chapter_index + 1,
                bookmark.label
            );
            let mut line = UiLine::from(pad_row(&row, content_width));
            if index == state.selection {
                line.style = self.palette.selection();
            }
            lines.push(line);
        }
        if let Some(input) = &state.rename {
            lines.push(UiLine::from(input.render_caret("New label: ", blink)));
        }
        let top = state.top;
        let widget = Paragraph::new(Text::from(lines)).block(
            TuiBlock::default().borders(Borders::ALL).title(format!(
                " Bookmarks ({}) — Enter: go, r: rename, d: delete, Esc: close ",
                entries.len()
            )),
        );
        frame.render_widget(Clear, area);
        frame.render_widget(widget, area);
        self.render_popup_scrollbar(frame, area, entries.len(), visible, top);
    }

    fn draw_sync_prompt(&self, frame: &mut Frame, reader: &ReaderScreen) {
        let Some(prompt) = &reader.sync_prompt else {
            return;
        };
        let popup = reader.sync_prompt_area();
        let direction = if prompt.remote_percent > prompt.local_percent {
            "ahead of"
        } else {
            "behind"
        };
        let device = prompt.device.as_deref().unwrap_or("another device");
        let buttons_y = popup.y + 5;
        let hovered = |start: u16, length: usize| {
            self.hover.is_some_and(|hover| {
                hover.y == buttons_y
                    && hover.x >= start
                    && hover.x < start + u16::try_from(length).unwrap_or(0)
            })
        };
        let go_x = popup.x + 1;
        let stay_x = go_x + u16::try_from(PROMPT_GO_LABEL.len() + 2).unwrap_or(0);
        let style_for = |is_hovered: bool| {
            if is_hovered {
                self.palette.hover()
            } else {
                Style::new()
            }
        };
        let buttons = UiLine::from(vec![
            Span::styled(
                PROMPT_GO_LABEL,
                style_for(hovered(go_x, PROMPT_GO_LABEL.len())),
            ),
            Span::raw("  "),
            Span::styled(
                PROMPT_STAY_LABEL,
                style_for(hovered(stay_x, PROMPT_STAY_LABEL.len())),
            ),
            Span::raw("  (Enter / Esc)"),
        ]);
        let text = Text::from(vec![
            UiLine::from(vec![
                Span::styled(device.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(format!(" is {direction} this device.")),
            ]),
            UiLine::from(""),
            UiLine::from(vec![
                Span::raw("Server: "),
                Span::styled(
                    format!("{:.1}%", prompt.remote_percent * 100.0),
                    self.palette.status(),
                ),
                Span::raw("   Here: "),
                Span::styled(
                    format!("{:.1}%", prompt.local_percent * 100.0),
                    self.palette.title(),
                ),
            ]),
            UiLine::from(""),
            buttons,
        ]);
        let widget = Paragraph::new(text).block(
            TuiBlock::default()
                .borders(Borders::ALL)
                .title(" Sync position "),
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(widget, popup);
    }

    fn draw_link_prompt(&self, frame: &mut Frame, reader: &ReaderScreen) {
        let Some(prompt) = &reader.link_prompt else {
            return;
        };
        let popup = reader.link_prompt_area();
        let buttons_y = popup.y + popup.height.saturating_sub(2);
        let hovered = |start: u16, length: usize| {
            self.hover.is_some_and(|hover| {
                hover.y == buttons_y
                    && hover.x >= start
                    && hover.x < start + u16::try_from(length).unwrap_or(0)
            })
        };
        let open_x = popup.x + 1;
        let cancel_x = open_x + u16::try_from(LINK_OPEN_LABEL.len() + 2).unwrap_or(0);
        let style_for = |active: bool| {
            if active {
                self.palette.hover()
            } else {
                Style::new()
            }
        };
        let text = Text::from(vec![
            UiLine::from("Open this address in your browser?"),
            UiLine::from(""),
            UiLine::from(prompt.url.clone()).style(self.palette.title()),
            UiLine::from(""),
            UiLine::from(vec![
                Span::styled(
                    LINK_OPEN_LABEL,
                    style_for(hovered(open_x, LINK_OPEN_LABEL.len())),
                ),
                Span::raw("  "),
                Span::styled(
                    LINK_CANCEL_LABEL,
                    style_for(hovered(cancel_x, LINK_CANCEL_LABEL.len())),
                ),
                Span::raw("  (Enter / Esc)"),
            ]),
        ]);
        let widget = Paragraph::new(text).wrap(Wrap { trim: false }).block(
            TuiBlock::default()
                .borders(Borders::ALL)
                .title(" External link "),
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(widget, popup);
    }

    fn draw_reader_toc(&mut self, frame: &mut Frame, reader: &mut ReaderScreen) {
        let area = reader.toc_area();
        let entries = reader.filtered_toc();
        let rows_count = reader.toc_visible_rows();
        let current = reader.chapter_index;
        let (current_mark, read_mark) = if self.config.reading.ascii_only {
            ('o', 'x')
        } else {
            ('●', '✓')
        };
        let blink = self.config.theme.caret_blink;
        let content_width = area.width.saturating_sub(2);
        let Some(toc) = &mut reader.toc else { return };
        let mut rows = vec![toc.filter.render_caret("Search: ", blink)];
        let mut selected_row = None;
        for (index, (spine_index, label, depth)) in
            entries.iter().enumerate().skip(toc.top).take(rows_count)
        {
            let state = match spine_index.cmp(&current) {
                std::cmp::Ordering::Equal => current_mark,
                std::cmp::Ordering::Less => read_mark,
                std::cmp::Ordering::Greater => ' ',
            };
            let indent = "  ".repeat((*depth).min(6));
            let row = format!("{state} {:>4}  {indent}{label}", spine_index + 1);
            if index == toc.selection {
                selected_row = Some(rows.len());
                rows.push(pad_row(&row, content_width));
            } else {
                rows.push(row);
            }
            self.hit_targets.push((
                Rect::new(
                    area.x + 1,
                    area.y + 2 + u16::try_from(index - toc.top).unwrap_or(0),
                    area.width.saturating_sub(2),
                    1,
                ),
                Action::TocSelect(index),
            ));
        }
        let top = toc.top;
        let mut text = self.styled_rows(area, rows);
        if let Some(row) = selected_row {
            if let Some(line) = text.lines.get_mut(row) {
                line.style = self.palette.selection();
            }
        }
        let popup = Paragraph::new(text)
            .block(TuiBlock::default().borders(Borders::ALL).title(format!(
                " Chapters ({}/{}) — type to filter, Esc to close ",
                entries.len(),
                reader.book.spine.len()
            )))
            .style(Style::default().add_modifier(Modifier::BOLD));
        frame.render_widget(Clear, area);
        frame.render_widget(popup, area);
        self.render_popup_scrollbar(frame, area, entries.len(), rows_count, top);
    }

    /// Byte range of the registered target under the mouse on row `y`,
    /// relative to text starting at column `x_origin`.
    fn hovered_range(&self, y: u16, x_origin: u16) -> Option<(usize, usize)> {
        let hover = self.hover?;
        if hover.y != y {
            return None;
        }
        let (rect, _) = self
            .hit_targets
            .iter()
            .find(|(rect, _)| rect.contains(hover))?;
        if rect.y != y {
            return None;
        }
        let start = usize::from(rect.x.saturating_sub(x_origin));
        Some((start, start + usize::from(rect.width)))
    }

    /// Highlight the hovered clickable segment of a rendered line.
    fn styled_line(&self, text: String, y: u16, x_origin: u16) -> UiLine<'static> {
        let Some((start, end)) = self.hovered_range(y, x_origin) else {
            return UiLine::from(text);
        };
        let end = end.min(text.len());
        if start >= end {
            return UiLine::from(text);
        }
        match (text.get(..start), text.get(start..end), text.get(end..)) {
            (Some(prefix), Some(target), Some(suffix)) => UiLine::from(vec![
                Span::raw(prefix.to_owned()),
                Span::styled(target.to_owned(), self.palette.hover()),
                Span::raw(suffix.to_owned()),
            ]),
            // Hover range not on a char boundary; skip the highlight.
            _ => UiLine::from(text),
        }
    }

    /// Convert content rows into text with hover highlighting applied.
    fn styled_rows(&self, area: Rect, rows: Vec<String>) -> Text<'static> {
        let lines: Vec<UiLine<'static>> = rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let y = area.y + 1 + u16::try_from(index).unwrap_or(u16::MAX);
                self.styled_line(row, y, area.x + 1)
            })
            .collect();
        Text::from(lines)
    }

    /// Bottom-title footer with hover highlighting.
    fn styled_footer(&self, area: Rect, footer: &str) -> UiLine<'static> {
        let y = area.y + area.height.saturating_sub(1);
        self.styled_line(footer.to_owned(), y, area.x + 1)
    }

    fn register_footer(&mut self, area: Rect, footer: &str, actions: &[(&str, Action)]) {
        let row = area.y + area.height.saturating_sub(1);
        for (label, action) in actions {
            if let Some(index) = footer.find(label) {
                self.hit_targets.push((
                    Rect::new(
                        u16::try_from(index + 1).unwrap_or(0),
                        row,
                        u16::try_from(label.len()).unwrap_or(0),
                        1,
                    ),
                    *action,
                ));
            }
        }
    }

    /// Screen row (0-based content row inside `area`'s border) → y coordinate,
    /// or `None` when the row is scrolled away or clipped by the border.
    fn content_row_y(&self, area: Rect, row_index: usize) -> Option<u16> {
        let visible_index = row_index.checked_sub(self.scroll_offset)?;
        let y = area.y + 1 + u16::try_from(visible_index).ok()?;
        (y < area.y + area.height.saturating_sub(1)).then_some(y)
    }

    /// Make an entire content row clickable.
    fn register_row(&mut self, area: Rect, row_index: usize, action: Action) {
        if let Some(y) = self.content_row_y(area, row_index) {
            self.hit_targets.push((
                Rect::new(area.x + 1, y, area.width.saturating_sub(2), 1),
                action,
            ));
        }
    }

    /// Make each label (and the text following it, up to the next label)
    /// clickable. Labels must appear before any non-ASCII text in the row.
    fn register_row_labels(
        &mut self,
        area: Rect,
        row_index: usize,
        text: &str,
        labels: &[(&str, Action)],
    ) {
        let Some(y) = self.content_row_y(area, row_index) else {
            return;
        };
        let mut starts: Vec<(usize, Action)> = labels
            .iter()
            .filter_map(|(label, action)| text.find(label).map(|index| (index, *action)))
            .collect();
        starts.sort_by_key(|(index, _)| *index);
        for (position, (start, action)) in starts.iter().enumerate() {
            let end = starts
                .get(position + 1)
                .map_or(text.len(), |(next, _)| *next);
            self.hit_targets.push((
                Rect::new(
                    area.x + 1 + u16::try_from(*start).unwrap_or(0),
                    y,
                    u16::try_from(end - start).unwrap_or(0),
                    1,
                ),
                *action,
            ));
        }
    }

    /// Make the value area of a rendered text input clickable for cursor moves.
    fn register_input_row(&mut self, area: Rect, row_index: usize, label: &str) {
        let Some(y) = self.content_row_y(area, row_index) else {
            return;
        };
        let label_width = u16::try_from(label.chars().count()).unwrap_or(0);
        let x = area.x + 1 + label_width;
        let width = (area.x + area.width.saturating_sub(1)).saturating_sub(x);
        self.hit_targets
            .push((Rect::new(x, y, width, 1), Action::InputClick));
    }

    /// The home screen's selectable items in display order.
    fn home_items(&self) -> Vec<HomeItem> {
        home_items(
            self.recents
                .most_recent()
                .is_some_and(|recent| recent.path.exists()),
            self.recents.list().len(),
            self.config.library.book_dirs.len(),
        )
    }

    fn home_item_count(&self) -> usize {
        self.home_items().len()
    }

    fn activate_home_item(&mut self, index: usize) {
        let items = self.home_items();
        match items.get(index) {
            Some(HomeItem::Continue) => {
                let path = self.recents.most_recent().map(|recent| recent.path.clone());
                if let Some(path) = path {
                    self.open_book_or_status(path);
                }
            }
            Some(HomeItem::Recent(list_index)) => {
                let Some(recent) = self.recents.list().get(*list_index) else {
                    return;
                };
                let path = recent.path.clone();
                if path.exists() {
                    self.open_book_or_status(path);
                } else {
                    self.status = Some(
                        "Book file is missing — press Del to remove it from Recent.".to_owned(),
                    );
                }
            }
            Some(HomeItem::AllBooks) => self.open_aggregated_library(),
            Some(HomeItem::Library(dir_index)) => {
                if let Some(directory) = self.config.library.book_dirs.get(*dir_index) {
                    let directory = directory.clone();
                    self.start_library_scan(directory.display().to_string(), vec![directory]);
                }
            }
            Some(HomeItem::AddLibrary) | None => {
                self.next_screen = Some(Screen::Settings(SettingsScreen {
                    mode: SettingsMode::AddingLibrary,
                    ..SettingsScreen::default()
                }));
            }
        }
    }

    /// One merged, deduplicated, title-sorted list across all book dirs.
    fn open_aggregated_library(&mut self) {
        let directories = self.config.library.book_dirs.clone();
        let title = format!("All books ({} libraries)", directories.len());
        self.start_library_scan(title, directories);
    }

    /// Scan on a worker thread so a cold scan of a large library does not
    /// freeze the UI; the result arrives via `process_scan_events`.
    fn start_library_scan(&mut self, title: String, directories: Vec<PathBuf>) {
        if self.scanner.busy {
            self.status = Some("A library scan is already running…".to_owned());
            return;
        }
        self.status = Some("Scanning library…".to_owned());
        self.scanner
            .start(title, directories, self.scan_cache.clone());
    }

    /// Delete the selected recent entry (Continue reading counts as the
    /// newest one); library rows are left alone.
    fn remove_home_recent(&mut self, home: &mut HomeScreen) {
        let items = self.home_items();
        let list_index = match items.get(home.selection) {
            Some(HomeItem::Continue) => 0,
            Some(HomeItem::Recent(index)) => *index,
            _ => return,
        };
        let Some(recent) = self.recents.list().get(list_index) else {
            return;
        };
        let title = recent.title.clone();
        let path = recent.path.clone();
        match self.recents.remove(&path) {
            Ok(true) => {
                self.status = Some(format!("Removed \"{title}\" from Recent."));
                home.selection = home.selection.min(self.home_item_count().saturating_sub(1));
            }
            Ok(false) => {}
            Err(error) => self.status = Some(format!("Could not update recents: {error}")),
        }
    }

    fn filtered_books(library: &LibraryScreen) -> Vec<&LibraryBook> {
        let needle = library.filter.value().to_lowercase();
        library
            .books
            .iter()
            .filter(|book| {
                needle.is_empty()
                    || book.metadata.title.to_lowercase().contains(&needle)
                    || book
                        .metadata
                        .authors
                        .join(" ")
                        .to_lowercase()
                        .contains(&needle)
            })
            .collect()
    }

    fn open_book(&mut self, path: PathBuf) -> Result<()> {
        self.pending_status = None;
        self.session_summary_visible = false;
        let book =
            EpubBook::open(&path).with_context(|| format!("could not open {}", path.display()))?;
        let position = self.positions.get(&path);
        let recent = RecentBook {
            path: path.clone(),
            title: book.metadata.title.clone(),
            authors: book.metadata.authors.join(", "),
            spine_count: book.spine.len(),
            last_chapter: position.chapter_index,
            last_opened: 0,
        };
        self.recents.touch(recent)?;
        let mut reader = ReaderScreen::new(path, book, &position);
        let excluded = self.config.sync.excluded_books.contains(&reader.path);
        if excluded {
            self.status = Some("Sync is disabled for this book — press x to enable.".to_owned());
        } else if self.sync.logged_in() {
            match sync::digest_for(&reader.path, self.config.sync.matching) {
                Ok(document) => {
                    reader.document_digest = Some(document.clone());
                    if self.config.sync.auto_sync {
                        self.sync.pull(&self.config.sync, document, false);
                    }
                }
                Err(error) => logging::warn(&format!("could not hash document: {error}")),
            }
        }
        self.next_screen = Some(Screen::Reader(Box::new(reader)));
        Ok(())
    }

    /// Open a book, surfacing failures (corrupt EPUBs, I/O errors) in the
    /// status line instead of silently doing nothing.
    fn open_book_or_status(&mut self, path: PathBuf) {
        if let Err(error) = self.open_book(path) {
            self.status = Some(format!("{error:#}"));
        }
    }

    fn leave_reader(&mut self, reader: &mut ReaderScreen) {
        if self.config.sync.auto_sync {
            Self::push_progress(&self.config, &mut self.sync, reader, false);
        }
        self.session_summary_visible =
            Self::record_session(&mut self.stats, &mut self.status, reader);
        let position = reader.position();
        let recent = RecentBook {
            path: reader.path.clone(),
            title: reader.book.metadata.title.clone(),
            authors: reader.book.metadata.authors.join(", "),
            spine_count: reader.book.spine.len(),
            last_chapter: reader.chapter_index,
            last_opened: 0,
        };
        let _ = self.positions.save_position(reader.path.clone(), position);
        let _ = self.recents.touch(recent);
        self.next_screen = Some(Screen::Home(HomeScreen::default()));
    }

    /// Combined app/sync status for footers.
    fn status_line(&self) -> Option<String> {
        self.status.clone().or_else(|| self.sync.status.clone())
    }

    /// Clear the footer status a few seconds after it first appeared.
    fn expire_status(&mut self) {
        let Some(current) = self.status_line() else {
            self.status_seen = None;
            return;
        };
        match &self.status_seen {
            Some((seen, since)) if *seen == current => {
                if since.elapsed() >= STATUS_TTL {
                    self.status = None;
                    self.sync.status = None;
                    self.status_seen = None;
                    if self.session_summary_visible {
                        self.session_summary_visible = false;
                        self.status = self.pending_status.take();
                    }
                }
            }
            _ => self.status_seen = Some((current, Instant::now())),
        }
    }

    /// Queue a progress push for the reader's current position.
    fn push_progress(
        config: &Config,
        sync: &mut SyncController,
        reader: &mut ReaderScreen,
        manual: bool,
    ) {
        let Some(document) = reader.document_digest.clone() else {
            if manual {
                sync.status = Some("Not signed in.".to_owned());
            }
            return;
        };
        let (progress, percentage) = reader.progress_payload();
        reader.last_push = Instant::now();
        reader.last_pushed_progress = Some(progress.clone());
        let update = ProgressUpdate {
            document,
            metadata: None,
            progress,
            percentage,
            device: config
                .sync
                .device_name
                .clone()
                .unwrap_or_else(|| "terminalreader".to_owned()),
            device_id: config.sync.device_id.clone().unwrap_or_default(),
        };
        sync.push(&config.sync, update, manual);
    }

    /// Count a page turn and push when the configured interval is reached.
    fn note_page_turn(config: &Config, sync: &mut SyncController, reader: &mut ReaderScreen) {
        reader.page_turns += 1;
        reader.session_pages += 1;
        if let Some(pages) = config.sync.pages_before_update {
            if pages > 0 && reader.page_turns >= pages {
                reader.page_turns = 0;
                Self::push_progress(config, sync, reader, false);
            }
        }
    }

    /// Add the finished session to the book's reading statistics.
    fn record_session(
        stats: &mut StatsStore,
        footer_status: &mut Option<String>,
        reader: &mut ReaderScreen,
    ) -> bool {
        let seconds = reader.session_start.elapsed().as_secs();
        let pages = std::mem::take(&mut reader.session_pages);
        reader.session_start = Instant::now();
        if pages == 0 && seconds < 30 {
            return false;
        }
        if let Err(error) = stats.record(&reader.path, seconds, pages) {
            logging::warn(&format!("could not save reading stats: {error}"));
            return false;
        }
        *footer_status = Some(format!(
            "Read {} this session ({pages} pages).",
            format_duration(seconds)
        ));
        true
    }

    /// Push on a timer so progress isn't lost when the app never exits
    /// cleanly (laptop lid closed, terminal killed, …).
    fn maybe_timed_push(&mut self) {
        let Some(minutes) = self.config.sync.minutes_before_update else {
            return;
        };
        let Screen::Reader(reader) = &mut self.screen else {
            return;
        };
        if minutes == 0
            || reader.document_digest.is_none()
            || reader.last_push.elapsed() < Duration::from_secs(u64::from(minutes) * 60)
        {
            return;
        }
        let (progress, _) = reader.progress_payload();
        if reader.last_pushed_progress.as_deref() == Some(progress.as_str()) {
            // Nothing new to report; restart the timer.
            reader.last_push = Instant::now();
            return;
        }
        Self::push_progress(&self.config, &mut self.sync, reader, false);
    }

    /// Handle a finished background library scan.
    fn process_scan_events(&mut self) {
        let Some(outcome) = self.scanner.poll() else {
            // Animate the scanning notice while the worker runs.
            if self.scanner.busy
                && self
                    .status
                    .as_deref()
                    .is_none_or(|status| status.starts_with("Scanning library"))
            {
                let frame = spinner_frame(
                    self.scanner.started.elapsed(),
                    self.config.reading.ascii_only,
                );
                self.status = Some(format!("Scanning library… {frame}"));
            }
            return;
        };
        self.scan_cache = outcome.cache;
        if let Err(error) = self.scan_cache.save() {
            logging::warn(&format!("could not save scan cache: {error}"));
        }
        // Only jump to the library if the user is still where they asked for it.
        if matches!(self.screen, Screen::Home(_)) {
            self.status = None;
            self.next_screen = Some(Screen::Library(LibraryScreen {
                title: outcome.title,
                books: outcome.books,
                filter: TextInput::default(),
                selection: 0,
                top: 0,
                sort: LibrarySort::default(),
            }));
            self.apply_screen_transition();
        } else {
            self.status = Some(format!(
                "Library scan finished ({} books).",
                outcome.books.len()
            ));
        }
    }

    /// Handle finished background update work.
    fn process_update_events(&mut self) {
        for event in self.update.poll() {
            let message = match event {
                UpdateEvent::Checked(Ok(status)) if status.available => format!(
                    "Update available: {} (current {}). Press i in Settings to install.",
                    status.latest, status.current
                ),
                UpdateEvent::Checked(Ok(status)) => {
                    format!("Up to date ({}).", status.current)
                }
                UpdateEvent::Checked(Err(error)) => format!("Update check failed: {error}"),
                UpdateEvent::Applied(Ok(tag)) => {
                    format!("Updated to {tag}. Restart TerminalReader to use it.")
                }
                UpdateEvent::Applied(Err(error)) => format!("Update failed: {error}"),
            };
            logging::info(&message);
            self.status = Some(message.clone());
            if let Screen::Settings(settings) = &mut self.screen {
                settings.message = Some(message);
            }
        }
    }

    /// Handle finished background sync work (auth outcomes, pull results).
    fn process_sync_events(&mut self) {
        let events = self.sync.poll(&self.config.sync);
        if let Some(notice) = self.sync.take_push_notice() {
            if notice.success && self.session_summary_visible {
                self.pending_status = Some(notice.message);
                self.sync.status = None;
            } else {
                self.status = Some(notice.message);
                if !notice.success {
                    self.session_summary_visible = false;
                    self.pending_status = None;
                }
            }
        }
        if events.is_empty() {
            return;
        }
        let mut screen = std::mem::replace(&mut self.screen, Screen::Home(HomeScreen::default()));
        for event in events {
            match event {
                SyncEvent::Auth {
                    username,
                    userkey,
                    result,
                    registered,
                } => self.finish_auth(&mut screen, username, userkey, result, registered),
                SyncEvent::Pull {
                    document,
                    result,
                    manual,
                } => {
                    if let Screen::Reader(reader) = &mut screen {
                        if reader.document_digest.as_deref() == Some(document.as_str()) {
                            match result {
                                Ok(Some(record)) => self.apply_pull(reader, &record, manual),
                                Ok(None) => {
                                    if manual {
                                        self.sync.status = Some(
                                            "Server has no position for this book yet.".to_owned(),
                                        );
                                    }
                                }
                                Err(error) => {
                                    logging::warn(&format!("sync pull failed: {error}"));
                                    self.sync.status = Some(format!("Pull failed: {error}"));
                                }
                            }
                        }
                    }
                }
                SyncEvent::Push { .. } => {}
            }
        }
        self.screen = screen;
    }

    fn finish_auth(
        &mut self,
        screen: &mut Screen,
        username: String,
        userkey: String,
        result: Result<(), String>,
        registered: bool,
    ) {
        let message = match result {
            Ok(()) => {
                self.config.sync.username = Some(username.clone());
                if self.config.sync.device_id.is_none() {
                    self.config.sync.device_id = Some(sync::generate_device_id());
                }
                if self.config.sync.device_name.is_none() {
                    self.config.sync.device_name = Some("terminalreader".to_owned());
                }
                if let Err(error) = self.config.save() {
                    logging::warn(&format!("could not save config: {error}"));
                }
                let message = match credentials::store_userkey(
                    &self.config.sync.server_url,
                    &username,
                    &userkey,
                ) {
                    Ok(()) if registered => "Registered and signed in.".to_owned(),
                    Ok(()) => "Signed in.".to_owned(),
                    Err(error) => {
                        logging::warn(&format!("keyring store failed: {error}"));
                        format!("Signed in, but storing credentials failed: {error}")
                    }
                };
                self.sync
                    .set_credentials(Some(Credentials { username, userkey }));
                self.sync.drain_next(&self.config.sync);
                logging::info("sync sign-in succeeded");
                message
            }
            Err(error) => {
                logging::warn(&format!("sync sign-in failed: {error}"));
                format!("Sign-in failed: {error}")
            }
        };
        self.sync.status = Some(message.clone());
        if let Screen::Settings(settings) = screen {
            settings.message = Some(message);
        }
    }

    /// Decide what to do with a pulled server position.
    fn apply_pull(&mut self, reader: &mut ReaderScreen, record: &ProgressRecord, manual: bool) {
        let Some(remote_percent) = record.percentage else {
            if manual {
                self.sync.status = Some("Server has no position for this book.".to_owned());
            }
            return;
        };
        let local_percent = reader.percentage();
        let forward = remote_percent > local_percent + 1e-6;
        let backward = remote_percent < local_percent - 1e-6;
        if !forward && !backward {
            if manual {
                self.sync.status = Some("Already in sync.".to_owned());
            }
            return;
        }
        let Some(position) = reader.position_from_record(record) else {
            self.sync.status = Some("Could not map the synced position.".to_owned());
            return;
        };
        // Manual pulls always show the prompt so `p` never jumps unasked.
        let strategy = if manual {
            SyncStrategy::Prompt
        } else if forward {
            self.config.sync.sync_forward
        } else {
            self.config.sync.sync_backward
        };
        match strategy {
            SyncStrategy::Disable => {}
            SyncStrategy::Silent => {
                reader.apply_position(&position);
                self.sync.status = Some("Position synced from server.".to_owned());
            }
            SyncStrategy::Prompt => {
                reader.sync_prompt = Some(SyncPrompt {
                    position,
                    remote_percent,
                    local_percent,
                    device: record.device.clone(),
                });
            }
        }
    }

    fn apply_screen_transition(&mut self) {
        if let Some(screen) = self.next_screen.take() {
            self.screen = screen;
        }
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// One selectable row on the home screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeItem {
    Continue,
    Recent(usize),
    AllBooks,
    Library(usize),
    AddLibrary,
}

/// Single source of truth for home-screen item order; drawing, activation,
/// and deletion all derive their indices from this list.
fn home_items(has_continue: bool, recent_count: usize, library_count: usize) -> Vec<HomeItem> {
    let mut items = Vec::new();
    if has_continue {
        items.push(HomeItem::Continue);
    }
    items.extend((0..recent_count).map(HomeItem::Recent));
    if library_count > 1 {
        items.push(HomeItem::AllBooks);
    }
    items.extend((0..library_count).map(HomeItem::Library));
    items.push(HomeItem::AddLibrary);
    items
}

/// A finished background library scan.
#[derive(Debug)]
struct ScanOutcome {
    title: String,
    books: Vec<LibraryBook>,
    cache: ScanCache,
}

/// Background library-scan worker, mirroring `UpdateController`.
#[derive(Debug)]
struct LibraryScanner {
    tx: Sender<ScanOutcome>,
    rx: Receiver<ScanOutcome>,
    busy: bool,
    /// When the current scan started, for the spinner.
    started: Instant,
}

impl LibraryScanner {
    fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            busy: false,
            started: Instant::now(),
        }
    }

    /// Scan `directories` on a worker thread with a copy of the cache; the
    /// updated cache comes back with the result.
    fn start(&mut self, title: String, directories: Vec<PathBuf>, mut cache: ScanCache) {
        self.busy = true;
        self.started = Instant::now();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut books = Vec::new();
            for directory in &directories {
                books.extend(scan_library_cached(directory, &mut cache));
            }
            if directories.len() > 1 {
                books.sort_by(|left, right| left.path.cmp(&right.path));
                books.dedup_by(|left, right| left.path == right.path);
                books.sort_by(|left, right| left.metadata.title.cmp(&right.metadata.title));
            }
            let _ = tx.send(ScanOutcome {
                title,
                books,
                cache,
            });
        });
    }

    fn poll(&mut self) -> Option<ScanOutcome> {
        let outcome = self.rx.try_recv().ok();
        if outcome.is_some() {
            self.busy = false;
        }
        outcome
    }
}

/// The reader's character keys with config overrides applied.
struct ReaderKeys {
    contents: char,
    search: char,
    next_match: char,
    previous_match: char,
    bookmark_add: char,
    bookmarks: char,
    sync_push: char,
    sync_pull: char,
    sync_toggle: char,
    quit: char,
}

fn reader_keys(keys: &tr_core::KeyBindings) -> ReaderKeys {
    ReaderKeys {
        contents: keys.contents.unwrap_or('t'),
        search: keys.search.unwrap_or('/'),
        next_match: keys.next_match.unwrap_or('n'),
        previous_match: keys.previous_match.unwrap_or('N'),
        bookmark_add: keys.bookmark_add.unwrap_or('m'),
        bookmarks: keys.bookmarks.unwrap_or('M'),
        sync_push: keys.sync_push.unwrap_or('s'),
        sync_pull: keys.sync_pull.unwrap_or('p'),
        sync_toggle: keys.sync_toggle.unwrap_or('x'),
        quit: keys.quit.unwrap_or('q'),
    }
}

/// Human-readable reading duration: "under a minute", "N min", "N h M min".
fn format_duration(seconds: u64) -> String {
    let minutes = seconds / 60;
    match minutes {
        0 => "under a minute".to_owned(),
        1..=59 => format!("{minutes} min"),
        _ => format!("{} h {} min", minutes / 60, minutes % 60),
    }
}

/// Pad with spaces to `width` columns so a row style spans the whole line.
fn pad_row(text: &str, width: u16) -> String {
    let width = usize::from(width);
    let used = UnicodeWidthStr::width(text);
    let mut padded = text.to_owned();
    padded.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    padded
}

/// Truncate to `width` columns (with an ellipsis when clipped) and pad to
/// exactly `width`, for column-aligned lists.
fn fit(text: &str, width: usize) -> String {
    let full = UnicodeWidthStr::width(text);
    if full <= width {
        let mut out = text.to_owned();
        out.extend(std::iter::repeat_n(' ', width - full));
        return out;
    }
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars() {
        let char_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + char_width > width.saturating_sub(1) {
            break;
        }
        used += char_width;
        out.push(character);
    }
    out.push('…');
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used + 1)));
    out
}

/// A thin progress bar filling `fraction` of `width` cells.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn progress_gauge(fraction: f64, width: usize, ascii_only: bool) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    let (full, empty) = if ascii_only {
        ('=', '-')
    } else {
        ('━', '╌')
    };
    let mut gauge = String::new();
    gauge.extend(std::iter::repeat_n(full, filled));
    gauge.extend(std::iter::repeat_n(empty, width - filled));
    gauge
}

/// Animation frame for background work, driven by the 50 ms event tick.
fn spinner_frame(elapsed: Duration, ascii_only: bool) -> char {
    const BRAILLE: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    const ASCII: [char; 4] = ['|', '/', '-', '\\'];
    let frames: &[char] = if ascii_only { &ASCII } else { &BRAILLE };
    let index = usize::try_from(elapsed.as_millis() / 120).unwrap_or(0) % frames.len();
    frames.get(index).copied().unwrap_or(' ')
}

/// Rough remaining reading time from total time spent and fraction done.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn estimate_time_left(seconds: u64, percent: f64) -> Option<u64> {
    (percent > 0.01 && seconds >= 300).then(|| (seconds as f64 * (1.0 - percent) / percent) as u64)
}

/// Progress cell for a library row: the exact saved percent when available,
/// a chapter fraction as fallback, or a finished marker.
#[allow(clippy::cast_precision_loss)]
fn book_progress_cell(position: &SavedPosition, spine_count: usize, ascii_only: bool) -> String {
    let chapter_fraction = if spine_count > 0 {
        position.chapter_index as f64 / spine_count as f64
    } else {
        0.0
    };
    let percent = position.percent.max(chapter_fraction);
    if percent >= FINISHED_PERCENT {
        (if ascii_only { "done" } else { "✓ done" }).to_owned()
    } else if percent > 0.0 {
        format!("{:.0}%", percent * 100.0)
    } else {
        String::new()
    }
}

/// Split `text` into styled spans, patching `base` with every layer that
/// covers a segment. Layer bounds must be char boundaries of `text`.
fn styled_segments(
    text: &str,
    base: Style,
    layers: &[(usize, usize, Style)],
) -> Vec<Span<'static>> {
    if layers.is_empty() {
        return vec![Span::styled(text.to_owned(), base)];
    }
    let mut cuts: Vec<usize> = layers
        .iter()
        .flat_map(|&(start, end, _)| [start, end])
        .filter(|&cut| cut <= text.len())
        .collect();
    cuts.push(0);
    cuts.push(text.len());
    cuts.sort_unstable();
    cuts.dedup();
    let mut spans = Vec::new();
    for (&start, &end) in cuts.iter().zip(cuts.iter().skip(1)) {
        let Some(segment) = text.get(start..end) else {
            continue;
        };
        if segment.is_empty() {
            continue;
        }
        let mut style = base;
        for &(layer_start, layer_end, layer) in layers {
            if layer_start <= start && end <= layer_end {
                style = style.patch(layer);
            }
        }
        spans.push(Span::styled(segment.to_owned(), style));
    }
    spans
}

/// Byte ranges of case-insensitive occurrences of `query` in `text`.
///
/// Compares char-by-char so offsets stay valid even when lowercasing
/// changes byte lengths.
fn find_matches(text: &str, query: &str) -> Vec<(usize, usize)> {
    let query_chars: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if query_chars.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (offset, _) in text.char_indices() {
        if let Some(length) = match_length(text.get(offset..).unwrap_or_default(), &query_chars) {
            found.push((offset, offset + length));
        }
    }
    found
}

/// Bytes at the start of `haystack` whose lowercased chars spell out
/// `query_chars`, or `None` when they do not.
fn match_length(haystack: &str, query_chars: &[char]) -> Option<usize> {
    let mut expected = query_chars.iter();
    for (index, character) in haystack.char_indices() {
        if expected.len() == 0 {
            return Some(index);
        }
        for lower in character.to_lowercase() {
            match expected.next() {
                Some(&want) if want == lower => {}
                _ => return None,
            }
        }
    }
    (expected.len() == 0).then_some(haystack.len())
}

/// Short context around a match for the search results list.
fn snippet_around(text: &str, start: usize, end: usize) -> String {
    const BEFORE: usize = 12;
    const AFTER: usize = 34;
    let mut prefix_start = start;
    for _ in 0..BEFORE {
        let Some(previous) = text
            .get(..prefix_start)
            .and_then(|head| head.char_indices().next_back().map(|(index, _)| index))
        else {
            break;
        };
        prefix_start = previous;
    }
    let mut suffix_end = end;
    for _ in 0..AFTER {
        match text.get(suffix_end..).and_then(|tail| tail.chars().next()) {
            Some(character) => suffix_end += character.len_utf8(),
            None => break,
        }
    }
    let head = if prefix_start > 0 { "…" } else { "" };
    let tail = if suffix_end < text.len() { "…" } else { "" };
    format!(
        "{head}{}{tail}",
        text.get(prefix_start..suffix_end).unwrap_or_default()
    )
}

fn floor_char_boundary(text: &str, mut byte: usize) -> usize {
    byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn previous_char_boundary(text: &str, byte: usize) -> usize {
    text.get(..floor_char_boundary(text, byte))
        .and_then(|head| head.char_indices().next_back().map(|(index, _)| index))
        .unwrap_or(0)
}

fn next_char_boundary(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text.get(byte..)
        .and_then(|tail| tail.chars().next())
        .map_or(text.len(), |character| byte + character.len_utf8())
}

fn byte_at_display_column(text: &str, column: usize) -> usize {
    let mut width = 0;
    for (byte, character) in text.char_indices() {
        let next = width + UnicodeWidthChar::width(character).unwrap_or(0);
        if column < next {
            return byte;
        }
        width = next;
    }
    text.len()
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let lookup = |index: usize| char::from(TABLE.get(index).copied().unwrap_or(b'='));
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk.first().copied().unwrap_or(0);
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(lookup(usize::from(first >> 2)));
        encoded.push(lookup(usize::from((first & 0x03) << 4 | second >> 4)));
        if chunk.len() > 1 {
            encoded.push(lookup(usize::from((second & 0x0f) << 2 | third >> 6)));
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(lookup(usize::from(third & 0x3f)));
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn copy_osc52(text: &str) -> String {
    let encoded = base64_encode(text.as_bytes());
    let result = (|| -> std::io::Result<()> {
        let mut stdout = std::io::stdout();
        write!(stdout, "\x1b]52;c;{encoded}\x07")?;
        stdout.flush()
    })();
    match result {
        Ok(()) => format!("Copied {} characters.", text.chars().count()),
        Err(error) => format!("Could not copy selection: {error}"),
    }
}

/// Extract the clicked image to a temp file and open it with the OS default
/// handler for its file type. Returns a status message.
fn open_reader_image(reader: &mut ReaderScreen, block: usize) -> (String, Option<PathBuf>) {
    let href = match reader.blocks.get(block) {
        Some(tr_epub::Block::Image {
            href: Some(href), ..
        }) => href.clone(),
        _ => return ("Image has no source to open.".to_owned(), None),
    };
    let (archive_path, bytes) = match reader.book.resource_bytes(reader.chapter_index, &href) {
        Ok(resource) => resource,
        Err(error) => return (format!("Could not read image: {error}"), None),
    };
    let name = hashed_image_name(archive_path.rsplit('/').next().unwrap_or("image"), &bytes);
    let directory = env::temp_dir().join("terminalreader");
    if let Err(error) = fs::create_dir_all(&directory) {
        return (format!("Could not create temp folder: {error}"), None);
    }
    let target = directory.join(&name);
    if !target.exists() {
        if let Err(error) = fs::write(&target, bytes) {
            return (format!("Could not write image: {error}"), None);
        }
    }
    match open_with_system_viewer(&target) {
        Ok(()) => (format!("Opened {name} in the system viewer."), Some(target)),
        Err(error) => (
            format!("Could not open the system viewer: {error}"),
            Some(target),
        ),
    }
}

fn hashed_image_name(name: &str, bytes: &[u8]) -> String {
    let sanitized = sanitize_file_name(name);
    let digest = hex::encode(Sha256::digest(bytes));
    let suffix = digest.get(..8).unwrap_or(&digest);
    if let Some((stem, extension)) = sanitized.rsplit_once('.') {
        format!("{stem}-{suffix}.{extension}")
    } else {
        format!("{sanitized}-{suffix}")
    }
}

fn cleanup_stale_temp_images(max_age: Duration) {
    let directory = env::temp_dir().join("terminalreader");
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        let should_remove = entry.file_type().is_ok_and(|kind| kind.is_file())
            && entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= max_age);
        if should_remove {
            if let Err(error) = fs::remove_file(entry.path()) {
                logging::debug(&format!(
                    "could not remove stale temp image {}: {error}",
                    entry.path().display()
                ));
            }
        }
    }
}

/// Keep only safe filename characters, preserving the extension so the OS
/// picks the right viewer.
fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
        .collect();
    if cleaned.trim_matches('.').is_empty() {
        "image.bin".to_owned()
    } else {
        cleaned
    }
}

/// Open a file with the platform's default application, detached.
fn open_with_system_viewer(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(path)
        .spawn()
        .map(|_| ())
}

fn is_external_link(href: &str) -> bool {
    url::Url::parse(href).is_ok_and(|url| matches!(url.scheme(), "http" | "https" | "mailto"))
}

fn open_external_url(url: &str) -> String {
    if !is_external_link(url) {
        return "Blocked an unsupported link scheme.".to_owned();
    }
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let program = "xdg-open";
    match std::process::Command::new(program).arg(url).spawn() {
        Ok(_) => "Opened link in the system browser.".to_owned(),
        Err(error) => format!("Could not open the system browser: {error}"),
    }
}

/// One Recent list row: title — authors — ch. x/y — 2 days ago (missing).
fn recent_row(recent: &RecentBook, finished: bool) -> String {
    let authors = if recent.authors.is_empty() {
        String::new()
    } else {
        format!(" — {}", recent.authors)
    };
    let opened = if recent.last_opened > 0 {
        format!(" — {}", relative_time(recent.last_opened))
    } else {
        String::new()
    };
    let done = if finished { " — finished ✓" } else { "" };
    let missing = if recent.path.exists() {
        ""
    } else {
        " (missing)"
    };
    format!(
        "{}{authors} — ch. {}/{}{opened}{done}{missing}",
        recent.title,
        recent.last_chapter + 1,
        recent.spine_count
    )
}

fn relative_time(then: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let delta = now.saturating_sub(then);
    match delta {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{} min ago", delta / 60),
        3_600..=86_399 => format!("{} h ago", delta / 3_600),
        86_400..=172_799 => "1 day ago".to_owned(),
        _ => format!("{} days ago", delta / 86_400),
    }
}

fn matching_label(matching: tr_core::MatchingMethod) -> &'static str {
    match matching {
        tr_core::MatchingMethod::Binary => "binary (identical file)",
        tr_core::MatchingMethod::Filename => "filename (same file name)",
    }
}

fn default_library_suggestion() -> String {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let Some(home) = home else {
        return String::new();
    };
    let books = home.join("Books");
    if books.is_dir() {
        books.display().to_string()
    } else {
        home.display().to_string()
    }
}

impl ReaderScreen {
    fn new(path: PathBuf, book: EpubBook, position: &SavedPosition) -> Self {
        Self {
            path,
            book,
            chapter_index: position.chapter_index,
            top_line: 0,
            anchor: (position.block_index, position.char_offset),
            blocks: Vec::new(),
            source_paths: Vec::new(),
            blocks_loaded: false,
            lines: Vec::new(),
            selection: None,
            width: 0,
            height: 0,
            toc: None,
            document_digest: None,
            page_turns: 0,
            last_push: Instant::now(),
            last_pushed_progress: None,
            sync_prompt: None,
            link_prompt: None,
            open_at_end: false,
            search: None,
            search_matches: Vec::new(),
            search_index: 0,
            search_query: None,
            search_snippets: Vec::new(),
            results_open: None,
            goto_input: None,
            footnote: None,
            stats_open: false,
            zen: false,
            inline: Vec::new(),
            ids: Vec::new(),
            bookmarks_open: None,
            session_start: Instant::now(),
            session_pages: 0,
        }
    }

    fn ensure_layout(&mut self, width: u16, height: u16, options: LayoutOptions) {
        self.height = height;
        if self.width == width && !self.lines.is_empty() {
            return;
        }
        self.width = width;
        if !self.blocks_loaded {
            self.blocks_loaded = true;
            let sourced = self
                .book
                .chapter_blocks(self.chapter_index)
                .unwrap_or_default();
            self.blocks = Vec::with_capacity(sourced.len());
            self.source_paths = Vec::with_capacity(sourced.len());
            self.inline = Vec::with_capacity(sourced.len());
            self.ids = Vec::with_capacity(sourced.len());
            for block in sourced {
                self.blocks.push(block.block);
                self.source_paths.push(block.source_path);
                self.inline.push(block.inline);
                self.ids.push(block.ids);
            }
        }
        if self.blocks.is_empty() {
            self.lines = vec![Line {
                text: "Unable to render this chapter.".to_owned(),
                block: 0,
                char_offset: 0,
                atomic: false,
            }];
        } else {
            self.lines = layout_with(&self.blocks, width, options);
        }
        if self.open_at_end {
            self.open_at_end = false;
            let step = self.content_height();
            self.top_line = self.lines.len().saturating_sub(1) / step * step;
            self.update_anchor();
        } else {
            self.top_line = line_for_anchor(&self.lines, self.anchor.0, self.anchor.1);
            self.clamp_top();
        }
    }

    fn invalidate_layout(&mut self) {
        self.width = 0;
        self.lines.clear();
        self.selection = None;
    }
    /// Invalidate layout *and* the loaded chapter blocks.
    fn invalidate_chapter(&mut self) {
        self.blocks.clear();
        self.source_paths.clear();
        self.inline.clear();
        self.ids.clear();
        self.blocks_loaded = false;
        self.open_at_end = false;
        self.invalidate_layout();
    }
    fn content_height(&self) -> usize {
        let chrome = if self.zen { 0 } else { 2 };
        usize::from(self.height.saturating_sub(chrome)).max(1)
    }
    fn content_height_isize(&self) -> isize {
        isize::try_from(self.content_height()).unwrap_or(isize::MAX)
    }
    fn start_selection(&mut self) {
        let point = TextPoint {
            line: self.top_line.min(self.lines.len().saturating_sub(1)),
            byte: 0,
        };
        self.selection = Some(TextSelection {
            anchor: point,
            head: point,
            dragging: false,
        });
    }
    fn move_selection(&mut self, columns: isize, rows: isize) {
        let Some(mut selection) = self.selection else {
            return;
        };
        if self.lines.is_empty() {
            return;
        }
        let last = self.lines.len().saturating_sub(1);
        let line = if rows < 0 {
            selection.head.line.saturating_sub(rows.unsigned_abs())
        } else {
            selection
                .head
                .line
                .saturating_add(rows.unsigned_abs())
                .min(last)
        };
        let Some(text) = self.lines.get(line).map(|line| line.text.as_str()) else {
            return;
        };
        let mut byte = floor_char_boundary(text, selection.head.byte.min(text.len()));
        if columns < 0 {
            for _ in 0..columns.unsigned_abs() {
                byte = previous_char_boundary(text, byte);
            }
        } else {
            for _ in 0..columns.unsigned_abs() {
                byte = next_char_boundary(text, byte);
            }
        }
        if rows != 0 {
            byte = floor_char_boundary(text, selection.head.byte.min(text.len()));
        }
        selection.head = TextPoint { line, byte };
        self.selection = Some(selection);
        if line < self.top_line {
            self.top_line = line;
        } else if line >= self.top_line + self.content_height() {
            self.top_line = line + 1 - self.content_height();
        }
        self.update_anchor();
    }
    fn handle_selection_mouse(&mut self, mouse: MouseEvent) {
        let inset = u16::from(!self.zen);
        let x = mouse.column.saturating_sub(inset);
        let y = mouse.row.saturating_sub(inset);
        if y >= u16::try_from(self.content_height()).unwrap_or(u16::MAX) {
            return;
        }
        let line_index = self.top_line + usize::from(y);
        let Some(line) = self.lines.get(line_index) else {
            return;
        };
        let point = TextPoint {
            line: line_index,
            byte: byte_at_display_column(&line.text, usize::from(x)),
        };
        let Some(selection) = &mut self.selection else {
            return;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                selection.anchor = point;
                selection.head = point;
                selection.dragging = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if selection.dragging => {
                selection.head = point;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                selection.head = point;
                selection.dragging = false;
            }
            _ => {}
        }
    }
    fn selection_range(&self, line_index: usize) -> Option<(usize, usize)> {
        let selection = self.selection?;
        let (start, end) = if selection.anchor <= selection.head {
            (selection.anchor, selection.head)
        } else {
            (selection.head, selection.anchor)
        };
        let line = self.lines.get(line_index)?;
        if line_index < start.line || line_index > end.line || start == end {
            return None;
        }
        let range_start = if line_index == start.line {
            start.byte
        } else {
            0
        };
        let range_end = if line_index == end.line {
            end.byte
        } else {
            line.text.len()
        };
        (range_start < range_end).then_some((range_start, range_end))
    }
    fn selected_text(&self) -> String {
        let Some(selection) = self.selection else {
            return String::new();
        };
        let (start, end) = if selection.anchor <= selection.head {
            (selection.anchor, selection.head)
        } else {
            (selection.head, selection.anchor)
        };
        if start == end {
            return String::new();
        }
        let mut selected = Vec::new();
        for line_index in start.line..=end.line {
            let Some(line) = self.lines.get(line_index) else {
                continue;
            };
            let from = if line_index == start.line {
                start.byte
            } else {
                0
            };
            let to = if line_index == end.line {
                end.byte
            } else {
                line.text.len()
            };
            let text = line.text.get(from..to).unwrap_or_default();
            selected.push(collapse_whitespace(text));
        }
        selected.join("\n")
    }
    fn clamp_top(&mut self) {
        self.top_line = self.top_line.min(self.lines.len().saturating_sub(1));
    }
    fn update_anchor(&mut self) {
        if let Some(line) = self
            .lines
            .get(self.top_line..)
            .and_then(|rest| rest.iter().find(|line| !line.is_separator()))
        {
            self.anchor = (line.block, line.char_offset);
        }
    }
    fn visible_lines(&self) -> &[Line] {
        self.lines
            .get(self.top_line..(self.top_line + self.content_height()).min(self.lines.len()))
            .unwrap_or_default()
    }
    /// TOC label for the current chapter, when the book provides one.
    fn chapter_label(&self) -> Option<&str> {
        self.book
            .toc
            .iter()
            .find(|entry| entry.spine_index == self.chapter_index)
            .map(|entry| entry.label.as_str())
    }
    fn page_numbers(&self) -> (usize, usize) {
        let step = self.content_height();
        (
            self.top_line / step + 1,
            self.lines.len().div_ceil(step).max(1),
        )
    }
    fn next_page(&mut self) {
        let step = self.content_height();
        if self.top_line + step < self.lines.len() {
            self.top_line += step;
            self.update_anchor();
        } else {
            self.next_chapter();
        }
    }
    /// Scroll by whole lines (arrow keys and the mouse wheel), staying
    /// inside the current chapter.
    fn scroll_lines(&mut self, delta: isize) {
        let max_top = self.lines.len().saturating_sub(self.content_height());
        let target = if delta < 0 {
            self.top_line.saturating_sub(delta.unsigned_abs())
        } else if self.top_line >= max_top {
            self.top_line
        } else {
            (self.top_line + delta.unsigned_abs()).min(max_top)
        };
        if target != self.top_line {
            self.top_line = target;
            self.update_anchor();
        }
    }
    fn previous_page(&mut self) {
        if self.top_line > 0 {
            self.top_line = self.top_line.saturating_sub(self.content_height());
            self.update_anchor();
        } else if self.chapter_index > 0 {
            self.chapter_index -= 1;
            self.top_line = 0;
            self.anchor = (0, 0);
            self.invalidate_chapter();
            self.open_at_end = true;
        }
    }
    fn next_chapter(&mut self) {
        if self.chapter_index + 1 < self.book.spine.len() {
            self.chapter_index += 1;
            self.top_line = 0;
            self.anchor = (0, 0);
            self.invalidate_chapter();
        }
    }
    fn previous_chapter(&mut self) {
        if self.chapter_index > 0 {
            self.chapter_index -= 1;
            self.top_line = 0;
            self.anchor = (0, 0);
            self.invalidate_chapter();
        }
    }
    fn position(&self) -> SavedPosition {
        SavedPosition {
            chapter_index: self.chapter_index,
            block_index: self.anchor.0,
            char_offset: self.anchor.1,
            percent: self.percentage(),
        }
    }

    /// Fraction of the book read, rounded like `KOReader`.
    #[allow(clippy::cast_precision_loss)]
    fn percentage(&self) -> f64 {
        let spine = self.book.spine.len().max(1);
        let within = if self.lines.is_empty() {
            0.0
        } else {
            (self.top_line as f64 / self.lines.len() as f64).clamp(0.0, 1.0)
        };
        sync::round_percent((self.chapter_index as f64 + within) / spine as f64)
    }

    /// Progress string (xpointer when possible) and percentage for a push.
    fn progress_payload(&self) -> (String, f64) {
        let percentage = self.percentage();
        let progress = self.source_paths.get(self.anchor.0).map_or_else(
            || format!("{percentage}"),
            |source_path| {
                sync::progress_string(
                    self.chapter_index,
                    source_path,
                    self.blocks.get(self.anchor.0).and_then(sync::block_text),
                    self.anchor.1,
                )
            },
        );
        (progress, percentage)
    }

    /// Map a pulled record to a local position: xpointer first, then
    /// percentage (chapter-level) as fallback.
    fn position_from_record(&mut self, record: &ProgressRecord) -> Option<SavedPosition> {
        if let Some(progress) = record.progress.as_deref() {
            if let Some(pointer) = XPointer::parse(progress) {
                let chapter = pointer
                    .fragment
                    .saturating_sub(1)
                    .min(self.book.spine.len().saturating_sub(1));
                if let Ok(blocks) = self.book.chapter_blocks(chapter) {
                    if let Some((block, offset)) = sync::block_for_pointer(&blocks, &pointer) {
                        return Some(SavedPosition {
                            chapter_index: chapter,
                            block_index: block,
                            char_offset: offset,
                            ..SavedPosition::default()
                        });
                    }
                }
                return Some(SavedPosition {
                    chapter_index: chapter,
                    block_index: 0,
                    char_offset: 0,
                    ..SavedPosition::default()
                });
            }
        }
        record
            .percentage
            .map(|percentage| self.position_for_percentage(percentage))
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn position_for_percentage(&self, percentage: f64) -> SavedPosition {
        let spine = self.book.spine.len().max(1);
        let scaled = percentage.clamp(0.0, 1.0) * spine as f64;
        let chapter = (scaled.floor() as usize).min(spine.saturating_sub(1));
        SavedPosition {
            chapter_index: chapter,
            block_index: 0,
            char_offset: 0,
            ..SavedPosition::default()
        }
    }

    /// Jump to a synced position.
    fn apply_position(&mut self, position: &SavedPosition) {
        self.chapter_index = position
            .chapter_index
            .min(self.book.spine.len().saturating_sub(1));
        self.anchor = (position.block_index, position.char_offset);
        self.top_line = 0;
        self.toc = None;
        self.invalidate_chapter();
    }
    fn open_toc(&mut self) {
        self.toc = Some(TocState {
            filter: TextInput::default(),
            selection: 0,
            top: 0,
        });
    }
    fn filtered_toc(&self) -> Vec<(usize, String, usize)> {
        let needle = self
            .toc
            .as_ref()
            .map_or("", |toc| toc.filter.value())
            .to_lowercase();
        (0..self.book.spine.len())
            .filter_map(|index| {
                let entry = self
                    .book
                    .toc
                    .iter()
                    .find(|entry| entry.spine_index == index);
                let label = entry.map_or_else(
                    || format!("Chapter {}", index + 1),
                    |entry| entry.label.clone(),
                );
                let depth = entry.map_or(0, |entry| entry.depth);
                (needle.is_empty()
                    || (index + 1).to_string().contains(&needle)
                    || label.to_lowercase().contains(&needle))
                .then_some((index, label, depth))
            })
            .collect()
    }
    fn toc_area(&self) -> Rect {
        let width = self.width.saturating_sub(8).clamp(30, 60);
        let height = self.height.saturating_sub(6).clamp(6, 24);
        Rect::new(
            (self.width.saturating_sub(width)) / 2,
            (self.height.saturating_sub(height)) / 2,
            width,
            height,
        )
    }
    fn sync_prompt_area(&self) -> Rect {
        let width = self.width.saturating_sub(8).clamp(40, 64);
        let height = 7;
        Rect::new(
            (self.width.saturating_sub(width)) / 2,
            (self.height.saturating_sub(height)) / 2,
            width,
            height,
        )
    }
    fn link_prompt_area(&self) -> Rect {
        let width = self.width.saturating_sub(8).clamp(44, 72);
        let height = 8;
        Rect::new(
            (self.width.saturating_sub(width)) / 2,
            (self.height.saturating_sub(height)) / 2,
            width,
            height,
        )
    }
    fn search_area(&self) -> Rect {
        let width = self.width.saturating_sub(8).clamp(30, 50);
        let height = 4;
        Rect::new(
            (self.width.saturating_sub(width)) / 2,
            (self.height.saturating_sub(height)) / 2,
            width,
            height,
        )
    }
    /// Collect case-insensitive matches for `query` across all chapters.
    fn run_search(&mut self, query: &str) {
        const MATCH_CAP: usize = 250;
        self.search_matches.clear();
        self.search_snippets.clear();
        self.search_index = 0;
        'chapters: for chapter_index in 0..self.book.spine.len() {
            let Ok(blocks) = self.book.chapter_blocks(chapter_index) else {
                continue;
            };
            for (block_index, sourced) in blocks.iter().enumerate() {
                let Some(text) = sync::block_text(&sourced.block) else {
                    continue;
                };
                for (char_offset, match_end) in find_matches(text, query) {
                    self.search_matches.push(SavedPosition {
                        chapter_index,
                        block_index,
                        char_offset,
                        ..SavedPosition::default()
                    });
                    self.search_snippets
                        .push(snippet_around(text, char_offset, match_end));
                    if self.search_matches.len() >= MATCH_CAP {
                        break 'chapters;
                    }
                }
            }
        }
    }
    fn toc_visible_rows(&self) -> usize {
        usize::from(self.toc_area().height.saturating_sub(3)).max(1)
    }
    /// Visible rows of popups without a filter row (bookmarks, results).
    fn popup_visible_rows(&self) -> usize {
        usize::from(self.toc_area().height.saturating_sub(2)).max(1)
    }
    /// Whether any reader popup is open (inline images must not cover them).
    #[cfg(feature = "inline-images")]
    fn popup_open(&self) -> bool {
        self.toc.is_some()
            || self.search.is_some()
            || self.goto_input.is_some()
            || self.results_open.is_some()
            || self.bookmarks_open.is_some()
            || self.stats_open
            || self.footnote.is_some()
            || self.sync_prompt.is_some()
            || self.link_prompt.is_some()
    }
    /// Text of the note a noteref span points at, if it can be found.
    fn footnote_text(&mut self, href: &str) -> Option<String> {
        let fragment = href.split_once('#').map(|(_, fragment)| fragment)?;
        if fragment.is_empty() {
            return None;
        }
        let chapter = self.book.spine_index_for(self.chapter_index, href)?;
        if chapter == self.chapter_index {
            let index = self
                .ids
                .iter()
                .position(|ids| ids.iter().any(|id| id == fragment))?;
            return self
                .blocks
                .get(index)
                .and_then(sync::block_text)
                .map(str::to_owned);
        }
        let blocks = self.book.chapter_blocks(chapter).ok()?;
        let block = blocks
            .iter()
            .find(|block| block.ids.iter().any(|id| id == fragment))?;
        sync::block_text(&block.block).map(str::to_owned)
    }
    fn ensure_toc_visible(&mut self) {
        let count = self.filtered_toc().len();
        let rows = self.toc_visible_rows();
        let Some(toc) = &mut self.toc else { return };
        if count == 0 {
            toc.selection = 0;
            toc.top = 0;
            return;
        }
        toc.selection = toc.selection.min(count - 1);
        if toc.selection < toc.top {
            toc.top = toc.selection;
        } else if toc.selection >= toc.top + rows {
            toc.top = toc.selection + 1 - rows;
        }
    }
    fn select_toc(&mut self) {
        let Some(toc) = &self.toc else { return };
        self.select_filtered_toc(toc.selection);
    }
    fn select_filtered_toc(&mut self, selection: usize) {
        if let Some((chapter, _, _)) = self.filtered_toc().get(selection) {
            self.chapter_index = *chapter;
            self.top_line = 0;
            self.anchor = (0, 0);
            self.toc = None;
            self.invalidate_chapter();
        }
    }
    fn handle_toc_mouse(&mut self, mouse: MouseEvent) {
        let area = self.toc_area();
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if let Some(toc) = &mut self.toc {
                    toc.selection = toc.selection.saturating_sub(1);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(toc) = &mut self.toc {
                    toc.selection += 1;
                }
            }
            MouseEventKind::Down(MouseButton::Left)
                if !area.contains(Position::new(mouse.column, mouse.row)) =>
            {
                self.toc = None;
                return;
            }
            MouseEventKind::Down(MouseButton::Left) if mouse.row == area.y + 1 => {
                if let Some(toc) = &mut self.toc {
                    toc.filter.click(mouse.column.saturating_sub(area.x + 9));
                }
                return;
            }
            MouseEventKind::Down(MouseButton::Left)
                if mouse.row > area.y + 1 && mouse.row < area.y + area.height.saturating_sub(1) =>
            {
                let top = self.toc.as_ref().map_or(0, |toc| toc.top);
                self.select_filtered_toc(top + usize::from(mouse.row - area.y - 2));
                return;
            }
            _ => {}
        }
        self.ensure_toc_visible();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_items_orders_continue_recents_aggregate_libraries_add() {
        let items = home_items(true, 2, 3);
        assert_eq!(
            items,
            vec![
                HomeItem::Continue,
                HomeItem::Recent(0),
                HomeItem::Recent(1),
                HomeItem::AllBooks,
                HomeItem::Library(0),
                HomeItem::Library(1),
                HomeItem::Library(2),
                HomeItem::AddLibrary,
            ]
        );
    }

    #[test]
    fn home_items_skips_continue_and_aggregate_when_absent() {
        assert_eq!(home_items(false, 0, 0), vec![HomeItem::AddLibrary]);
        assert_eq!(
            home_items(false, 1, 1),
            vec![
                HomeItem::Recent(0),
                HomeItem::Library(0),
                HomeItem::AddLibrary,
            ]
        );
    }

    #[test]
    fn theme_presets_cycle_through_custom_and_back() {
        let mut seen = vec![CUSTOM_THEME];
        let mut current = next_theme_preset(CUSTOM_THEME);
        while current != CUSTOM_THEME {
            seen.push(current);
            current = next_theme_preset(current);
        }
        assert_eq!(seen.len(), THEME_PRESETS.len() + 1);
        assert!(seen.contains(&"gruvbox"));
        // Unknown names fall back into the cycle instead of getting stuck.
        assert_eq!(
            next_theme_preset("no-such-theme"),
            next_theme_preset(CUSTOM_THEME)
        );
    }

    #[test]
    fn find_matches_returns_case_insensitive_byte_ranges() {
        // É is two bytes, so ranges must track bytes, not chars.
        assert_eq!(
            find_matches("CAFÉ und café", "café"),
            vec![(0, 5), (10, 15)]
        );
        assert_eq!(find_matches("aaa", "aa"), vec![(0, 2), (1, 3)]);
        assert!(find_matches("abc", "").is_empty());
    }

    #[test]
    fn external_link_policy_allows_only_browser_safe_schemes() {
        assert!(is_external_link("https://example.com/path"));
        assert!(is_external_link("http://example.com"));
        assert!(is_external_link("mailto:reader@example.com"));
        assert!(!is_external_link("file:///etc/passwd"));
        assert!(!is_external_link("javascript:alert(1)"));
        assert!(!is_external_link("../chapter.xhtml#part"));
    }

    #[test]
    fn selection_helpers_keep_utf8_boundaries_and_encode_clipboard_data() {
        assert_eq!(byte_at_display_column("a日b", 0), 0);
        assert_eq!(byte_at_display_column("a日b", 1), 1);
        assert_eq!(byte_at_display_column("a日b", 3), 4);
        assert_eq!(previous_char_boundary("a日b", 4), 1);
        assert_eq!(next_char_boundary("a日b", 1), 4);
        assert_eq!(collapse_whitespace("  one   two\tthree "), "one two three");
        assert_eq!(base64_encode(b"TerminalReader"), "VGVybWluYWxSZWFkZXI=");
    }

    #[test]
    fn image_names_are_content_addressed_and_keep_extensions() {
        let first = hashed_image_name("cover art.jpg", b"first");
        let again = hashed_image_name("cover art.jpg", b"first");
        let second = hashed_image_name("cover art.jpg", b"second");
        assert_eq!(first, again);
        assert_ne!(first, second);
        assert!(
            Path::new(&first)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jpg"))
        );
    }

    #[test]
    fn fit_pads_and_truncates_to_column_width() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        // Wide chars count as two columns.
        assert_eq!(fit("日本語", 4), "日… ");
    }

    #[test]
    fn progress_gauge_fills_proportionally() {
        assert_eq!(progress_gauge(0.0, 4, true), "----");
        assert_eq!(progress_gauge(0.5, 4, true), "==--");
        assert_eq!(progress_gauge(1.0, 4, true), "====");
        assert_eq!(progress_gauge(2.0, 4, true), "====", "clamped above 1.0");
    }

    #[test]
    fn snippet_around_clips_with_ellipses() {
        let text = "The quick brown fox jumps over the lazy dog again and again";
        let snippet = snippet_around(text, 16, 19);
        assert!(snippet.starts_with('…'));
        assert!(snippet.ends_with('…'));
        assert!(snippet.contains("fox"));
        // Whole-text snippets carry no ellipses.
        assert_eq!(snippet_around("tiny", 0, 4), "tiny");
    }
}
