use crate::app::{App, Mode};
use crate::config::{Config, KeyBindings, Theme};
use crate::session::Session;
use anyhow::{Context, Result};
use crossterm::event::KeyCode;
use git_tui_core::error::GitError;
use git_tui_core::jobqueue::JobQueue;
use git_tui_core::repo::Repo;
use std::path::PathBuf;

/// Mouse input, mapped from the terminal backend in `main` so this stays
/// toolkit-agnostic and unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    /// Left-press at screen cell `(col, row)`.
    Down(u16, u16),
    /// Left-drag at screen cell `(col, row)`.
    Drag(u16, u16),
    /// Left-release.
    Up,
    /// Wheel-up at screen cell `(col, row)`.
    ScrollUp(u16, u16),
    /// Wheel-down at screen cell `(col, row)`.
    ScrollDown(u16, u16),
    /// Right-press at screen cell `(col, row)`: context menu.
    RightDown(u16, u16),
}

pub struct Workspace {
    apps: Vec<App>,
    /// Canonicalized workdir roots parallel to `apps` (dedup + switching).
    roots: Vec<PathBuf>,
    keys: KeyBindings,
    config: Config,
    current: usize,
    quit: bool,
    /// Project-tab click ranges `(x_start, x_end, tab)`, recorded while
    /// rendering the project bar, plus its screen row.
    bar_hit: std::cell::RefCell<Vec<(u16, u16, usize)>>,
    bar_y: std::cell::Cell<u16>,
    /// First-run welcome overlay. Set once at startup; any dismiss key
    /// clears it and the binary persists the marker so it shows only once.
    show_welcome: bool,
    /// Where to persist the open-project session. `None` disables
    /// persistence (tests); the binary sets it to `Session::default_path()`.
    session_path: Option<PathBuf>,
    /// When Backspace last emptied the browser filter. A held Backspace
    /// must stop there instead of carrying on up the directory tree.
    filter_emptied: Option<std::time::Instant>,
}

/// How long after Backspace empties the browser filter the next Backspace
/// is still "the same hold" (and so does not go up a directory).
const FILTER_HOLD: std::time::Duration = if cfg!(test) {
    std::time::Duration::ZERO
} else {
    std::time::Duration::from_millis(400)
};

impl Workspace {
    pub fn open(paths: Vec<PathBuf>, config: Config) -> Result<Self> {
        let inputs = if paths.is_empty() {
            vec![std::env::current_dir().context("cannot read current directory")?]
        } else {
            paths
        };
        let keys = config.keys.clone();
        let mut ws = Self {
            apps: Vec::new(),
            roots: Vec::new(),
            keys,
            config,
            current: 0,
            quit: false,
            bar_hit: std::cell::RefCell::new(Vec::new()),
            bar_y: std::cell::Cell::new(0),
            show_welcome: false,
            session_path: None,
            filter_emptied: None,
        };
        for input in &inputs {
            let root = Repo::discover(input)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .workdir()
                .with_context(|| {
                    format!("bare repositories are not supported: {}", input.display())
                })?;
            // Skip duplicates instead of failing the whole launch.
            if ws
                .roots
                .iter()
                .any(|r| r == &dunce::canonicalize(&root).unwrap_or_else(|_| root.clone()))
            {
                continue;
            }
            ws.push_project(&root)?;
        }
        if ws.apps.is_empty() {
            anyhow::bail!("no repositories to open");
        }
        // Launch order: first path is the selected tab.
        ws.current = 0;
        Ok(ws)
    }

    /// Spawn one project tab for `root` (already a workdir). Switches to it.
    fn push_project(&mut self, root: &std::path::Path) -> Result<()> {
        let canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if let Some(i) = self.roots.iter().position(|r| *r == canon) {
            self.current = i;
            return Ok(());
        }
        let queue = JobQueue::spawn(root)
            .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", root.display()))?;
        let mut app = App::new_with_config(queue, self.config.clone());
        app.set_config_path(Config::default_path());
        if let Some(name) = root
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        {
            app.set_repo_name(name);
        }
        self.roots.push(canon);
        self.apps.push(app);
        self.current = self.apps.len() - 1;
        Ok(())
    }

    /// Enable session persistence (the binary sets this to
    /// `Session::default_path()` right after [`Self::open`]).
    pub fn set_session_path(&mut self, path: Option<PathBuf>) {
        self.session_path = path;
    }

    /// Snapshot the current tabs for the session file.
    pub fn snapshot_session(&self) -> Session {
        Session::new(self.roots.clone(), self.current)
    }

    /// Write the open projects + selected tab to the session file.
    /// No-op when no session path is set (tests) or there is nothing open.
    /// Errors are swallowed: persistence must never break the UI.
    pub fn save_session(&self) {
        let Some(path) = self.session_path.clone() else {
            return;
        };
        if self.roots.is_empty() {
            return;
        }
        let _ = self.snapshot_session().save_to_path(&path);
    }

    /// Select tab `index` (clamped). Used to restore the session's tab.
    pub fn set_current(&mut self, index: usize) {
        if !self.apps.is_empty() {
            self.current = index.min(self.apps.len() - 1);
            self.save_session();
            self.entered_tab();
        }
    }

    /// A tab that was in the background may be out of date: nothing
    /// refreshed it while another project was in front.
    fn entered_tab(&mut self) {
        if matches!(self.current().mode(), Mode::Normal | Mode::FullDiff) {
            self.current_mut().refresh_quietly();
        }
    }

