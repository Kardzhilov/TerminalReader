use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::Line as UiLine,
    widgets::{Block as TuiBlock, Borders, Clear, Paragraph},
};
use tr_core::{
    Config, LibraryBook, PositionStore, RecentBook, RecentsStore, SavedPosition, ScanCache,
    SyncStrategy, credentials, logging, scan_library_cached,
};
use tr_epub::EpubBook;
use tr_kosync::{Credentials, ProgressRecord, ProgressUpdate, xpointer::XPointer};
use tr_render::{LayoutOptions, Line, layout_with, line_for_anchor};

use crate::{
    sync::{self, SyncController, SyncEvent},
    text_input::TextInput,
};

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 16;

/// Styling that honors the `NO_COLOR` convention.
#[derive(Debug, Clone, Copy)]
struct Palette {
    color: bool,
}

impl Palette {
    fn detect() -> Self {
        let no_color = env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        Self { color: !no_color }
    }

    fn title(self) -> Style {
        if self.color {
            Style::new().fg(Color::Cyan)
        } else {
            Style::new()
        }
    }

    fn status(self) -> Style {
        if self.color {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().add_modifier(Modifier::ITALIC)
        }
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
    directory: PathBuf,
    books: Vec<LibraryBook>,
    filter: TextInput,
    selection: usize,
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
}

#[derive(Debug)]
struct TocState {
    filter: TextInput,
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
    sync_prompt: Option<SyncPrompt>,
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
    ReaderContents,
    ReaderPrevious,
    ReaderNext,
    ReaderHome,
    TocSelect(usize),
}

#[derive(Debug)]
pub struct App {
    config: Config,
    positions: PositionStore,
    recents: RecentsStore,
    scan_cache: ScanCache,
    sync: SyncController,
    palette: Palette,
    screen: Screen,
    hit_targets: Vec<(Rect, Action)>,
    status: Option<String>,
    help: bool,
    should_exit: bool,
    next_screen: Option<Screen>,
}

impl App {
    pub fn new(initial_book: Option<PathBuf>) -> Result<Self> {
        let first_run = !Config::exists();
        let config = Config::load()?;
        let mut sync = SyncController::new();
        if let Some(username) = &config.sync.username {
            match credentials::load_userkey(&config.sync.server_url, username) {
                Ok(Some(userkey)) => sync.set_credentials(Some(Credentials {
                    username: username.clone(),
                    userkey,
                })),
                Ok(None) => logging::warn("sync account configured but keyring has no userkey"),
                Err(error) => logging::warn(&format!("keyring unavailable: {error}")),
            }
        }
        let show_wizard =
            first_run && config.library.book_dirs.is_empty() && initial_book.is_none();
        let mut app = Self {
            config,
            positions: PositionStore::load()?,
            recents: RecentsStore::load()?,
            scan_cache: ScanCache::load(),
            sync,
            palette: Palette::detect(),
            screen: if show_wizard {
                Screen::Wizard(WizardScreen {
                    input: TextInput::new(default_library_suggestion()),
                    message: None,
                })
            } else {
                Screen::Home(HomeScreen::default())
            },
            hit_targets: Vec::new(),
            status: None,
            help: false,
            should_exit: false,
            next_screen: None,
        };
        if let Some(path) = initial_book {
            app.open_book(path)?;
            app.apply_screen_transition();
        }
        Ok(app)
    }

