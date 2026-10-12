//! The start screen: choose which project to open before anything opens.
//!
//! Shown when ActiveGit is started with `--pick` (the Start-menu / taskbar
//! shortcut does that) and when there is no repository to open at all. It
//! lists the recent projects, the home folder and the drives, and browses
//! folders from there with the same picker as `o` inside the app.

use crate::app::{BrowserRow, OpenBrowser};
use crate::config::Theme;
use crate::input::{self, Chord, Input};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, MouseButton, MouseEventKind};
use git_tui_core::error::GitError;
use git_tui_core::repo::Repo;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How the start screen ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    /// Open this repository.
    Open(PathBuf),
    Quit,
}

/// A second click on the same row this soon opens it.
const DOUBLE_CLICK: Duration = Duration::from_millis(450);

pub struct StartScreen {
    pub(crate) browser: OpenBrowser,
    /// The typed path (`tab`, or a folder dragged into the window).
    pub(crate) path_line: String,
    /// A plain folder was picked: waiting for a yes/no on `git init`.
    pub(crate) confirm_init: Option<PathBuf>,
    /// Screen row to list index, recorded while drawing (mouse clicks).
    pub(crate) hits: Vec<(u16, usize)>,
    last_click: Option<(Instant, u16)>,
}

impl StartScreen {
    /// Starts at the places level: `recents` first, then home and drives.
    pub fn new(recents: Vec<PathBuf>) -> Self {
        Self {
            browser: OpenBrowser::new(PathBuf::new(), recents),
            path_line: String::new(),
            confirm_init: None,
            hits: Vec::new(),
            last_click: None,
        }
    }

    pub fn on_key(&mut self, key: KeyCode) -> Option<Choice> {
        if let Some(dir) = self.confirm_init.take() {
            if key == KeyCode::Enter {
                match Repo::init(&dir) {
                    Ok(_) => return Some(Choice::Open(dir)),
                    Err(e) => {
                        self.browser.error = Some(format!("cannot init {}: {e}", dir.display()))
                    }
                }
            }
            // Anything else backs out to the list.
            return None;
        }
        if self.browser.editing_path {
            match key {
                KeyCode::Char(c) => self.path_line.push(c),
                KeyCode::Backspace => {
                    self.path_line.pop();
                }
                KeyCode::Enter => self.submit_path(),
                KeyCode::Esc => {
                    self.browser.editing_path = false;
                    self.path_line.clear();
                }
                _ => {}
            }
            return None;
        }
        match key {
            KeyCode::Char(c) => self.browser.push_filter_char(c),
            KeyCode::Backspace => {
                if self.browser.filter.is_empty() {
                    self.go_up();
                } else {
                    self.browser.pop_filter_char();
                }
            }
            KeyCode::Up => self.browser.move_cursor(-1),
            KeyCode::Down => self.browser.move_cursor(1),
            KeyCode::PageUp => self.browser.move_cursor(-10),
            KeyCode::PageDown => self.browser.move_cursor(10),
            KeyCode::Enter => return self.activate(),
            KeyCode::Right => {
                if matches!(self.browser.selected_row(), BrowserRow::Dir(_)) {
                    let target = self.browser.selected_path();
                    self.browser.goto(target);
                }
            }
            KeyCode::Left => self.go_up(),
            KeyCode::Tab => {
                self.browser.editing_path = true;
                self.path_line.clear();
            }
            KeyCode::Esc => {
                if self.browser.filter.is_empty() {
                    return Some(Choice::Quit);
                }
                self.browser.clear_filter();
            }
            _ => {}
        }
        None
    }

    /// Pasted text (or a folder dragged into the window) is a path.
    pub fn on_paste(&mut self, text: &str) {
        let line: String = text.chars().filter(|c| !c.is_control()).collect();
        self.confirm_init = None;
        self.browser.editing_path = true;
        self.path_line = line;
    }