    /// Open `path` as a project tab (Enter on a browser row).
    /// Every edge case stays inside the browser as an error except success
    /// (opens/switches and closes it) and a plain directory picked via the
    /// `.` row (moves to the `git init` confirm step).
    pub fn open_path(&mut self, path: PathBuf) {
        match Repo::discover_root(&path) {
            Ok(root) => {
                let canon = dunce::canonicalize(&root).unwrap_or_else(|_| root.clone());
                if let Some(i) = self.roots.iter().position(|r| *r == canon) {
                    self.current = i;
                    self.current_mut().finish_open_project();
                    self.save_session();
                    return;
                }
                match JobQueue::spawn(&root) {
                    Ok(queue) => {
                        let mut app = App::new_with_config(queue, self.config.clone());
                        app.set_config_path(Config::default_path());
                        let name = root
                            .file_name()
                            .and_then(|s| s.to_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| root.display().to_string());
                        app.set_repo_name(name);
                        // Close the browser on the tab it was opened from:
                        // the new tab starts clean, and switching back must
                        // not resurrect the picker.
                        let origin = self.current;
                        self.roots.push(canon);
                        self.apps.push(app);
                        self.apps[origin].finish_open_project();
                        self.current = self.apps.len() - 1;
                        self.save_session();
                    }
                    Err(e) => self
                        .current_mut()
                        .set_browser_error(format!("cannot open {}: {e}", root.display())),
                }
            }
            Err(GitError::NotARepo(_)) => {
                // Offer `git init` when the pick is a plain directory.
                if path.is_dir() {
                    self.current_mut()
                        .confirm_init_prompt(path.display().to_string());
                } else {
                    self.current_mut().set_browser_error(format!(
                        "not inside a git repository (searched upward from {})",
                        path.display()
                    ));
                }
            }
            Err(e) => {
                self.current_mut().set_browser_error(e.to_string());
            }
        }
    }

    /// Enter on the highlighted browser row: `.` opens this folder (offering
    /// `git init` when it is a plain directory), `..` goes up, a subfolder
    /// that is a repo opens as a project while a plain subfolder is browsed
    /// into. A query with no matches is an error, never a fallback to
    /// `.`/`..`.
    pub fn open_selected(&mut self) {
        let no_match = self
            .current()
            .open_browser()
            .is_some_and(|b| !b.filter.is_empty() && b.view.is_empty());
        if no_match {
            let q = self
                .current()
                .open_browser()
                .map(|b| b.filter.clone())
                .unwrap_or_default();
            self.current_mut()
                .set_browser_error(format!("no folders matching {q:?} here"));
            return;
        }
        let Some(target) = self.current().open_browser().map(|b| b.selected_path()) else {
            return;
        };
        let selected_row = self.current().open_browser().map(|b| b.selected_row());
        if matches!(selected_row, Some(crate::app::BrowserRow::Parent)) {
            self.goto_parent();
            return;
        }
        // A plain (non-repo) subfolder is browsed into so its contents stay
        // reachable; `git init` for it is one more Enter away via the `.`
        // row once inside. Anything else (repos, bare repos, files, missing
        // paths) goes through `open_path` for the tab/error handling there.
        if matches!(selected_row, Some(crate::app::BrowserRow::Dir(_)))
            && target.is_dir()
            && matches!(Repo::discover_root(&target), Err(GitError::NotARepo(_)))
        {
            if let Some(b) = self.current_mut().open_browser_mut() {
                b.goto(target);
            }
            return;
        }
        self.open_path(target);
    }

    /// Descend into the highlighted subfolder (`→`/`l`). `.`/`..` are
    /// handled by [`Self::open_selected`] instead.
    pub fn descend_selected(&mut self) {
        let Some(target) = self.current().open_browser().map(|b| b.selected_path()) else {
            return;
        };
        let is_dir_row = matches!(
            self.current().open_browser().map(|b| b.selected_row()),
            Some(crate::app::BrowserRow::Dir(_))
        );
        if !is_dir_row {
            return;
        }
        if let Some(b) = self.current_mut().open_browser_mut() {
            b.goto(target);
        }
    }

    pub fn goto_parent(&mut self) {
        let parent = self
            .current()
            .open_browser()
            .and_then(|b| b.cwd.parent().map(|p| p.to_path_buf()));
        // No parent means the filesystem root: nothing to do.
        if let Some(parent) = parent {
            if let Some(b) = self.current_mut().open_browser_mut() {
                b.goto(parent);
            }
        }
    }

    /// Go up one level regardless of the highlighted row (`←`/`h`).
    pub fn browser_up(&mut self) {
        let parent = self
            .current()
            .open_browser()
            .and_then(|b| b.cwd.parent().map(|p| p.to_path_buf()));
        if let Some(parent) = parent {
            if let Some(b) = self.current_mut().open_browser_mut() {
                b.goto(parent);
            }
        }
    }

    /// Submit the jump-to-path line (`tab` then Enter): move the browser to
    /// the typed directory instead of only browsing from the start folder.
    pub fn submit_jump_path(&mut self) {
        let draft = self.current().draft().to_string();
        let path = match Repo::normalize_project_input(&draft) {
            Ok(p) => p,
            Err(e) => {
                self.current_mut().set_browser_error(e.to_string());
                return;
            }
        };
        if !path.is_dir() {
            self.current_mut()
                .set_browser_error(format!("not a directory: {}", path.display()));
            return;
        }
        let dir = dunce::canonicalize(&path).unwrap_or(path);
        let ok = self
            .current_mut()
            .open_browser_mut()
            .map(|b| {
                b.editing_path = false;
                b.goto(dir)
            })
            .unwrap_or(false);
        if ok {
            self.current_mut().clear_draft();
        }
    }

    /// Confirm the init step (Enter in `Mode::ConfirmInit`): `git init`
    /// the prompted directory, then open it as a project.
    pub fn confirm_init_project(&mut self) {
        let draft = self.current().draft().to_string();
        let dir = match Repo::normalize_project_input(&draft) {
            Ok(p) => {
                if p.is_dir() {
                    p
                } else {
                    self.current_mut()
                        .set_error(format!("cannot init here: {}", p.display()));
                    return;
                }
            }
            Err(e) => {
                self.current_mut().set_error(e.to_string());
                return;
            }
        };
        match Repo::init(&dir) {
            Ok(_) => match Repo::discover_root(&dir) {
                Ok(root) => {
                    let canon = dunce::canonicalize(&root).unwrap_or_else(|_| root.clone());
                    if let Some(i) = self.roots.iter().position(|r| *r == canon) {
                        self.current = i;
                        self.current_mut().finish_open_project();
                        self.save_session();
                        return;
                    }
                    match JobQueue::spawn(&root) {
                        Ok(queue) => {
                            let mut app = App::new_with_config(queue, self.config.clone());
                            app.set_config_path(Config::default_path());
                            let name = root
                                .file_name()
                                .and_then(|s| s.to_str())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| root.display().to_string());
                            app.set_repo_name(name);
                            // Same as `open_path`: close the picker on the
                            // originating tab so it stays closed when
                            // switching back.
                            let origin = self.current;
                            self.roots.push(canon);
                            self.apps.push(app);
                            self.apps[origin].finish_open_project();
                            self.current = self.apps.len() - 1;
                            self.save_session();
                        }
                        Err(e) => self.current_mut().set_error(format!(
                            "initialized {}, but cannot open it: {e}",
                            dir.display()
                        )),
                    }
                }
                Err(e) => self.current_mut().set_error(format!(
                    "initialized {}, but cannot open it: {e}",
                    dir.display()
                )),
            },
            Err(e) => self
                .current_mut()
                .set_error(format!("cannot init {}: {e}", dir.display())),
        }
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn index(&self) -> usize {
        self.current
    }

