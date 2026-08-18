use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    widgets::{Block as TuiBlock, Borders, Clear, Paragraph},
};
use tr_core::{
    Config, LibraryBook, PositionStore, RecentBook, RecentsStore, SavedPosition, scan_library,
};
use tr_epub::{Block, EpubBook};
use tr_render::{Line, layout, line_for_anchor};

use crate::text_input::TextInput;

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 16;

#[derive(Debug)]
enum Screen {
    Home(HomeScreen),
    Library(LibraryScreen),
    Settings(SettingsScreen),
    Reader(Box<ReaderScreen>),
}

#[derive(Debug, Default)]
struct HomeScreen {
    selection: usize,
}

#[derive(Debug)]
struct LibraryScreen {
    directory: PathBuf,
    books: Vec<LibraryBook>,
    filter: TextInput,
    selection: usize,
}

#[derive(Debug, Default)]
struct SettingsScreen {
    selection: usize,
    adding: bool,
    input: TextInput,
    confirming_remove: bool,
    message: Option<String>,
}

#[derive(Debug)]
struct TocState {
    filter: TextInput,
    selection: usize,
    top: usize,
}

#[derive(Debug)]
struct ReaderScreen {
    path: PathBuf,
    book: EpubBook,
    chapter_index: usize,
    top_line: usize,
    anchor: (usize, usize),
    lines: Vec<Line>,
    width: u16,
    height: u16,
    toc: Option<TocState>,
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
    screen: Screen,
    hit_targets: Vec<(Rect, Action)>,
    status: Option<String>,
    should_exit: bool,
    next_screen: Option<Screen>,
}

impl App {
    pub fn new(initial_book: Option<PathBuf>) -> Result<Self> {
        let mut app = Self {
            config: Config::load()?,
            positions: PositionStore::load()?,
            recents: RecentsStore::load()?,
            screen: Screen::Home(HomeScreen::default()),
            hit_targets: Vec::new(),
            status: None,
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
            terminal.draw(|frame| self.draw(frame))?;
            if crossterm::event::poll(Duration::from_millis(50))? {
                let event = crossterm::event::read()?;
                self.handle_event(&event);
            }
        }
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
        let mut screen = std::mem::replace(&mut self.screen, Screen::Home(HomeScreen::default()));
        match &mut screen {
            Screen::Home(home) => self.handle_home_key(home, key),
            Screen::Library(library) => self.handle_library_key(library, key),
            Screen::Settings(settings) => self.handle_settings_key(settings, key),
            Screen::Reader(reader) => self.handle_reader_key(reader, key),
        }
        self.screen = screen;
        self.apply_screen_transition();
    }

    fn handle_home_key(&mut self, home: &mut HomeScreen, key: KeyCode) {
        let count = self.home_item_count();
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_exit = true,
            KeyCode::Char('s') => {
                self.next_screen = Some(Screen::Settings(SettingsScreen::default()));
            }
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

    fn handle_settings_key(&mut self, settings: &mut SettingsScreen, key: KeyCode) {
        if settings.adding {
            match key {
                KeyCode::Esc => settings.adding = false,
                KeyCode::Enter => {
                    let path = PathBuf::from(settings.input.value());
                    match self.config.add_book_dir(&path) {
                        Ok(true) => {
                            settings.message = Some("Library added.".to_owned());
                            settings.adding = false;
                            settings.input.clear();
                        }
                        Ok(false) => {
                            settings.message = Some("Library is already configured.".to_owned());
                        }
                        Err(error) => settings.message = Some(error.to_string()),
                    }
                }
                _ => {
                    let _ = settings.input.handle_key(key);
                }
            }
            return;
        }
        if settings.confirming_remove {
            match key {
                KeyCode::Esc => settings.confirming_remove = false,
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
                    settings.confirming_remove = false;
                }
                _ => {}
            }
            return;
        }
        match key {
            KeyCode::Esc => self.next_screen = Some(Screen::Home(HomeScreen::default())),
            KeyCode::Char('a') => {
                settings.adding = true;
                settings.message = None;
            }
            KeyCode::Char('d') if !self.config.library.book_dirs.is_empty() => {
                settings.confirming_remove = true;
            }
            KeyCode::Up => settings.selection = settings.selection.saturating_sub(1),
            KeyCode::Down => {
                settings.selection = (settings.selection + 1)
                    .min(self.config.library.book_dirs.len().saturating_sub(1));
            }
            _ => {}
        }
    }

