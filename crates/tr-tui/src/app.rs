use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line as UiLine, Span, Text},
    widgets::{Block as TuiBlock, Borders, Clear, Paragraph},
};
use tr_core::{
    Bookmark, BookmarkStore, Config, LibraryBook, PositionStore, RecentBook, RecentsStore,
    SavedPosition, ScanCache, StatsStore, SyncStrategy, credentials, logging, scan_library_cached,
};
use tr_epub::EpubBook;
use tr_kosync::{Credentials, ProgressRecord, ProgressUpdate, xpointer::XPointer};
use tr_render::{LayoutOptions, Line, layout_with, line_for_anchor};

use crate::{
    sync::{self, SyncController, SyncEvent},
    text_input::TextInput,
    update::{UpdateController, UpdateEvent, current_version},
};

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 16;
/// How long footer status messages stay on screen.
const STATUS_TTL: Duration = Duration::from_secs(5);
const PROMPT_GO_LABEL: &str = "[Go to position]";
const PROMPT_STAY_LABEL: &str = "[Stay here]";

/// Styling that honors the `NO_COLOR` convention and the theme config.
#[derive(Debug, Clone, Copy)]
struct Palette {
    color: bool,
    accent: Color,
    light: bool,
}

impl Palette {
    fn detect(theme: &tr_core::ThemeConfig) -> Self {
        let no_color = env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        Self {
            color: !no_color,
            accent: accent_color(&theme.accent),
            light: theme.light,
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
            // Yellow is unreadable on light backgrounds.
            Style::new().fg(if self.light { Color::Blue } else { Color::Yellow })
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
            Style::new().fg(if self.light { Color::Gray } else { Color::DarkGray })
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    fn missing(self) -> Style {
        if self.color {
            Style::new().fg(Color::Red)
        } else {
            Style::new().add_modifier(Modifier::DIM)
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
}

/// A pulled server position awaiting the user's decision.
#[derive(Debug, Clone)]
struct SyncPrompt {
    position: SavedPosition,
    remote_percent: f64,
    local_percent: f64,
    device: Option<String>,
}

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
    /// Open the next laid-out chapter at its last page (backward page turn).
    open_at_end: bool,
    /// In-book search input popup, when open.
    search: Option<TextInput>,
    /// Positions of all matches from the last search.
    search_matches: Vec<SavedPosition>,
    search_index: usize,
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
    /// Behave as if this key was pressed on the current screen.
    Key(KeyCode),
    /// Move the focused text input's cursor to the clicked column.
    InputClick,
}

#[derive(Debug)]
pub struct App {
    config: Config,
    positions: PositionStore,
    recents: RecentsStore,
    scan_cache: ScanCache,
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
    help: bool,
    should_exit: bool,
    next_screen: Option<Screen>,
}

impl App {
    pub fn new(initial_book: Option<PathBuf>, offline: bool) -> Result<Self> {
        let first_run = !Config::exists();
        let (config, config_backup) = Config::load_or_backup()?;
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
        let show_wizard =
            first_run && config.library.book_dirs.is_empty() && initial_book.is_none();
        let palette = Palette::detect(&config.theme);
        let mut app = Self {
            config,
            positions: PositionStore::load()?,
            recents: RecentsStore::load()?,
            scan_cache: ScanCache::load(),
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
            help: false,
            should_exit: false,
            next_screen: None,
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
            self.process_sync_events();
            self.process_update_events();
            self.maybe_timed_push();
            self.expire_status();
            terminal.draw(|frame| self.draw(frame))?;
            if crossterm::event::poll(Duration::from_millis(50))? {
                let event = crossterm::event::read()?;
                self.handle_event(&event);
            }
        }
        // Give push-on-quit and queued retries a moment to finish.
        self.sync.flush(&self.config.sync, Duration::from_secs(3));
        Ok(())
    }

    fn handle_event(&mut self, event: &Event) {
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
            KeyCode::Enter => self.activate_home_item(home.selection),
            KeyCode::Delete | KeyCode::Backspace => self.remove_home_recent(home),
            _ => {}
        }
    }

    fn handle_library_key(&mut self, library: &mut LibraryScreen, key: KeyCode) {
        match key {
            KeyCode::Esc => self.next_screen = Some(Screen::Home(HomeScreen::default())),
            KeyCode::Up => library.selection = library.selection.saturating_sub(1),
            KeyCode::Down => {
                library.selection = (library.selection + 1)
                    .min(Self::filtered_books(library).len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(book) = Self::filtered_books(library).get(library.selection) {
                    let _ = self.open_book(book.path.clone());
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
                self.handle_settings_edit(settings, key, SettingsMode::EditMinutes, |app, value| {
                    if value.is_empty() || value == "0" {
                        app.config.sync.minutes_before_update = None;
                        return Ok("Timed pushes disabled.".to_owned());
                    }
                    let minutes: u32 = value
                        .parse()
                        .map_err(|_| "Enter minutes, 0, or leave empty.".to_owned())?;
                    app.config.sync.minutes_before_update = Some(minutes);
                    Ok(format!("Pushing every {minutes} minutes."))
                });
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

    fn handle_reader_key(&mut self, reader: &mut ReaderScreen, key: KeyCode) {
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
        if reader.bookmarks_open.is_some() {
            self.handle_bookmarks_key(reader, key);
            return;
        }
        if reader.toc.is_some() {
            match key {
                KeyCode::Esc => reader.toc = None,
                KeyCode::Up => {
                    if let Some(toc) = &mut reader.toc {
                        toc.selection = toc.selection.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    let count = reader.filtered_toc().len();
                    if let Some(toc) = &mut reader.toc {
                        toc.selection = (toc.selection + 1).min(count.saturating_sub(1));
                    }
                    reader.ensure_toc_visible();
                }
                KeyCode::Enter => reader.select_toc(),
                _ => {
                    if let Some(toc) = &mut reader.toc {
                        if toc.filter.handle_key(key) {
                            toc.selection = 0;
                            toc.top = 0;
                        }
                    }
                }
            }
            return;
        }
        match key {
            KeyCode::Esc => self.leave_reader(reader),
            KeyCode::Char(' ') | KeyCode::PageDown | KeyCode::Right => {
                reader.next_page();
                Self::note_page_turn(&self.config, &mut self.sync, reader);
            }
            KeyCode::PageUp | KeyCode::Left => {
                reader.previous_page();
                Self::note_page_turn(&self.config, &mut self.sync, reader);
            }
            KeyCode::Char(character) => self.handle_reader_char(reader, character),
            _ => {}
        }
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
                if !query.is_empty() {
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

    /// Search all chapters and jump to the first match after the current spot.
    fn run_reader_search(&mut self, reader: &mut ReaderScreen, query: &str) {
        reader.run_search(query);
        if reader.search_matches.is_empty() {
            self.status = Some(format!("No matches for \"{query}\"."));
            return;
        }
        let current = (reader.chapter_index, reader.anchor.0, reader.anchor.1);
        let start = reader
            .search_matches
            .iter()
            .position(|hit| (hit.chapter_index, hit.block_index, hit.char_offset) > current)
            .unwrap_or(0);
        reader.search_index = start;
        if let Some(position) = reader.search_matches.get(start).cloned() {
            reader.apply_position(&position);
        }
        self.status = Some(format!(
            "Match {}/{} for \"{query}\" — n/N to move",
            start + 1,
            reader.search_matches.len()
        ));
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
            if reader.bookmarks_open.is_some() {
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
                            let top = reader
                                .bookmarks_open
                                .as_ref()
                                .map_or(0, |state| state.top);
                            let index = top + usize::from(mouse.row - area.y - 1);
                            if let Some(bookmark) = self.bookmarks.list(&reader.path).get(index) {
                                let position = SavedPosition {
                                    chapter_index: bookmark.chapter_index,
                                    block_index: bookmark.block_index,
                                    char_offset: bookmark.char_offset,
                                };
                                reader.bookmarks_open = None;
                                reader.apply_position(&position);
                            }
                        }
                    }
                    _ => {}
                }
                return;
            }
            if reader.toc.is_some() {
                reader.handle_toc_mouse(mouse);
                return;
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
            Screen::Reader(_) | Screen::Wizard(_) => false,
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
                if let Screen::Library(library) = &self.screen {
                    if let Some(book) = Self::filtered_books(library).get(index) {
                        let _ = self.open_book(book.path.clone());
                    }
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
                    let message = open_reader_image(reader, block);
                    self.status = Some(message);
                }
            }
            Action::SelectLibraryDir(index) => {
                if let Screen::Settings(settings) = &mut self.screen {
                    settings.selection = index;
                }
            }
            Action::Key(code) => self.handle_key(code),
            Action::InputClick => {}
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
        match screen {
            Screen::Home(_) => rows.extend(
                [
                    "Home",
                    "  arrows      move selection",
                    "  Enter       open selection",
                    "  Del         remove selection from Recent",
                    "  s           settings",
                    "  q           quit",
                ]
                .map(String::from),
            ),
            Screen::Library(_) => rows.extend(
                [
                    "Library",
                    "  type        filter books",
                    "  arrows      move selection",
                    "  Enter       open book",
                ]
                .map(String::from),
            ),
            Screen::Settings(_) => rows.extend(
                [
                    "Settings",
                    "  a/d         add / remove library",
                    "  w j m       max width / justify / ASCII mode",
                    "  u c         sync server / matching method",
                    "  f b t       forward / backward / auto sync",
                    "  g e         push every N pages / N minutes",
                    "  l r o n     login / register / logout / device name",
                    "  v i         check for updates / install update",
                ]
                .map(String::from),
            ),
            Screen::Reader(_) => {
                let keys = reader_keys(&self.config.keys);
                rows.extend([
                    "Reader".to_owned(),
                    "  Space PgDn →   next page".to_owned(),
                    "  PgUp ←         previous page".to_owned(),
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
                    format!(
                        "  {} {}            push / pull progress now",
                        keys.sync_push, keys.sync_pull
                    ),
                    format!("  {}              toggle sync for this book", keys.sync_toggle),
                    "  click image    open image in system viewer".to_owned(),
                    "  Esc            save and go home".to_owned(),
                    format!("  {}              save and quit", keys.quit),
                ]);
            }
            Screen::Wizard(_) => rows.extend(
                [
                    "Setup",
                    "  Enter       save library directory",
                    "  Esc         skip",
                ]
                .map(String::from),
            ),
        }
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

    #[allow(clippy::too_many_lines)]
    fn draw_home(&mut self, frame: &mut Frame, home: &mut HomeScreen) {
        let area = frame.area();
        let continue_row = self
            .recents
            .most_recent()
            .filter(|recent| recent.path.exists())
            .map(|recent| {
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
            .map(|recent| (recent_row(recent), !recent.path.exists()))
            .collect();
        let dir_rows: Vec<String> = self
            .config
            .library
            .book_dirs
            .iter()
            .map(|directory| directory.display().to_string())
            .collect();

        let mut rows = Vec::new();
        let mut index = 0;
        if let Some(text) = continue_row {
            rows.push("Continue reading".to_owned());
            self.register_row(area, rows.len(), Action::HomeOpen(index));
            rows.push(Self::selectable_row(home.selection, index, &text));
            index += 1;
        } else {
            rows.push("Open a library or run terminalreader read <file>".to_owned());
        }
        rows.push(String::new());
        rows.push("Recent".to_owned());
        let mut missing_rows = Vec::new();
        for (text, missing) in &recent_rows {
            self.register_row(area, rows.len(), Action::HomeOpen(index));
            if *missing {
                missing_rows.push(rows.len());
            }
            rows.push(Self::selectable_row(home.selection, index, text));
            index += 1;
        }
        rows.push(String::new());
        rows.push("Libraries".to_owned());
        if self.config.library.book_dirs.len() > 1 {
            self.register_row(area, rows.len(), Action::HomeOpen(index));
            rows.push(Self::selectable_row(
                home.selection,
                index,
                &format!("All books ({} libraries)", self.config.library.book_dirs.len()),
            ));
            index += 1;
        }
        for text in &dir_rows {
            self.register_row(area, rows.len(), Action::HomeOpen(index));
            rows.push(Self::selectable_row(home.selection, index, text));
            index += 1;
        }
        self.register_row(area, rows.len(), Action::HomeOpen(index));
        rows.push(Self::selectable_row(
            home.selection,
            index,
            "+ Add a library…",
        ));
        home.selection = home.selection.min(index);
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
        let mut rows = vec![library.filter.render("Search: "), String::new()];
        self.register_input_row(area, 0, "Search: ");
        for (index, book) in books.iter().enumerate().skip(library.top).take(visible) {
            let prefix = if index == library.selection { ">" } else { " " };
            self.register_row(area, rows.len(), Action::LibraryOpen(index));
            rows.push(format!(
                "{prefix} {} — {}",
                book.metadata.title,
                book.metadata.authors.join(", ")
            ));
        }
        if books.is_empty() {
            rows.push("No matching EPUBs.".to_owned());
        }
        let footer = " [Home]  type: filter | Enter: open | Esc: home ";
        self.register_footer(area, footer, &[("[Home]", Action::LibraryHome)]);
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(format!(" Library: {} ", library.title))
            .title_style(self.palette.title())
            .title_bottom(self.styled_footer(area, footer));
        frame.render_widget(
            Paragraph::new(self.styled_rows(area, rows)).block(block),
            area,
        );
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
        rows.push(input.render(label));
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
        for (index, directory) in dir_rows.iter().enumerate() {
            let prefix = if index == settings.selection {
                ">"
            } else {
                " "
            };
            self.register_row(area, rows.len(), Action::SelectLibraryDir(index));
            rows.push(format!("{prefix} {directory}"));
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
        let visible_rows: Vec<String> = rows.into_iter().skip(settings.scroll).collect();
        frame.render_widget(
            Paragraph::new(self.styled_rows(area, visible_rows)).block(block),
            area,
        );
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
        let (page, count) = reader.page_numbers();
        let percent = reader.percentage() * 100.0;
        let queued = self.sync.queue_len();
        let badge = if queued > 0 && self.config.sync.auto_sync && self.config.sync.username.is_some()
        {
            format!("| {queued} queued ")
        } else {
            String::new()
        };
        let footer = format!(
            " [Contents] [Previous] [Next] [Home]  [/] chapter | ?: help | page {page}/{count} | {percent:.0}% {badge}"
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
        let content: Vec<UiLine<'static>> = reader
            .visible_lines()
            .iter()
            .map(|line| {
                let style = match reader.blocks.get(line.block) {
                    Some(tr_epub::Block::Heading { .. }) => self.palette.heading(),
                    Some(
                        tr_epub::Block::Quote(_)
                        | tr_epub::Block::Image { .. }
                        | tr_epub::Block::Rule,
                    ) => self.palette.dim(),
                    _ => Style::new(),
                };
                UiLine::from(line.text.clone()).style(style)
            })
            .collect();
        // Image boxes with a source are clickable: open in the system viewer.
        for (row, line) in reader.visible_lines().iter().enumerate() {
            if !line.atomic {
                continue;
            }
            if let Some(tr_epub::Block::Image { href: Some(_), .. }) =
                reader.blocks.get(line.block)
            {
                if let Some(y) = self.content_row_y(area, row) {
                    self.hit_targets.push((
                        Rect::new(area.x + 1, y, area.width.saturating_sub(2), 1),
                        Action::OpenImage(line.block),
                    ));
                }
            }
        }
        frame.render_widget(Paragraph::new(Text::from(content)).block(block), area);
        if reader.toc.is_some() {
            self.draw_reader_toc(frame, reader);
        }
        if reader.search.is_some() {
            Self::draw_search(frame, reader);
        }
        if reader.bookmarks_open.is_some() {
            self.draw_bookmarks(frame, reader);
        }
        if reader.sync_prompt.is_some() {
            self.draw_sync_prompt(frame, reader);
        }
    }

    fn draw_search(frame: &mut Frame, reader: &ReaderScreen) {
        let Some(input) = &reader.search else { return };
        let area = reader.search_area();
        let text = format!(
            "{}\nEnter: search all chapters | Esc: close",
            input.render("Find: ")
        );
        let widget = Paragraph::new(text).block(
            TuiBlock::default()
                .borders(Borders::ALL)
                .title(" Search "),
        );
        frame.render_widget(Clear, area);
        frame.render_widget(widget, area);
    }

    fn draw_bookmarks(&mut self, frame: &mut Frame, reader: &mut ReaderScreen) {
        let area = reader.toc_area();
        let entries = self.bookmarks.list(&reader.path);
        let visible = usize::from(area.height.saturating_sub(2)).max(1);
        let Some(state) = &mut reader.bookmarks_open else {
            return;
        };
        state.selection = state.selection.min(entries.len().saturating_sub(1));
        if state.selection < state.top {
            state.top = state.selection;
        } else if state.selection >= state.top + visible {
            state.top = state.selection + 1 - visible;
        }
        let mut rows = Vec::new();
        if entries.is_empty() {
            rows.push("No bookmarks yet — press m in the reader to add one.".to_owned());
        }
        for (index, bookmark) in entries.iter().enumerate().skip(state.top).take(visible) {
            let marker = if index == state.selection { '>' } else { ' ' };
            rows.push(format!(
                "{marker} ch. {:>3}  {}",
                bookmark.chapter_index + 1,
                bookmark.label
            ));
        }
        let widget = Paragraph::new(rows.join("\n")).block(
            TuiBlock::default().borders(Borders::ALL).title(format!(
                " Bookmarks ({}) — Enter: go, d: delete, Esc: close ",
                entries.len()
            )),
        );
        frame.render_widget(Clear, area);
        frame.render_widget(widget, area);
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
            UiLine::from(format!("{device} is {direction} this device.")),
            UiLine::from(""),
            UiLine::from(format!(
                "Server: {:.1}%   Here: {:.1}%",
                prompt.remote_percent * 100.0,
                prompt.local_percent * 100.0
            )),
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
        let Some(toc) = &mut reader.toc else { return };
        let mut rows = vec![toc.filter.render("Search: ")];
        for (index, entry) in entries.iter().enumerate().skip(toc.top).take(rows_count) {
            let marker = if index == toc.selection { '>' } else { ' ' };
            let state = match entry.0.cmp(&current) {
                std::cmp::Ordering::Equal => current_mark,
                std::cmp::Ordering::Less => read_mark,
                std::cmp::Ordering::Greater => ' ',
            };
            rows.push(format!("{marker}{state} {:>4}  {}", entry.0 + 1, entry.1));
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
        let popup = Paragraph::new(self.styled_rows(area, rows))
            .block(TuiBlock::default().borders(Borders::ALL).title(format!(
                " Chapters ({}/{}) — type to filter, Esc to close ",
                entries.len(),
                reader.book.spine.len()
            )))
            .style(Style::default().add_modifier(Modifier::BOLD));
        frame.render_widget(Clear, area);
        frame.render_widget(popup, area);
    }

    fn selectable_row(current_selection: usize, selection: usize, text: &str) -> String {
        let marker = if current_selection == selection {
            ">"
        } else {
            " "
        };
        format!("{marker} {text}")
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

    fn home_item_count(&self) -> usize {
        usize::from(
            self.recents
                .most_recent()
                .is_some_and(|recent| recent.path.exists()),
        ) + self.recents.list().len()
            + usize::from(self.config.library.book_dirs.len() > 1)
            + self.config.library.book_dirs.len()
            + 1
    }

    fn activate_home_item(&mut self, index: usize) {
        let mut cursor = 0;
        if let Some(recent) = self
            .recents
            .most_recent()
            .filter(|recent| recent.path.exists())
        {
            if index == cursor {
                let _ = self.open_book(recent.path.clone());
                return;
            }
            cursor += 1;
        }
        if let Some(recent) = self.recents.list().get(index.saturating_sub(cursor)) {
            if recent.path.exists() {
                let _ = self.open_book(recent.path.clone());
            } else {
                self.status =
                    Some("Book file is missing — press Del to remove it from Recent.".to_owned());
            }
            return;
        }
        cursor += self.recents.list().len();
        if self.config.library.book_dirs.len() > 1 {
            if index == cursor {
                self.open_aggregated_library();
                return;
            }
            cursor += 1;
        }
        if let Some(directory) = self
            .config
            .library
            .book_dirs
            .get(index.saturating_sub(cursor))
        {
            let directory = directory.clone();
            let books = scan_library_cached(&directory, &mut self.scan_cache);
            if let Err(error) = self.scan_cache.save() {
                logging::warn(&format!("could not save scan cache: {error}"));
            }
            self.next_screen = Some(Screen::Library(LibraryScreen {
                title: directory.display().to_string(),
                books,
                filter: TextInput::default(),
                selection: 0,
                top: 0,
            }));
            return;
        }
        self.next_screen = Some(Screen::Settings(SettingsScreen {
            mode: SettingsMode::AddingLibrary,
            ..SettingsScreen::default()
        }));
    }

    /// One merged, deduplicated, title-sorted list across all book dirs.
    fn open_aggregated_library(&mut self) {
        let directories = self.config.library.book_dirs.clone();
        let mut books = Vec::new();
        for directory in &directories {
            books.extend(scan_library_cached(directory, &mut self.scan_cache));
        }
        if let Err(error) = self.scan_cache.save() {
            logging::warn(&format!("could not save scan cache: {error}"));
        }
        books.sort_by(|left, right| left.path.cmp(&right.path));
        books.dedup_by(|left, right| left.path == right.path);
        books.sort_by(|left, right| left.metadata.title.cmp(&right.metadata.title));
        self.next_screen = Some(Screen::Library(LibraryScreen {
            title: format!("All books ({} libraries)", directories.len()),
            books,
            filter: TextInput::default(),
            selection: 0,
            top: 0,
        }));
    }

    /// Delete the selected recent entry (Continue reading counts as the
    /// newest one); library rows are left alone.
    fn remove_home_recent(&mut self, home: &mut HomeScreen) {
        let has_continue = self
            .recents
            .most_recent()
            .is_some_and(|recent| recent.path.exists());
        let list_index = match (has_continue, home.selection) {
            (true, 0) => 0,
            (true, selection) => selection - 1,
            (false, selection) => selection,
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
        let book = EpubBook::open(&path).with_context(|| format!("opening {}", path.display()))?;
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

    fn leave_reader(&mut self, reader: &mut ReaderScreen) {
        if self.config.sync.auto_sync {
            Self::push_progress(&self.config, &mut self.sync, reader, false);
        }
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
    ) {
        let seconds = reader.session_start.elapsed().as_secs();
        let pages = std::mem::take(&mut reader.session_pages);
        reader.session_start = Instant::now();
        if pages == 0 && seconds < 30 {
            return;
        }
        if let Err(error) = stats.record(&reader.path, seconds, pages) {
            logging::warn(&format!("could not save reading stats: {error}"));
            return;
        }
        *footer_status = Some(format!(
            "Read {} this session ({pages} pages).",
            format_duration(seconds)
        ));
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

/// Byte offsets of case-insensitive occurrences of `query` in `text`.
///
/// Compares char-by-char so offsets stay valid even when lowercasing
/// changes byte lengths.
fn find_matches(text: &str, query: &str) -> Vec<usize> {
    let query_chars: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if query_chars.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (offset, _) in text.char_indices() {
        let mut haystack = text
            .get(offset..)
            .unwrap_or_default()
            .chars()
            .flat_map(char::to_lowercase);
        if query_chars
            .iter()
            .all(|&expected| haystack.next() == Some(expected))
        {
            found.push(offset);
        }
    }
    found
}

/// Extract the clicked image to a temp file and open it with the OS default
/// handler for its file type. Returns a status message.
fn open_reader_image(reader: &mut ReaderScreen, block: usize) -> String {
    let href = match reader.blocks.get(block) {
        Some(tr_epub::Block::Image {
            href: Some(href), ..
        }) => href.clone(),
        _ => return "Image has no source to open.".to_owned(),
    };
    let (archive_path, bytes) = match reader.book.resource_bytes(reader.chapter_index, &href) {
        Ok(resource) => resource,
        Err(error) => return format!("Could not read image: {error}"),
    };
    let name = sanitize_file_name(archive_path.rsplit('/').next().unwrap_or("image"));
    let directory = env::temp_dir().join("terminalreader");
    if let Err(error) = fs::create_dir_all(&directory) {
        return format!("Could not create temp folder: {error}");
    }
    let target = directory.join(&name);
    if let Err(error) = fs::write(&target, bytes) {
        return format!("Could not write image: {error}");
    }
    match open_with_system_viewer(&target) {
        Ok(()) => format!("Opened {name} in the system viewer."),
        Err(error) => format!("Could not open the system viewer: {error}"),
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

/// One Recent list row: title — authors — ch. x/y — 2 days ago (missing).
fn recent_row(recent: &RecentBook) -> String {
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
    let missing = if recent.path.exists() {
        ""
    } else {
        " (missing)"
    };
    format!(
        "{}{authors} — ch. {}/{}{opened}{missing}",
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
            width: 0,
            height: 0,
            toc: None,
            document_digest: None,
            page_turns: 0,
            last_push: Instant::now(),
            last_pushed_progress: None,
            sync_prompt: None,
            open_at_end: false,
            search: None,
            search_matches: Vec::new(),
            search_index: 0,
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
            let (blocks, source_paths) = sourced
                .into_iter()
                .map(|block| (block.block, block.source_path))
                .unzip();
            self.blocks = blocks;
            self.source_paths = source_paths;
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
    }
    /// Invalidate layout *and* the loaded chapter blocks.
    fn invalidate_chapter(&mut self) {
        self.blocks.clear();
        self.source_paths.clear();
        self.blocks_loaded = false;
        self.open_at_end = false;
        self.invalidate_layout();
    }
    fn content_height(&self) -> usize {
        usize::from(self.height.saturating_sub(2)).max(1)
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
                        });
                    }
                }
                return Some(SavedPosition {
                    chapter_index: chapter,
                    block_index: 0,
                    char_offset: 0,
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
    fn filtered_toc(&self) -> Vec<(usize, String)> {
        let needle = self
            .toc
            .as_ref()
            .map_or("", |toc| toc.filter.value())
            .to_lowercase();
        (0..self.book.spine.len())
            .filter_map(|index| {
                let label = self
                    .book
                    .toc
                    .iter()
                    .find(|entry| entry.spine_index == index)
                    .map_or_else(
                        || format!("Chapter {}", index + 1),
                        |entry| entry.label.clone(),
                    );
                (needle.is_empty()
                    || (index + 1).to_string().contains(&needle)
                    || label.to_lowercase().contains(&needle))
                .then_some((index, label))
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
        self.search_index = 0;
        'chapters: for chapter_index in 0..self.book.spine.len() {
            let Ok(blocks) = self.book.chapter_blocks(chapter_index) else {
                continue;
            };
            for (block_index, sourced) in blocks.iter().enumerate() {
                let Some(text) = sync::block_text(&sourced.block) else {
                    continue;
                };
                for char_offset in find_matches(text, query) {
                    self.search_matches.push(SavedPosition {
                        chapter_index,
                        block_index,
                        char_offset,
                    });
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
        if let Some((chapter, _)) = self.filtered_toc().get(selection) {
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