    pub fn current(&self) -> &App {
        &self.apps[self.current]
    }

    pub fn current_mut(&mut self) -> &mut App {
        &mut self.apps[self.current]
    }

    pub fn project_name(&self, i: usize) -> &str {
        self.apps[i].repo_name()
    }

    pub fn project_dirty_count(&self, i: usize) -> Option<usize> {
        self.apps[i].status().map(|st| st.files.len())
    }

    /// Canonicalized workdir roots, parallel to the tabs (for the
    /// browser's `[open]` badges).
    pub fn project_roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn theme(&self) -> Theme {
        self.current().theme()
    }

    /// Recorded by the project-bar renderer for mouse clicks.
    pub fn set_bar_hit(&self, hit: Vec<(u16, u16, usize)>, y: u16) {
        *self.bar_hit.borrow_mut() = hit;
        self.bar_y.set(y);
    }

    /// Route mouse input: the welcome overlay, project tabs, and the
    /// project browser live here; everything else belongs to the
    /// current project tab.
    pub fn on_mouse(&mut self, action: MouseAction) {
        if self.show_welcome {
            if matches!(action, MouseAction::Down(..)) {
                self.dismiss_welcome();
            }
            return;
        }
        // Project tabs (multi-project bar): click switches tabs.
        if self.apps.len() > 1 {
            if let MouseAction::Down(col, row) = action {
                if row == self.bar_y.get() {
                    let tab = self
                        .bar_hit
                        .borrow()
                        .iter()
                        .find(|(s, e, _)| col >= *s && col < *e)
                        .map(|(_, _, i)| *i);
                    if let Some(i) = tab {
                        self.set_current(i);
                        return;
                    }
                }
            }
        }
        // The project browser needs workspace-level opens.
        if self.current().mode() == Mode::OpenProject {
            match action {
                MouseAction::Down(col, row) => {
                    if self.current_mut().click_popup(col, row) {
                        self.replay_click_keys();
                    } else {
                        self.click_browser(col, row);
                    }
                }
                MouseAction::ScrollUp(..) => self.on_key_browser(KeyCode::Up),
                MouseAction::ScrollDown(..) => self.on_key_browser(KeyCode::Down),
                _ => {}
            }
            return;
        }
        match action {
            MouseAction::Down(col, row) => self.current_mut().on_mouse_down(col, row),
            MouseAction::Drag(col, row) => self.current_mut().on_mouse_drag(col, row),
            MouseAction::Up => self.current_mut().on_mouse_up(),
            MouseAction::ScrollUp(col, row) => self.current_mut().on_wheel(col, row, true),
            MouseAction::ScrollDown(col, row) => self.current_mut().on_wheel(col, row, false),
            MouseAction::RightDown(col, row) => self.current_mut().on_right_click(col, row),
        }
        self.replay_click_keys();
        self.sync_theme();
        self.run_menu_request();
    }

    /// Click a project-browser row: select it, or activate it when it is
    /// already selected (mouse double-click without a timer).
    fn click_browser(&mut self, col: u16, row: u16) {
        let double = self.current_mut().note_click(col, row);
        let Some(idx) = self.current().browser_hit_at(row) else {
            return;
        };
        if idx == self.current().browser_selected_index() && double {
            self.open_selected();
        } else {
            self.current_mut().set_browser_selected(idx);
        }
    }

    pub fn next(&mut self) {
        if !self.apps.is_empty() {
            self.current = (self.current + 1) % self.apps.len();
            self.save_session();
            self.entered_tab();
        }
    }

    pub fn prev(&mut self) {
        if !self.apps.is_empty() {
            self.current = (self.current + self.apps.len() - 1) % self.apps.len();
            self.save_session();
            self.entered_tab();
        }
    }

    /// Close the current project tab (`q`). With several tabs open the tab
    /// is removed and selection moves to the next tab (previous one when the
    /// last tab closed). With a single tab, closing exits the application.
    pub fn close_current_project(&mut self) {
        if self.apps.len() <= 1 {
            self.save_session();
            self.request_quit();
            return;
        }
        self.apps.remove(self.current);
        self.roots.remove(self.current);
        if self.current >= self.apps.len() {
            self.current = self.apps.len() - 1;
        }
        self.save_session();
    }

    /// Test/legacy path with no modifiers (see [`App::on_key`]). The binary
    /// uses [`Self::on_key_with_modifiers`].
    #[allow(dead_code)]
    pub fn on_key(&mut self, key: KeyCode) {
        let shift = matches!(key, KeyCode::Char('A'));
        self.on_key_with_modifiers(key, shift);
    }