    pub fn run(&mut self, mut terminal: DefaultTerminal) -> Result<()> {
        while !self.should_exit {
            self.process_sync_events();
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
            Event::Mouse(mouse) => self.handle_mouse(*mouse),
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
            KeyCode::Char('q') => {
                self.leave_reader(reader);
                self.should_exit = true;
            }
            KeyCode::Esc => self.leave_reader(reader),
            KeyCode::Char('t') => reader.open_toc(),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('s') => Self::push_progress(&self.config, &mut self.sync, reader, true),
            KeyCode::Char('p') => {
                if let Some(document) = reader.document_digest.clone() {
                    self.sync.pull(&self.config.sync, document, true);
                } else {
                    self.sync.status = Some("Not signed in.".to_owned());
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
            KeyCode::Char(']') => reader.next_chapter(),
            KeyCode::Char('[') => reader.previous_chapter(),
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if let Screen::Reader(reader) = &mut self.screen {
            if reader.sync_prompt.is_some() {
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
        if let Some((_, action)) = self
            .hit_targets
            .iter()
            .find(|(rect, _)| rect.contains(Position::new(mouse.column, mouse.row)))
            .copied()
        {
            self.activate(action);
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
                MouseEventKind::Down(MouseButton::Left) if mouse.row == 1 => {
                    library.filter.click(mouse.column.saturating_sub(9));
                    true
                }
                _ => false,
            },
            Screen::Settings(settings) => {
                let directory_rows =
                    u16::try_from(self.config.library.book_dirs.len()).unwrap_or(u16::MAX);
                if settings.mode == SettingsMode::Browse
                    && mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && mouse.row >= 2
                    && mouse.row < directory_rows + 2
                {
                    settings.selection = usize::from(mouse.row - 2);
                    return true;
                }
                false
            }
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
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.hit_targets.clear();
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
            Self::draw_help(frame, &screen);
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
            wizard.input.render("Directory: "),
            String::new(),
            "Enter: save | Esc: skip for now (add libraries in Settings later)".to_owned(),
        ];
        if let Some(message) = &wizard.message {
            rows.push(String::new());
            rows.push(message.clone());
        }
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(" First-run setup ")
            .title_style(self.palette.title());
        frame.render_widget(Paragraph::new(rows.join("\n")).block(block), area);
    }

    fn draw_help(frame: &mut Frame, screen: &Screen) {
        let area = frame.area();
        let mut rows: Vec<&str> = vec![
            "Global",
            "  F1 / ?      toggle this help",
            "  Esc         back / close",
            "",
        ];
        match screen {
            Screen::Home(_) => rows.extend([
                "Home",
                "  arrows      move selection",
                "  Enter       open selection",
                "  s           settings",
                "  q           quit",
            ]),
            Screen::Library(_) => rows.extend([
                "Library",
                "  type        filter books",
                "  arrows      move selection",
                "  Enter       open book",
            ]),
            Screen::Settings(_) => rows.extend([
                "Settings",
                "  a/d         add / remove library",
                "  w j m       max width / justify / ASCII mode",
                "  u c         sync server / matching method",
                "  f b t g     forward / backward / auto sync / pages",
                "  l r o n     login / register / logout / device name",
            ]),
            Screen::Reader(_) => rows.extend([
                "Reader",
                "  Space PgDn →   next page",
                "  PgUp ←         previous page",
                "  [ ]            previous / next chapter",
                "  t              table of contents",
                "  s              push progress now",
                "  p              pull progress now",
                "  Esc            save and go home",
                "  q              save and quit",
            ]),
            Screen::Wizard(_) => rows.extend([
                "Setup",
                "  Enter       save library directory",
                "  Esc         skip",
            ]),
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

    fn draw_home(&mut self, frame: &mut Frame, home: &mut HomeScreen) {
        let area = frame.area();
        let mut rows = Vec::new();
        let mut index = 0;
        if let Some(recent) = self
            .recents
            .most_recent()
            .filter(|recent| recent.path.exists())
        {
            rows.push("Continue reading".to_owned());
            rows.push(Self::selectable_row(
                home.selection,
                index,
                &format!(
                    "{} — ch. {}/{}",
                    recent.title,
                    recent.last_chapter + 1,
                    recent.spine_count
                ),
            ));
            index += 1;
        } else {
            rows.push("Open a library or run terminalreader read <file>".to_owned());
        }
        rows.push(String::new());
        rows.push("Recent".to_owned());
        for recent in self.recents.list() {
            let missing = if recent.path.exists() {
                ""
            } else {
                " (missing)"
            };
            rows.push(Self::selectable_row(
                home.selection,
                index,
                &format!("{}{}", recent.title, missing),
            ));
            index += 1;
        }
        rows.push(String::new());
        rows.push("Libraries".to_owned());
        for directory in &self.config.library.book_dirs {
            rows.push(Self::selectable_row(
                home.selection,
                index,
                &directory.display().to_string(),
            ));
            index += 1;
        }
        rows.push(Self::selectable_row(
            home.selection,
            index,
            "+ Add a library…",
        ));
        home.selection = home.selection.min(index);
        let footer = " [Settings] [Quit]  Enter: open | arrows: move | ?: help | q: quit ";
        let mut block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(" TerminalReader ")
            .title_style(self.palette.title())
            .title_bottom(footer);
        if let Some(status) = self.status_line() {
            block = block.title_bottom(
                UiLine::from(format!(" {status} "))
                    .style(self.palette.status())
                    .right_aligned(),
            );
        }
        frame.render_widget(Paragraph::new(rows.join("\n")).block(block), area);
        self.register_footer(
            area,
            footer,
            &[
                ("[Settings]", Action::HomeSettings),
                ("[Quit]", Action::HomeQuit),
            ],
        );
        self.register_home_rows(area, home.selection);
    }

    fn draw_library(&mut self, frame: &mut Frame, library: &mut LibraryScreen) {
        let area = frame.area();
        let books = Self::filtered_books(library)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        library.selection = library.selection.min(books.len().saturating_sub(1));
        let mut rows = vec![library.filter.render("Search: "), String::new()];
        for (index, book) in books
            .iter()
            .take(usize::from(area.height.saturating_sub(5)))
            .enumerate()
        {
            let prefix = if index == library.selection { ">" } else { " " };
            rows.push(format!(
                "{prefix} {} — {}",
                book.metadata.title,
                book.metadata.authors.join(", ")
            ));
            self.hit_targets.push((
                Rect::new(
                    1,
                    u16::try_from(index + 3).unwrap_or(0),
                    area.width.saturating_sub(2),
                    1,
                ),
                Action::LibraryOpen(index),
            ));
        }
        if books.is_empty() {
            rows.push("No matching EPUBs.".to_owned());
        }
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(format!(" Library: {} ", library.directory.display()))
            .title_bottom(" [Home]  type: filter | Enter: open | Esc: home ");
        frame.render_widget(Paragraph::new(rows.join("\n")).block(block), area);
        self.register_footer(area, " [Home] ", &[("[Home]", Action::LibraryHome)]);
    }

    #[allow(clippy::too_many_lines)]
    fn draw_settings(&mut self, frame: &mut Frame, settings: &mut SettingsScreen) {
        let area = frame.area();
        let mut rows = vec!["Libraries  (a: add, d: remove)".to_owned()];
        if self.config.library.book_dirs.is_empty() {
            rows.push("  (none configured)".to_owned());
        }
        for (index, directory) in self.config.library.book_dirs.iter().enumerate() {
            let prefix = if index == settings.selection {
                ">"
            } else {
                " "
            };
            rows.push(format!("{prefix} {}", directory.display()));
        }
        rows.push(String::new());
        rows.push("Reading".to_owned());
        rows.push(format!(
            "  [w] Max width: {}",
            self.config
                .reading
                .max_width
                .map_or("full".to_owned(), |width| width.to_string())
        ));
        rows.push(format!(
            "  [j] Justify: {}",
            on_off(self.config.reading.justify)
        ));
        rows.push(format!(
            "  [m] ASCII mode: {}",
            on_off(self.config.reading.ascii_only)
        ));
        rows.push(String::new());
        rows.push("Progress sync (KOReader-compatible)".to_owned());
        rows.push(format!("  [u] Server: {}", self.config.sync.server_url));
        let account = match (&self.config.sync.username, self.sync.logged_in()) {
            (Some(username), true) => format!("{username} (signed in)"),
            (Some(username), false) => format!("{username} (keyring locked or signed out)"),
            (None, _) => "not signed in".to_owned(),
        };
        rows.push(format!(
            "  [l] Login  [r] Register  [o] Logout — account: {account}"
        ));
        rows.push(format!(
            "  [c] Matching: {}",
            matching_label(self.config.sync.matching)
        ));
        rows.push(format!(
            "  [f] Forward: {}   [b] Backward: {}",
            self.config.sync.sync_forward.label(),
            self.config.sync.sync_backward.label()
        ));
        rows.push(format!(
            "  [t] Auto sync: {}   [g] Push every: {}",
            on_off(self.config.sync.auto_sync),
            self.config
                .sync
                .pages_before_update
                .map_or("off".to_owned(), |pages| format!("{pages} pages"))
        ));
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
        match &settings.mode {
            SettingsMode::AddingLibrary => {
                rows.push(settings.input.render("Add directory: "));
                rows.push("Enter: save | Esc: cancel".to_owned());
            }
            SettingsMode::ConfirmRemove => {
                rows.push("Remove selected library? Enter: confirm | Esc: cancel".to_owned());
            }
            SettingsMode::EditWidth => {
                rows.push(settings.input.render("Max width (empty = full): "));
                rows.push("Enter: save | Esc: cancel".to_owned());
            }
            SettingsMode::EditServer => {
                rows.push(settings.input.render("Server URL: "));
                rows.push("Enter: save | Esc: cancel".to_owned());
            }
            SettingsMode::EditPages => {
                rows.push(settings.input.render("Pages between pushes (0 = off): "));
                rows.push("Enter: save | Esc: cancel".to_owned());
            }
            SettingsMode::EditDevice => {
                rows.push(settings.input.render("Device name: "));
                rows.push("Enter: save | Esc: cancel".to_owned());
            }
            SettingsMode::LoginUser { register } => {
                let label = if *register {
                    "Register — username: "
                } else {
                    "Login — username: "
                };
                rows.push(settings.input.render(label));
                rows.push("Enter: next | Esc: cancel".to_owned());
            }
            SettingsMode::LoginPass { username, .. } => {
                rows.push(settings.input.render(&format!("Password for {username}: ")));
                rows.push("Enter: sign in | Esc: cancel".to_owned());
            }
            SettingsMode::Browse => {}
        }
        if let Some(message) = &settings.message {
            rows.push(message.clone());
        }
        if let Some(status) = &self.sync.status {
            rows.push(format!("Sync: {status}"));
        }
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(" Settings ")
            .title_style(self.palette.title())
            .title_bottom(" [Add] [Remove] [Home]  keys in brackets | ?: help | Esc: home ");
        frame.render_widget(Paragraph::new(rows.join("\n")).block(block), area);
        self.register_footer(
            area,
            " [Add] [Remove] [Home] ",
            &[
                ("[Add]", Action::SettingsAdd),
                ("[Remove]", Action::SettingsRemove),
                ("[Home]", Action::SettingsHome),
            ],
        );
    }

    fn draw_reader(&mut self, frame: &mut Frame, reader: &mut ReaderScreen) {
        let area = frame.area();
        let options = LayoutOptions {
            ascii_only: self.config.reading.ascii_only,
            justify: self.config.reading.justify,
            max_width: self.config.reading.max_width,
        };
        reader.ensure_layout(area.width, area.height, options);
        let (page, count) = reader.page_numbers();
        let footer = format!(
            " [Contents] [Previous] [Next] [Home]  [/] chapter | ?: help | page {page}/{count} "
        );
        let mut block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(format!(
                " {} — chapter {}/{} ",
                reader.book.metadata.title,
                reader.chapter_index + 1,
                reader.book.spine.len()
            ))
            .title_style(self.palette.title())
            .title_bottom(footer.clone());
        if let Some(status) = self.status_line() {
            block = block.title_bottom(
                UiLine::from(format!(" {status} "))
                    .style(self.palette.status())
                    .right_aligned(),
            );
        }
        frame.render_widget(
            Paragraph::new(reader.visible_lines().join("\n")).block(block),
            area,
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
        if reader.toc.is_some() {
            self.draw_reader_toc(frame, reader);
        }
        if reader.sync_prompt.is_some() {
            Self::draw_sync_prompt(frame, reader);
        }
    }

    fn draw_sync_prompt(frame: &mut Frame, reader: &ReaderScreen) {
        let Some(prompt) = &reader.sync_prompt else {
            return;
        };
        let area = frame.area();
        let width = area.width.saturating_sub(8).clamp(40, 64);
        let height = 7;
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        let direction = if prompt.remote_percent > prompt.local_percent {
            "ahead of"
        } else {
            "behind"
        };
        let device = prompt.device.as_deref().unwrap_or("another device");
        let rows = [
            format!("{device} is {direction} this device."),
            String::new(),
            format!(
                "Server: {:.1}%   Here: {:.1}%",
                prompt.remote_percent * 100.0,
                prompt.local_percent * 100.0
            ),
            String::new(),
            "Enter: go to synced position | Esc: stay here".to_owned(),
        ];
        let widget = Paragraph::new(rows.join("\n")).block(
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
        let Some(toc) = &mut reader.toc else { return };
        let mut rows = vec![toc.filter.render("Search: ")];
        for (index, entry) in entries.iter().enumerate().skip(toc.top).take(rows_count) {
            let marker = if index == toc.selection { ">" } else { " " };
            rows.push(format!("{marker} {:>4}  {}", entry.0 + 1, entry.1));
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
        let popup = Paragraph::new(rows.join("\n"))
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

    fn register_home_rows(&mut self, area: Rect, selection: usize) {
        let mut action_index = 0_usize;
        let mut row = if self
            .recents
            .most_recent()
            .is_some_and(|recent| recent.path.exists())
        {
            2
        } else {
            0
        };
        if self
            .recents
            .most_recent()
            .is_some_and(|recent| recent.path.exists())
        {
            self.hit_targets.push((
                Rect::new(1, row, area.width.saturating_sub(2), 1),
                Action::HomeOpen(action_index),
            ));
            action_index += 1;
        }
        row += 3;
        for _ in self.recents.list() {
            self.hit_targets.push((
                Rect::new(1, row, area.width.saturating_sub(2), 1),
                Action::HomeOpen(action_index),
            ));
            row += 1;
            action_index += 1;
        }
        row += 2;
        for _ in &self.config.library.book_dirs {
            self.hit_targets.push((
                Rect::new(1, row, area.width.saturating_sub(2), 1),
                Action::HomeOpen(action_index),
            ));
            row += 1;
            action_index += 1;
        }
        self.hit_targets.push((
            Rect::new(1, row, area.width.saturating_sub(2), 1),
            Action::HomeOpen(action_index),
        ));
        let _ = selection;
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

    fn home_item_count(&self) -> usize {
        usize::from(
            self.recents
                .most_recent()
                .is_some_and(|recent| recent.path.exists()),
        ) + self.recents.list().len()
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
                self.status = Some("Book file is missing.".to_owned());
            }
            return;
        }
        cursor += self.recents.list().len();
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
                directory,
                books,
                filter: TextInput::default(),
                selection: 0,
            }));
            return;
        }
        self.next_screen = Some(Screen::Settings(SettingsScreen {
            mode: SettingsMode::AddingLibrary,
            ..SettingsScreen::default()
        }));
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
        if self.sync.logged_in() {
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
        if let Some(pages) = config.sync.pages_before_update {
            if pages > 0 && reader.page_turns >= pages {
                reader.page_turns = 0;
                Self::push_progress(config, sync, reader, false);
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
                                Ok(record) => self.apply_pull(reader, &record, manual),
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
        let strategy = if manual {
            SyncStrategy::Silent
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

fn matching_label(matching: tr_core::MatchingMethod) -> &'static str {
    match matching {
        tr_core::MatchingMethod::Binary => "binary (identical files)",
        tr_core::MatchingMethod::Filename => "filename",
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
            sync_prompt: None,
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
        self.top_line = line_for_anchor(&self.lines, self.anchor.0, self.anchor.1);
        self.clamp_top();
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
    fn visible_lines(&self) -> Vec<String> {
        self.lines
            .get(self.top_line..(self.top_line + self.content_height()).min(self.lines.len()))
            .unwrap_or_default()
            .iter()
            .map(|line| line.text.clone())
            .collect()
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