    fn handle_reader_key(&mut self, reader: &mut ReaderScreen, key: KeyCode) {
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
            KeyCode::Char(' ') | KeyCode::PageDown | KeyCode::Right => reader.next_page(),
            KeyCode::PageUp | KeyCode::Left => reader.previous_page(),
            KeyCode::Char(']') => reader.next_chapter(),
            KeyCode::Char('[') => reader.previous_chapter(),
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if let Screen::Reader(reader) = &mut self.screen {
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
                if settings.adding
                    && mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && mouse.row == directory_rows + 3
                {
                    settings.input.click(mouse.column.saturating_sub(16));
                    return true;
                }
                if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && mouse.row >= 2
                    && mouse.row < directory_rows + 2
                {
                    settings.selection = usize::from(mouse.row - 2);
                    return true;
                }
                false
            }
            Screen::Reader(_) => false,
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
                    settings.adding = true;
                }
            }
            Action::SettingsRemove => {
                if let Screen::Settings(settings) = &mut self.screen {
                    settings.confirming_remove = !self.config.library.book_dirs.is_empty();
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
                }
            }
            Action::ReaderNext => {
                if let Screen::Reader(reader) = &mut self.screen {
                    reader.next_page();
                }
            }
            Action::ReaderHome => {
                if let Screen::Reader(reader) = &mut self.screen {
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
        }
        self.screen = screen;
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
        let footer = " [Settings] [Quit]  Enter: open | arrows: move | q: quit ";
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(" TerminalReader ")
            .title_bottom(footer);
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

    fn draw_settings(&mut self, frame: &mut Frame, settings: &mut SettingsScreen) {
        let area = frame.area();
        let mut rows = vec!["Libraries".to_owned()];
        for (index, directory) in self.config.library.book_dirs.iter().enumerate() {
            let prefix = if index == settings.selection {
                ">"
            } else {
                " "
            };
            rows.push(format!("{prefix} {}", directory.display()));
        }
        if settings.adding {
            rows.push(String::new());
            rows.push(settings.input.render("Add directory: "));
            rows.push("Enter: save | Esc: cancel".to_owned());
        } else if settings.confirming_remove {
            rows.push(String::new());
            rows.push("Remove selected library? Enter: confirm | Esc: cancel".to_owned());
        } else {
            rows.push(String::new());
            rows.push("Reading settings will appear here in a later milestone.".to_owned());
        }
        if let Some(message) = &settings.message {
            rows.push(message.clone());
        }
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(" Settings ")
            .title_bottom(" [Add] [Remove] [Home]  arrows: select | Esc: home ");
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
        reader.ensure_layout(area.width, area.height);
        let (page, count) = reader.page_numbers();
        let footer =
            format!(" [Contents] [Previous] [Next] [Home]  [/] chapter | page {page}/{count} ");
        let block = TuiBlock::default()
            .borders(Borders::ALL)
            .title(format!(
                " {} — chapter {}/{} ",
                reader.book.metadata.title,
                reader.chapter_index + 1,
                reader.book.spine.len()
            ))
            .title_bottom(footer.clone());
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
                self.status = Some(
                    "Book is missing. Remove it from recents in a future polish pass.".to_owned(),
                );
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
            self.next_screen = Some(Screen::Library(LibraryScreen {
                directory: directory.clone(),
                books: scan_library(directory),
                filter: TextInput::default(),
                selection: 0,
            }));
            return;
        }
        self.next_screen = Some(Screen::Settings(SettingsScreen {
            adding: true,
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
        self.next_screen = Some(Screen::Reader(Box::new(ReaderScreen::new(
            path, book, &position,
        ))));
        Ok(())
    }

    fn leave_reader(&mut self, reader: &mut ReaderScreen) {
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

    fn apply_screen_transition(&mut self) {
        if let Some(screen) = self.next_screen.take() {
            self.screen = screen;
        }
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
            lines: Vec::new(),
            width: 0,
            height: 0,
            toc: None,
        }
    }

    fn ensure_layout(&mut self, width: u16, height: u16) {
        self.height = height;
        if self.width == width && !self.lines.is_empty() {
            return;
        }
        self.width = width;
        self.lines = self.book.chapter_blocks(self.chapter_index).map_or_else(
            |_| {
                vec![Line {
                    text: "Unable to render this chapter.".to_owned(),
                    block: 0,
                    char_offset: 0,
                    atomic: false,
                }]
            },
            |blocks| {
                layout(
                    &blocks
                        .into_iter()
                        .map(|block| block.block)
                        .collect::<Vec<Block>>(),
                    width,
                    false,
                )
            },
        );
        self.top_line = line_for_anchor(&self.lines, self.anchor.0, self.anchor.1);
        self.clamp_top();
    }

    fn invalidate_layout(&mut self) {
        self.width = 0;
        self.lines.clear();
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
            self.invalidate_layout();
        }
    }
    fn previous_chapter(&mut self) {
        if self.chapter_index > 0 {
            self.chapter_index -= 1;
            self.top_line = 0;
            self.anchor = (0, 0);
            self.invalidate_layout();
        }
    }
    fn position(&self) -> SavedPosition {
        SavedPosition {
            chapter_index: self.chapter_index,
            block_index: self.anchor.0,
            char_offset: self.anchor.1,
        }
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
            self.invalidate_layout();
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