    /// Modifier-aware dispatch (Shift matters for Shift+arrows); keys
    /// pressed with Ctrl or Alt arrive through [`Self::on_chord`].
    pub fn on_key_with_modifiers(&mut self, key: KeyCode, shift_held: bool) {
        // The welcome overlay owns every key until dismissed.
        if self.show_welcome {
            self.on_key_welcome(key);
            return;
        }
        // The browser owns every key until it closes (Enter opens a repo
        // or browses into a plain folder, Esc closes). No global bindings
        // leak in.
        if self.current().mode() == Mode::OpenProject {
            self.on_key_browser(key);
            return;
        }
        if self.current().mode() == Mode::ConfirmInit {
            match key {
                // Any edit aborts the confirm and returns to the browser
                // so another folder can be picked instead.
                KeyCode::Char(_) | KeyCode::Backspace => {
                    self.current_mut().back_to_open_project();
                }
                KeyCode::Enter => self.confirm_init_project(),
                KeyCode::Esc => self.current_mut().back_to_open_project(),
                _ => {}
            }
            return;
        }
        // `q` closes the current project, `Q` (Shift+q) quits the whole app.
        // Only in the main view: text modals and the finder treat `q` as
        // literal input, and in the fullscreen diff `q` closes the diff.
        // `quit` wins when both actions share a key so a
        // `quit = ["q", "Q"]` override still quits everything.
        if self.current().mode() == Mode::Normal
            && !self.keys.quit.contains(&key)
            && self.keys.project_close.contains(&key)
        {
            self.close_current_project();
            return;
        }
        if self.current().mode() == Mode::Normal {
            if self.keys.project_open.contains(&key) {
                let start = self.roots[self.current].clone();
                self.current_mut().begin_open_project(start);
                return;
            }
            if self.apps.len() > 1 {
                if self.keys.project_next.contains(&key) {
                    self.next();
                    return;
                }
                if self.keys.project_prev.contains(&key) {
                    self.prev();
                    return;
                }
            }
        }
        self.current_mut().on_key_with_modifiers(key, shift_held);
        self.sync_theme();
        self.run_menu_request();
    }

    /// Carry out a project action picked in the actions menu (the menu
    /// lives in one tab; opening, switching and closing tabs happen here).
    fn run_menu_request(&mut self) {
        use crate::menu::WorkspaceRequest;
        let Some(request) = self.current_mut().take_workspace_request() else {
            return;
        };
        match request {
            WorkspaceRequest::Open => {
                let start = self.roots[self.current].clone();
                self.current_mut().begin_open_project(start);
            }
            WorkspaceRequest::Next => self.next(),
            WorkspaceRequest::Prev => self.prev(),
            WorkspaceRequest::Close => self.close_current_project(),
        }
    }

    /// A key pressed with Ctrl or Alt (see [`crate::input::Chord`]).
    pub fn on_chord(&mut self, chord: crate::input::Chord) {
        use crate::input::Chord;
        if self.show_welcome {
            return;
        }
        if matches!(self.current().mode(), Mode::OpenProject | Mode::ConfirmInit) {
            // The browser is workspace-owned: only the list movement
            // shortcuts mean anything there.
            match chord {
                Chord::Ctrl('n') => self.on_key_with_modifiers(KeyCode::Down, false),
                Chord::Ctrl('p') => self.on_key_with_modifiers(KeyCode::Up, false),
                _ => {}
            }
            return;
        }
        self.current_mut().on_chord(chord);
        self.sync_theme();
        self.run_menu_request();
    }

    /// Pasted text goes to the open text box as text, never as keys.
    pub fn on_paste(&mut self, text: &str) {
        if self.show_welcome {
            return;
        }
        match self.current().mode() {
            Mode::OpenProject => {
                // Filter or path line: one line of printable characters.
                for c in text.chars().filter(|c| !c.is_control()) {
                    self.on_key_browser(KeyCode::Char(c));
                }
            }
            Mode::ConfirmInit => {}
            _ => self.current_mut().on_paste(text),
        }
    }

    /// The terminal got focus back: files, branches or the index may have
    /// changed while the user was in their editor or another shell.
    pub fn on_focus_gained(&mut self) {
        if matches!(self.current().mode(), Mode::Normal | Mode::FullDiff) {
            self.current_mut().refresh_quietly();
        }
    }

    /// `[ui] mouse`.
    pub fn mouse_enabled(&self) -> bool {
        self.config.ui.mouse
    }

    /// Replay keys that clicks produced for the modes handled here.
    fn replay_click_keys(&mut self) {
        for key in self.current_mut().take_workspace_keys() {
            self.on_key_with_modifiers(key, false);
        }
    }

    /// The theme picker runs inside one project tab but the theme is
    /// global: copy the current tab's theme to every tab and to the config
    /// that new tabs are opened with.
    fn sync_theme(&mut self) {
        let theme = self.current().theme();
        if self.config.theme != theme {
            self.config.theme = theme;
            for app in &mut self.apps {
                app.set_theme(theme);
            }
        }
        // Same for LLM settings saved from the setup form: every tab, and
        // tabs opened later, use them without a restart.
        if self.config.llm != *self.current().llm() {
            self.config.llm = self.current().llm().clone();
            let llm = self.config.llm.clone();
            for app in &mut self.apps {
                app.set_llm(llm.clone());
            }
        }
    }

    /// Keys inside the project browser. Typing filters the current
    /// folder's list by default (no prefix key); everything else lives on
    /// non-printable keys so search text never triggers actions.
    fn on_key_browser(&mut self, key: KeyCode) {
        let editing = self
            .current()
            .open_browser()
            .is_some_and(|b| b.editing_path);
        if editing {
            match key {
                KeyCode::Char(c) => self.current_mut().push_draft_char(c),
                KeyCode::Backspace => self.current_mut().pop_draft_char(),
                KeyCode::Delete => self.current_mut().delete_draft_after(),
                KeyCode::Left => self.current_mut().move_draft_left(),
                KeyCode::Right => self.current_mut().move_draft_right(),
                KeyCode::Home => self.current_mut().move_draft_home(),
                KeyCode::End => self.current_mut().move_draft_end(),
                KeyCode::Enter => self.submit_jump_path(),
                KeyCode::Esc => {
                    if let Some(b) = self.current_mut().open_browser_mut() {
                        b.editing_path = false;
                    }
                    self.current_mut().clear_draft();
                }
                _ => {}
            }
            return;
        }
        match key {
            KeyCode::Char(c) => {
                if let Some(b) = self.current_mut().open_browser_mut() {
                    b.push_filter_char(c);
                }
            }
            // Edit the query; an empty query + Backspace goes up a level,
            // unless this is still the hold that emptied the query.
            KeyCode::Backspace => {
                let empty = self
                    .current()
                    .open_browser()
                    .is_some_and(|b| b.filter.is_empty());
                if empty {
                    let same_hold = self
                        .filter_emptied
                        .is_some_and(|t| t.elapsed() < FILTER_HOLD);
                    if same_hold {
                        self.filter_emptied = Some(std::time::Instant::now());
                    } else {
                        self.browser_up();
                    }
                } else if let Some(b) = self.current_mut().open_browser_mut() {
                    b.pop_filter_char();
                    if b.filter.is_empty() {
                        self.filter_emptied = Some(std::time::Instant::now());
                    }
                }
            }
            KeyCode::Up => {
                if let Some(b) = self.current_mut().open_browser_mut() {
                    b.move_cursor(-1);
                }
            }
            KeyCode::Down => {
                if let Some(b) = self.current_mut().open_browser_mut() {
                    b.move_cursor(1);
                }
            }
            KeyCode::Enter => self.open_selected(),
            KeyCode::Right => self.descend_selected(),
            KeyCode::Left => self.browser_up(),
            // Jump to a typed path instead of clicking through.
            KeyCode::Tab => {
                if let Some(b) = self.current_mut().open_browser_mut() {
                    b.editing_path = true;
                }
                self.current_mut().clear_draft();
            }
            // Clear the query first; close only with nothing to clear.
            KeyCode::Esc => {
                let has_filter = self
                    .current()
                    .open_browser()
                    .is_some_and(|b| !b.filter.is_empty());
                if has_filter {
                    if let Some(b) = self.current_mut().open_browser_mut() {
                        b.clear_filter();
                    }
                } else {
                    self.current_mut().cancel_open_project();
                }
            }
            _ => {}
        }
    }