    /// Left click on screen row `row`: select it; a second click opens it.
    pub fn on_click(&mut self, row: u16) -> Option<Choice> {
        self.confirm_init = None;
        let index = self.hits.iter().find(|(y, _)| *y == row).map(|(_, i)| *i)?;
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(at, r)| r == row && now.duration_since(at) < DOUBLE_CLICK);
        self.last_click = Some((now, row));
        if double && index == self.browser.selected {
            self.last_click = None;
            return self.activate();
        }
        self.browser.selected = index;
        None
    }

    pub fn on_wheel(&mut self, up: bool) {
        self.browser.move_cursor(if up { -1 } else { 1 });
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.browser.parent() {
            self.browser.goto(parent);
        }
    }

    /// Enter on the highlighted row: a repository opens, a plain folder is
    /// browsed into, `.` on a plain folder offers `git init`.
    fn activate(&mut self) -> Option<Choice> {
        if !self.browser.filter.is_empty() && self.browser.view.is_empty() {
            self.browser.error = Some(format!(
                "no folders matching {:?} here",
                self.browser.filter
            ));
            return None;
        }
        let row = self.browser.selected_row();
        if row == BrowserRow::Parent {
            self.go_up();
            return None;
        }
        let target = self.browser.selected_path();
        match Repo::discover_root(&target) {
            Ok(root) => Some(Choice::Open(root)),
            Err(GitError::NotARepo(_)) if target.is_dir() => {
                if row == BrowserRow::Current {
                    self.confirm_init = Some(target);
                } else {
                    self.browser.goto(target);
                }
                None
            }
            Err(e) => {
                self.browser.error = Some(e.to_string());
                None
            }
        }
    }

    /// Enter on the typed path: move the list there.
    fn submit_path(&mut self) {
        // Paths dragged onto a terminal arrive wrapped in quotes.
        let typed = self.path_line.trim().trim_matches('"').to_string();
        let path = match Repo::normalize_project_input(&typed) {
            Ok(path) => path,
            Err(e) => {
                self.browser.error = Some(e.to_string());
                return;
            }
        };
        if !path.is_dir() {
            self.browser.error = Some(format!("not a directory: {}", path.display()));
            return;
        }
        let dir = dunce::canonicalize(&path).unwrap_or(path);
        if self.browser.goto(dir) {
            self.browser.editing_path = false;
            self.path_line.clear();
        }
    }
}