    /// Drain every project's finished jobs; returns whether any landed
    /// (the screen then needs a repaint).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for app in &mut self.apps {
            changed |= app.poll();
        }
        changed
    }

    /// Whether any project still waits on its worker.
    pub fn busy(&self) -> bool {
        self.apps.iter().any(App::busy)
    }

    pub fn request_quit(&mut self) {
        self.save_session();
        self.quit = true;
        for app in &mut self.apps {
            app.request_quit();
        }
    }

    pub fn should_quit(&self) -> bool {
        self.quit || self.apps.iter().any(App::should_quit)
    }

    /// Whether the first-run welcome overlay is up.
    pub fn welcome_visible(&self) -> bool {
        self.show_welcome
    }

    /// Show or hide the first-run welcome overlay.
    pub fn set_show_welcome(&mut self, show: bool) {
        self.show_welcome = show;
    }

    /// Dismiss the welcome overlay (persistence is handled by the caller).
    pub fn dismiss_welcome(&mut self) {
        self.show_welcome = false;
    }

    /// Keys while the welcome overlay is up: it owns every key until
    /// dismissed. Enter/Esc/space/q start; `o` starts and opens the
    /// project browser; anything else is ignored.
    fn on_key_welcome(&mut self, key: KeyCode) {
        match key {
            KeyCode::Enter
            | KeyCode::Esc
            | KeyCode::Char(' ')
            | KeyCode::Char('q')
            | KeyCode::Char('Q') => self.dismiss_welcome(),
            KeyCode::Char('o') | KeyCode::Char('O') => {
                self.dismiss_welcome();
                let start = self.roots[self.current].clone();
                self.current_mut().begin_open_project(start);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo_with_file(name: &str, path: &str, contents: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test User").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        std::fs::write(dir.path().join(path), contents).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(path)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = repo.signature().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, name, &tree, &[])
            .unwrap();
        dir
    }

    fn type_text(ws: &mut Workspace, text: &str) {
        for c in text.chars() {
            ws.on_key(KeyCode::Char(c));
        }
    }

    /// Drive the UI the way a user would: `o`, `tab`, type the path,
    /// Enter (jump there), Enter (open the folder via `.`).
    fn ui_open_path(ws: &mut Workspace, path: &std::path::Path) {
        ws.on_key(KeyCode::Char('o'));
        ws.on_key(KeyCode::Tab);
        type_text(ws, path.to_str().unwrap());
        ws.on_key(KeyCode::Enter);
        ws.on_key(KeyCode::Enter);
    }

    /// Jump the browser via the `tab` editor (mirrors `ui_open_path`
    /// without opening anything afterwards).
    fn ui_jump_to(ws: &mut Workspace, path: &std::path::Path) {
        ws.on_key(KeyCode::Char('o'));
        ws.on_key(KeyCode::Tab);
        type_text(ws, path.to_str().unwrap());
        ws.on_key(KeyCode::Enter);
    }

    fn browser_error(ws: &Workspace) -> String {
        ws.current()
            .open_browser()
            .and_then(|b| b.error.clone())
            .unwrap_or_default()
    }

    #[test]
    fn opens_each_repo_with_isolated_state() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        assert_eq!(ws.len(), 2);
        let base =
            |d: &tempfile::TempDir| d.path().file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(ws.current().repo_name(), base(&a));
        assert_eq!(ws.project_name(1), base(&b));
    }

    #[test]
    fn switching_wraps_around() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        ws.next();
        assert_eq!(ws.index(), 1);
        ws.next();
        assert_eq!(ws.index(), 0);
        ws.prev();
        assert_eq!(ws.index(), 1);
    }

    #[test]
    fn duplicate_paths_open_only_once() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let ws = Workspace::open(
            vec![a.path().to_path_buf(), a.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        assert_eq!(ws.len(), 1);
    }

    #[test]
    fn invalid_path_is_an_error() {
        let missing = std::env::temp_dir().join("git-tui-definitely-not-a-repo-xyz");
        assert!(Workspace::open(vec![missing], Config::default()).is_err());
    }

    #[test]
    fn project_keys_cycle_only_from_normal_mode() {
        use crossterm::event::KeyCode;
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        ws.on_key(KeyCode::Char(']'));
        assert_eq!(ws.index(), 1);
        ws.current_mut().on_key(KeyCode::Char('c'));
        ws.on_key(KeyCode::Char(']'));
        assert_eq!(ws.index(), 1, "commit draft must win over project switch");
    }

    #[test]
    fn open_key_shows_browser_at_current_root() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('o'));
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        let browser = ws.current().open_browser().expect("browser is open");
        assert_eq!(
            browser.cwd,
            dunce::canonicalize(a.path()).unwrap(),
            "browser starts at the current project so siblings are nearby"
        );
        assert!(!browser.editing_path);
    }

    #[test]
    fn open_project_from_app_adds_and_selects_tab() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_open_path(&mut ws, b.path());
        assert_eq!(ws.len(), 2);
        assert_eq!(ws.index(), 1);
        assert_eq!(ws.current().mode(), Mode::Normal);
        // The picker closed on the tab it was opened from too: switching
        // back must land on a clean Normal mode, not the open browser.
        ws.prev();
        assert_eq!(ws.index(), 0);
        assert_eq!(ws.current().mode(), Mode::Normal);
        assert!(ws.current().open_browser().is_none());
    }

    #[test]
    fn open_duplicate_switches_without_new_tab() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        assert_eq!(ws.index(), 0);
        ws.next();
        assert_eq!(ws.index(), 1);
        ui_open_path(&mut ws, a.path());
        assert_eq!(ws.len(), 2, "duplicate must not add a tab");
        assert_eq!(ws.index(), 0);
    }

    #[test]
    fn enter_on_dot_opens_current_folder() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('o'));
        // `.` is pre-selected: Enter opens the browsed folder itself,
        // which here is already open, so it just closes the browser.
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::Normal);
        assert_eq!(ws.len(), 1);
    }

    #[test]
    fn browser_navigates_down_up_and_marks_repo_roots() {
        let parent = tempfile::TempDir::new().unwrap();
        let repo_dir = parent.path().join("aaa-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        git2::Repository::init(&repo_dir).unwrap();
        let plain_dir = parent.path().join("zzz-plain");
        std::fs::create_dir_all(&plain_dir).unwrap();
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_jump_to(&mut ws, parent.path());
        {
            let b = ws.current().open_browser().unwrap();
            let names: Vec<&str> = b.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["aaa-repo", "zzz-plain"]);
            assert!(b.entries[0].is_repo_root);
            assert!(!b.entries[1].is_repo_root);
        }
        // Down past `.`/`..` onto the repo dir, then descend into it.
        ws.on_key(KeyCode::Down);
        ws.on_key(KeyCode::Down);
        assert!(ws
            .current()
            .open_browser()
            .unwrap()
            .selected_path()
            .ends_with("aaa-repo"));
        ws.on_key(KeyCode::Right);
        assert_eq!(
            ws.current().open_browser().unwrap().cwd,
            dunce::canonicalize(&repo_dir).unwrap()
        );
        // Back up to the parent.
        ws.on_key(KeyCode::Left);
        assert_eq!(
            ws.current().open_browser().unwrap().cwd,
            dunce::canonicalize(parent.path()).unwrap()
        );
    }

    #[test]
    fn jump_to_tilde_via_tab_editor() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('o'));
        ws.on_key(KeyCode::Tab);
        assert!(ws.current().open_browser().unwrap().editing_path);
        type_text(&mut ws, "~");
        ws.on_key(KeyCode::Enter);
        let home = git_tui_core::repo::home_dir().unwrap();
        assert_eq!(
            ws.current().open_browser().unwrap().cwd,
            dunce::canonicalize(&home).unwrap()
        );
    }

    #[test]
    fn typing_filters_this_folder_and_enter_opens_match() {
        let parent = tempfile::TempDir::new().unwrap();
        for name in ["alpha-proj", "beta-proj", "gamma-other"] {
            let dir = parent.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            git2::Repository::init(&dir).unwrap();
        }
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_jump_to(&mut ws, parent.path());
        // No prefix key: typing narrows the listing to this folder's matches.
        type_text(&mut ws, "proj");
        {
            let b = ws.current().open_browser().unwrap();
            assert_eq!(b.filter, "proj");
            let names: Vec<&str> = b.view.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["alpha-proj", "beta-proj"]);
            // Fresh filter jumps the highlight to the first match.
            assert!(b.selected_path().ends_with("alpha-proj"));
        }
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws.current().mode(), Mode::Normal);
    }

    #[test]
    fn filter_no_match_stays_with_error_and_esc_clears() {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(parent.path().join("alpha")).unwrap();
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_jump_to(&mut ws, parent.path());
        type_text(&mut ws, "zzz-nope");
        assert!(ws.current().open_browser().unwrap().view.is_empty());
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        assert!(
            browser_error(&ws).contains("no folders matching"),
            "got: {}",
            browser_error(&ws)
        );
        // Esc clears the filter and restores the full listing.
        ws.on_key(KeyCode::Esc);
        let b = ws.current().open_browser().unwrap();
        assert!(b.filter.is_empty());
        assert_eq!(b.view.len(), b.entries.len());
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        // Second Esc closes the browser.
        ws.on_key(KeyCode::Esc);
        assert_eq!(ws.current().mode(), Mode::Normal);
    }

    #[test]
    fn filter_backspace_edits_then_goes_up_when_empty() {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(parent.path().join("alpha")).unwrap();
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_jump_to(&mut ws, parent.path());
        type_text(&mut ws, "ab");
        assert_eq!(ws.current().open_browser().unwrap().filter, "ab");
        ws.on_key(KeyCode::Backspace);
        assert_eq!(ws.current().open_browser().unwrap().filter, "a");
        ws.on_key(KeyCode::Backspace);
        {
            let b = ws.current().open_browser().unwrap();
            assert!(b.filter.is_empty());
            assert_eq!(b.view.len(), b.entries.len());
        }
        // Empty filter + Backspace goes up a level (old Backspace behavior).
        let expected = parent.path().parent().map(|p| p.to_path_buf()).unwrap();
        ws.on_key(KeyCode::Backspace);
        assert_eq!(
            ws.current().open_browser().unwrap().cwd,
            dunce::canonicalize(&expected).unwrap()
        );
    }

    #[test]
    fn jump_editor_edits_mid_text_with_cursor() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('o'));
        ws.on_key(KeyCode::Tab);
        type_text(&mut ws, "ab");
        ws.on_key(KeyCode::Left);
        ws.on_key(KeyCode::Backspace);
        assert_eq!(ws.current().draft(), "b");
        ws.on_key(KeyCode::Home);
        type_text(&mut ws, "x");
        assert_eq!(ws.current().draft(), "xb");
        ws.on_key(KeyCode::Esc);
        assert!(!ws.current().open_browser().unwrap().editing_path);
    }

    #[test]
    fn jump_to_file_is_an_error() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('o'));
        ws.on_key(KeyCode::Tab);
        type_text(&mut ws, a.path().join("a.txt").to_str().unwrap());
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        assert!(
            browser_error(&ws).contains("not a directory"),
            "got: {}",
            browser_error(&ws)
        );
    }

    #[test]
    fn open_empty_input_stays_in_modal_with_error() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('o'));
        ws.on_key(KeyCode::Tab);
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        assert!(!browser_error(&ws).is_empty());
        assert_eq!(ws.len(), 1);
    }

    #[test]
    fn open_missing_path_stays_in_modal_with_error() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('o'));
        ws.on_key(KeyCode::Tab);
        type_text(&mut ws, "/definitely/not/here-git-tui-xyz");
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        assert!(
            browser_error(&ws).contains("no such path"),
            "got: {}",
            browser_error(&ws)
        );
    }

    #[test]
    fn open_bare_repo_stays_in_modal_with_error() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let bare_parent = tempfile::TempDir::new().unwrap();
        let bare = bare_parent.path().join("bare.git");
        git2::Repository::init_bare(&bare).unwrap();
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_open_path(&mut ws, &bare);
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        assert!(
            browser_error(&ws).contains("bare"),
            "got: {}",
            browser_error(&ws)
        );
    }

    #[test]
    fn open_non_repo_dir_offers_init_then_opens() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let plain = tempfile::TempDir::new().unwrap();
        std::fs::write(plain.path().join("notes.txt"), "hello\n").unwrap();
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_open_path(&mut ws, plain.path());
        assert_eq!(ws.current().mode(), Mode::ConfirmInit);
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws.current().mode(), Mode::Normal);
        assert!(plain.path().join(".git").exists());
        // Same as a plain open: the originating tab's picker is closed.
        ws.prev();
        assert_eq!(ws.current().mode(), Mode::Normal);
        assert!(ws.current().open_browser().is_none());
    }

    #[test]
    fn enter_on_plain_subdir_browses_inside_and_dot_still_offers_init() {
        let parent = tempfile::TempDir::new().unwrap();
        let plain = parent.path().join("plain");
        std::fs::create_dir_all(plain.join("inner")).unwrap();
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_jump_to(&mut ws, parent.path());
        // Past `.`/`..` onto `plain`: Enter browses into it (no init prompt).
        ws.on_key(KeyCode::Down);
        ws.on_key(KeyCode::Down);
        assert!(ws
            .current()
            .open_browser()
            .unwrap()
            .selected_path()
            .ends_with("plain"));
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        assert_eq!(
            ws.current().open_browser().unwrap().cwd,
            dunce::canonicalize(&plain).unwrap()
        );
        // `.` is highlighted inside the plain folder: Enter offers `git init`.
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::ConfirmInit);
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws.current().mode(), Mode::Normal);
        assert!(plain.join(".git").exists());
    }

    #[test]
    fn enter_on_bare_subdir_still_errors_instead_of_descending() {
        let parent = tempfile::TempDir::new().unwrap();
        let bare = parent.path().join("bare.git");
        git2::Repository::init_bare(&bare).unwrap();
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_jump_to(&mut ws, parent.path());
        ws.on_key(KeyCode::Down);
        ws.on_key(KeyCode::Down);
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        assert!(
            browser_error(&ws).contains("bare"),
            "got: {}",
            browser_error(&ws)
        );
    }

    #[test]
    fn open_empty_repo_works_with_no_files() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        // Fresh `git init`, no commits, no files: the empty project.
        let empty = tempfile::TempDir::new().unwrap();
        git2::Repository::init(empty.path()).unwrap();
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ui_open_path(&mut ws, empty.path());
        assert_eq!(ws.len(), 2);
        assert_eq!(ws.current().mode(), Mode::Normal);
    }

    #[test]
    fn esc_clears_filter_then_closes_and_keys_do_not_leak() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        ws.on_key(KeyCode::Char('o'));
        // `[`, `]` and `q` are filter text now, not actions.
        ws.on_key(KeyCode::Char('['));
        assert_eq!(
            ws.index(),
            0,
            "project_prev must not fire inside the browser"
        );
        ws.on_key(KeyCode::Char('q'));
        assert!(!ws.should_quit(), "quit must not fire inside the browser");
        assert_eq!(ws.current().open_browser().unwrap().filter, "[q");
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        // First Esc clears the query, second closes the browser.
        ws.on_key(KeyCode::Esc);
        assert!(ws.current().open_browser().unwrap().filter.is_empty());
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        ws.on_key(KeyCode::Esc);
        assert_eq!(ws.current().mode(), Mode::Normal);
        assert_eq!(ws.len(), 2);
    }

    #[test]
    fn q_closes_current_project_and_keeps_selection() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        assert_eq!(ws.len(), 2);
        assert_eq!(ws.index(), 0);
        ws.on_key(KeyCode::Char('q'));
        assert!(!ws.should_quit(), "closing one of two tabs must not quit");
        assert_eq!(ws.len(), 1);
        assert_eq!(ws.index(), 0);
    }

    #[test]
    fn q_on_last_tab_quits_and_shift_q_quits_everything() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        ws.on_key(KeyCode::Char('Q'));
        assert!(ws.should_quit(), "Shift+Q must quit the whole app");

        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('q'));
        assert!(
            ws.should_quit(),
            "closing the last tab must quit the application"
        );
    }

    #[test]
    fn q_in_fullscreen_closes_the_diff_and_shift_q_quits() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        // Wait for the file tree so Enter can open fullscreen.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            ws.poll();
            if ws.current().has_files() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for files"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::FullDiff);
        ws.on_key(KeyCode::Char('q'));
        assert_eq!(
            ws.len(),
            2,
            "q in the diff closes the diff, not the project"
        );
        assert_eq!(ws.current().mode(), Mode::Normal);
        assert!(!ws.should_quit());
        // From the main view it closes the project as before.
        ws.on_key(KeyCode::Char('q'));
        assert_eq!(ws.len(), 1);
        assert!(!ws.should_quit());

        // Reopen to two tabs and verify Q quits from fullscreen.
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            ws.poll();
            if ws.current().has_files() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for files"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::FullDiff);
        ws.on_key(KeyCode::Char('Q'));
        assert!(ws.should_quit(), "Q fullscreen must quit the app");
    }

    #[test]
    fn project_tab_click_switches_tabs() {
        use crate::ui::render_workspace;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        assert_eq!(ws.index(), 0);
        // Render once so the tab click ranges are recorded.
        let backend = TestBackend::new(170, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_workspace(f, &ws)).unwrap();
        // Click inside the second tab's recorded range.
        let (x0, x1, i) = ws.bar_hit.borrow()[1];
        assert_eq!(i, 1);
        ws.on_mouse(MouseAction::Down((x0 + x1) / 2, ws.bar_y.get()));
        assert_eq!(ws.index(), 1);
        // Clicking empty bar space switches nothing.
        ws.on_mouse(MouseAction::Down(169, ws.bar_y.get()));
        assert_eq!(ws.index(), 1);
    }

    #[test]
    fn welcome_click_dismisses_overlay() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.set_show_welcome(true);
        ws.on_mouse(MouseAction::Down(5, 5));
        assert!(!ws.welcome_visible());
        assert!(!ws.should_quit());
    }

    #[test]
    fn snapshot_captures_roots_and_selection() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        ws.next();
        let snap = ws.snapshot_session();
        assert_eq!(snap.projects.len(), 2);
        assert_eq!(snap.current, 1);
    }

    #[test]
    fn save_session_writes_file_and_tab_switch_updates_it() {
        use crate::session::Session;
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let dir = tempfile::TempDir::new().unwrap();
        let session_file = dir.path().join("session.toml");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        ws.set_session_path(Some(session_file.clone()));
        ws.save_session();
        let loaded = Session::load_from_path(&session_file).unwrap();
        assert_eq!(loaded.projects.len(), 2);
        assert_eq!(loaded.current, 0);
        // Switching tabs auto-persists the new selection.
        ws.next();
        let reloaded = Session::load_from_path(&session_file).unwrap();
        assert_eq!(reloaded.current, 1);
        // Closing a tab auto-persists the shorter list.
        ws.close_current_project();
        let closed = Session::load_from_path(&session_file).unwrap();
        assert_eq!(closed.projects.len(), 1);
    }

    #[test]
    fn save_session_without_path_is_a_noop() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        // No session path set (tests): must not panic or touch the disk.
        ws.save_session();
    }

    #[test]
    fn welcome_is_hidden_by_default() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        assert!(!ws.welcome_visible());
    }

    #[test]
    fn welcome_dismiss_keys_close_it_without_quitting() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        for key in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Char(' '),
            KeyCode::Char('q'),
            KeyCode::Char('Q'),
        ] {
            let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
            ws.set_show_welcome(true);
            ws.on_key(key);
            assert!(!ws.welcome_visible(), "key {key:?} did not dismiss");
            assert!(!ws.should_quit(), "key {key:?} quit instead of dismiss");
            assert_eq!(ws.len(), 1, "key {key:?} closed the project");
        }
    }

    #[test]
    fn welcome_ignores_other_keys() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.set_show_welcome(true);
        ws.on_key(KeyCode::Char('j'));
        assert!(ws.welcome_visible());
        assert!(!ws.should_quit());
    }

    #[test]
    fn welcome_o_opens_the_project_browser() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.set_show_welcome(true);
        ws.on_key(KeyCode::Char('o'));
        assert!(!ws.welcome_visible());
        assert_eq!(ws.current().mode(), Mode::OpenProject);
    }

    #[test]
    fn theme_picker_preview_applies_to_every_project_tab() {
        let a = init_repo_with_file(
            "a", "a.txt", "a
",
        );
        let b = init_repo_with_file(
            "b", "b.txt", "b
",
        );
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        let before = ws.theme();
        ws.on_key(KeyCode::Char('T'));
        ws.on_key(KeyCode::Down);
        let picked = ws.theme();
        assert_ne!(picked, before);
        assert!(ws.apps.iter().all(|app| app.theme() == picked));
        // Esc restores every tab, not just the one showing the picker.
        ws.on_key(KeyCode::Esc);
        assert!(ws.apps.iter().all(|app| app.theme() == before));
    }

    #[test]
    fn actions_menu_can_open_the_project_browser() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('?'));
        assert_eq!(ws.current().mode(), Mode::ActionMenu);
        type_text(&mut ws, "open project");
        ws.on_key(KeyCode::Enter);
        assert_eq!(ws.current().mode(), Mode::OpenProject);
    }

    #[test]
    fn typing_q_in_the_actions_menu_does_not_close_the_project() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        ws.on_key(KeyCode::Char('?'));
        type_text(&mut ws, "q]o");
        assert_eq!(ws.apps.len(), 2);
        assert_eq!(ws.current().mode(), Mode::ActionMenu);
        assert_eq!(ws.current().draft(), "q]o");
    }

    #[test]
    fn paste_and_chords_in_the_browser_are_text_or_nothing() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_key(KeyCode::Char('o'));
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        ws.on_paste("zz\nq");
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        assert_eq!(ws.current().open_browser().unwrap().filter, "zzq");
        ws.on_chord(crate::input::Chord::Ctrl('d'));
        ws.on_chord(crate::input::Chord::Ctrl('q'));
        assert_eq!(ws.current().mode(), Mode::OpenProject);
        assert!(!ws.should_quit());
    }

    #[test]
    fn paste_in_the_main_view_runs_nothing() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let mut ws = Workspace::open(vec![a.path().to_path_buf()], Config::default()).unwrap();
        ws.on_paste("qQo]\n");
        assert_eq!(ws.len(), 1);
        assert!(!ws.should_quit());
        assert_eq!(ws.current().mode(), Mode::Normal);
    }

    #[test]
    fn llm_settings_saved_in_one_tab_reach_the_others() {
        let a = init_repo_with_file("a", "a.txt", "a\n");
        let b = init_repo_with_file("b", "b.txt", "b\n");
        let mut ws = Workspace::open(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        let mut llm = ws.current().llm().clone();
        llm.provider = "ollama".into();
        llm.model = "llama3".into();
        ws.current_mut().set_llm(llm.clone());
        // Any key runs the sync, as saving the form does.
        ws.on_key(KeyCode::Esc);
        assert!(ws.apps.iter().all(|app| *app.llm() == llm));
        assert_eq!(ws.config.llm, llm, "tabs opened later get it too");
    }
}