/// Run the start screen until a project is picked or the user quits.
pub fn choose(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    theme: Theme,
    recents: Vec<PathBuf>,
) -> Result<Choice> {
    let mut screen = StartScreen::new(recents);
    loop {
        terminal
            .draw(|f| crate::ui::render_start(f, &mut screen, theme))
            .context("cannot render")?;
        // Everything already queued, so a paste that arrives as key presses
        // is seen whole (see `input::paste_text`).
        let mut batch = vec![event::read().context("cannot read input")?];
        while event::poll(Duration::ZERO).context("cannot poll input")? {
            batch.push(event::read().context("cannot read input")?);
        }
        let inputs: Vec<Input> = batch
            .iter()
            .filter_map(|event| match event {
                Event::Key(key) => Some(input::classify(key)),
                _ => None,
            })
            .collect();
        let pasted = input::paste_text(&inputs);
        if let Some(text) = &pasted {
            screen.on_paste(text);
        }
        for event in batch {
            let choice = match event {
                Event::Key(_) if pasted.is_some() => None,
                Event::Key(key) => match input::classify(&key) {
                    Input::Key(code, _) => screen.on_key(code),
                    Input::Chord(Chord::Ctrl('n')) => screen.on_key(KeyCode::Down),
                    Input::Chord(Chord::Ctrl('p')) => screen.on_key(KeyCode::Up),
                    Input::Quit => Some(Choice::Quit),
                    _ => None,
                },
                Event::Paste(text) => {
                    screen.on_paste(&text);
                    None
                }
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => screen.on_click(m.row),
                    MouseEventKind::ScrollUp => {
                        screen.on_wheel(true);
                        None
                    }
                    MouseEventKind::ScrollDown => {
                        screen.on_wheel(false);
                        None
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(choice) = choice {
                return Ok(choice);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        dir
    }

    fn canon(path: &std::path::Path) -> PathBuf {
        dunce::canonicalize(path).unwrap()
    }

    #[test]
    fn starts_at_the_places_with_recent_projects_first() {
        let recent = repo();
        let screen = StartScreen::new(vec![recent.path().to_path_buf()]);
        assert!(screen.browser.at_places());
        // No `.` / `..` up here: row 0 is the first recent project.
        assert_eq!(screen.browser.selected_row(), BrowserRow::Dir(0));
        assert_eq!(screen.browser.selected_path(), recent.path());
        assert!(screen.browser.view[0].is_repo_root);
        // Home and at least one drive (or `/`) follow.
        assert!(screen.browser.view.len() >= 2);
        assert!(screen.browser.parent().is_none());
    }

    #[test]
    fn enter_on_a_recent_project_opens_it() {
        let recent = repo();
        let mut screen = StartScreen::new(vec![recent.path().to_path_buf()]);
        match screen.on_key(KeyCode::Enter) {
            Some(Choice::Open(root)) => assert_eq!(canon(&root), canon(recent.path())),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn esc_clears_the_filter_then_quits() {
        let mut screen = StartScreen::new(Vec::new());
        screen.on_key(KeyCode::Char('z'));
        assert_eq!(screen.on_key(KeyCode::Esc), None);
        assert!(screen.browser.filter.is_empty());
        assert_eq!(screen.on_key(KeyCode::Esc), Some(Choice::Quit));
    }

    #[test]
    fn a_drive_root_leads_up_to_the_places_and_back_down() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut screen = StartScreen::new(Vec::new());
        screen.on_paste(&format!("\"{}\"", dir.path().display()));
        assert!(screen.browser.editing_path);
        assert_eq!(screen.on_key(KeyCode::Enter), None);
        assert_eq!(screen.browser.cwd, canon(dir.path()));
        // Left all the way: every folder, the drive, then the places.
        for _ in 0..64 {
            screen.on_key(KeyCode::Left);
        }
        assert!(screen.browser.at_places());
        // Every place is a folder that can be entered again.
        let last = screen.browser.view.len() - 1;
        screen.browser.selected = last;
        let drive = screen.browser.selected_path();
        screen.on_key(KeyCode::Right);
        assert_eq!(screen.browser.cwd, drive);
        assert_eq!(screen.browser.selected_row(), BrowserRow::Current);
    }

    #[test]
    fn a_plain_folder_is_browsed_and_dot_offers_git_init() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("inner")).unwrap();
        let mut screen = StartScreen::new(Vec::new());
        screen.on_paste(&dir.path().display().to_string());
        screen.on_key(KeyCode::Enter);
        // Row 2 is the only subfolder: Enter goes into it.
        screen.browser.selected = 2;
        assert_eq!(screen.on_key(KeyCode::Enter), None);
        let inner = canon(&dir.path().join("inner"));
        assert_eq!(screen.browser.cwd, inner);
        // `.` on a plain folder asks first; Esc backs out, Enter creates.
        assert_eq!(screen.on_key(KeyCode::Enter), None);
        assert_eq!(screen.confirm_init.as_deref(), Some(inner.as_path()));
        assert_eq!(screen.on_key(KeyCode::Esc), None);
        assert!(screen.confirm_init.is_none());
        assert!(!inner.join(".git").exists());
        screen.on_key(KeyCode::Enter);
        assert_eq!(
            screen.on_key(KeyCode::Enter),
            Some(Choice::Open(inner.clone()))
        );
        assert!(inner.join(".git").exists());
    }

    #[test]
    fn a_second_click_on_a_row_opens_it() {
        let recent = repo();
        let mut screen = StartScreen::new(vec![recent.path().to_path_buf()]);
        screen.hits = vec![(5, 0), (6, 1)];
        assert_eq!(screen.on_click(6), None);
        assert_eq!(screen.browser.selected, 1);
        assert_eq!(screen.on_click(5), None);
        assert_eq!(screen.browser.selected, 0);
        assert!(matches!(screen.on_click(5), Some(Choice::Open(_))));
        // A click off the list does nothing.
        assert_eq!(screen.on_click(40), None);
    }

    #[test]
    fn a_bad_typed_path_is_an_error_not_a_move() {
        let mut screen = StartScreen::new(Vec::new());
        screen.on_key(KeyCode::Tab);
        for c in "/definitely/not/here-activegit-xyz".chars() {
            screen.on_key(KeyCode::Char(c));
        }
        screen.on_key(KeyCode::Enter);
        assert!(screen.browser.error.is_some());
        assert!(screen.browser.at_places());
        assert!(screen.browser.editing_path);
    }
}
