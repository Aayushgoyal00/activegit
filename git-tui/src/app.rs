//! Phase 3: status panel state machine.
//!
//! Pure logic: selection, stage toggle, commit modal. Rendering lives in
//! `ui.rs`; the event loop lives in `main.rs`.

use crossterm::event::KeyCode;
use git_tui_core::branch::BranchInfo;
use git_tui_core::diff::FileDiff;
use git_tui_core::error::GitError;
use git_tui_core::jobqueue::{AsyncJob, AsyncResult, JobId, JobQueue};
use git_tui_core::llm::LlmConfig;
use git_tui_core::log::{CommitInfo, CommitOverview};
use git_tui_core::repo::RepoState;
use git_tui_core::stash::StashEntry;
use git_tui_core::status::{FileState, RepoStatus, StatusEntry};
use git_tui_core::sync::SyncStatus;
use ratatui::layout::Rect;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::{Config, KeyBindings, Theme};
use crate::fuzzy;
use crate::input::Chord;
use crate::menu::{MenuAction, MenuContext, MenuEntry, WorkspaceRequest};
use crate::ui::{
    cursor_line_text, diff_rows, footer_button_at, footer_buttons, panel_divider_ys,
    rail_divider_x, visible_file_rows, wrap_draft, DiffRow, FileRow, FooterAction, LayoutOverrides,
    ScreenLayout,
};

/// Labels for the LLM setup form rows: provider, model, API key, base URL.
pub const LLM_FIELD_LABELS: [&str; 4] = [
    "Provider",
    "Model",
    "API key (empty = use env var)",
    "Base URL (optional, custom only)",
];

/// Row index of the two option pickers (provider, model) in the setup form.
/// `←/→` cycle these rows through their option list; typing still works, so
/// a custom id can be entered on either. Rows 2 and 3 are free text.
pub const LLM_PICKER_ROWS: [usize; 2] = [0, 1];

/// Input mode: normal list navigation, a fullscreen diff overlay, or a
/// text-input modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    FullDiff,
    Committing,
    NewBranch,
    StashPush,
    FindFile,
    OpenProject,
    ConfirmInit,
    /// Push with no upstream yet: the draft names the remote to push `-u` to.
    SetUpstream,
    /// Confirm a push to the tracked upstream (`P` with an upstream set):
    /// Enter pushes, Esc cancels. The `(remote, branch)` pair waits here.
    ConfirmPush,
    /// Publish a repo with no remotes: the draft is the new `origin` URL.
    SetRemote,
    /// In-TUI LLM provider setup (`A` in the file list): provider, model,
    /// API key, and optional base URL. Enter saves to the config file.
    LlmSettings,
    /// In-TUI theme picker (`T`): moving the selection previews the theme
    /// live, Enter keeps it and saves `[theme]`, Esc restores the old one.
    ThemePicker,
    /// Actions menu (`?`): a searchable list of every action. The draft is
    /// the filter; Enter runs the highlighted entry.
    ActionMenu,
    /// Yes/no box before an action that throws work away (see
    /// [`Danger`]). Enter or `y` runs it, Esc or `n` backs out.
    Confirm,
    /// Every key on one scrollable screen (`F1`).
    Help,
    /// Typing a search for the open diff (`Ctrl+F`); Enter jumps to the
    /// first match, then `n` / `N` step through them.
    SearchDiff,
}

/// What kind of job a pending result belongs to, so a failure can be
/// handled where it matters (a failed commit gives the message back, a
/// failed push stops "pushing…") instead of every error clearing
/// everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Commit,
    /// Changes the repository: stage, branch, stash.
    Mutation,
    /// A discard: like any mutation, and it can be undone.
    Discard,
    /// Push, pull, publish.
    Sync,
    Generate,
    Other,
}

/// A request to throw work away, as the user expressed it. It is resolved
/// against the current selection into a concrete [`DangerAction`] before
/// anything is confirmed, so the box names exactly what will be affected
/// and a list that changes underneath cannot redirect it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Danger {
    /// The selected file, or everything under the selected folder.
    DiscardSelected,
    /// The file open in the fullscreen diff.
    DiscardLoaded,
    RestoreHunk,
    DeleteBranch,
    DropStash,
    ForcePush,
    AbortOperation,
}

/// A commit opened from the commits panel: its files are stepped through
/// in the fullscreen diff, read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitView {
    pub(crate) oid: String,
    /// Short id for the title.
    pub(crate) id: String,
    pub(crate) files: Vec<String>,
    pub(crate) index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DangerAction {
    DiscardFiles(Vec<String>),
    RestoreHunk {
        path: String,
        hunk_index: usize,
        staged: bool,
    },
    DeleteBranch(String),
    DropStash(usize),
    ForcePush {
        remote: String,
        branch: String,
    },
    AbortOperation,
}

struct Confirm {
    action: DangerAction,
    prompt: String,
    /// Mode to return to (the main view or the fullscreen diff).
    back: Mode,
    opened: Instant,
}

/// How long a confirm box ignores Enter after opening. A paste or a key
/// burst that happens to contain `d` and a newline must not confirm the
/// box it just opened; a person needs longer than this to read it anyway.
const CONFIRM_ARM: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_millis(250)
};

/// What a click on a button drawn inside a popup does: the same as the key
/// (or chord) printed next to it, so mouse and keyboard share one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiClick {
    Key(KeyCode),
    Chord(Chord),
}

/// Which panel receives navigation keys. Everything is vertical: Tab cycles
/// the rail panels plus the right-side diff preview; Enter opens the
/// selected file fullscreen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Status,
    Branches,
    Log,
    Stash,
    Diff,
}

/// UI state. Owns the [`JobQueue`]; git state itself lives on the worker
/// thread and is mirrored here as snapshots (status, diff, branches).
pub struct App {
    queue: JobQueue,
    keys: KeyBindings,
    theme: Theme,
    /// LLM provider config for Ctrl+G commit generation.
    llm: LlmConfig,
    /// A GenerateCommitMessage job is in flight ("generating…" in the
    /// commit modal). Set on Ctrl+G, cleared when the message or an
    /// error arrives via `poll`.
    generating: bool,
    /// One-line success confirmation (e.g. "LLM settings saved"), shown
    /// above the footer until the next keypress. Like `error` but green.
    notice: Option<String>,
    /// Where the config file lives (for the LLM setup form to persist).
    /// `None` in tests: saving still updates the session, minus the file.
    config_path: Option<std::path::PathBuf>,
    /// LLM setup form buffers: provider, model, api_key, base_url.
    llm_form: [String; 4],
    /// Saved cursor (chars) per form field while switching rows.
    llm_cursors: [usize; 4],
    /// Highlighted form row.
    llm_selected: usize,
    /// Model ids the provider returned for the form's current provider,
    /// fetched over HTTP (nothing is hardcoded per provider). Empty while
    /// loading or when the fetch failed, which leaves the model row as
    /// free text.
    llm_models: Vec<String>,
    /// A FetchModels job is in flight (the model row shows "loading…").
    llm_models_loading: bool,
    /// Why the last model fetch failed, shown inline in the form.
    llm_models_error: Option<String>,
    /// Identity of the provider/key/base-URL the in-flight (or last)
    /// fetch belongs to, so an edit to one of those rows re-asks.
    llm_models_target: String,
    /// Id of the fetch the form is waiting on, bumped per request so a
    /// late answer for an abandoned config is dropped instead of filling
    /// the row with another provider's models.
    llm_models_request: u64,
    /// Short repo name for the status panel (workdir basename).
    repo_name: String,
    status: Option<RepoStatus>,
    /// Browsable tree: changed files in status order, then clean tracked
    /// files alphabetically. Selection indexes into this, not `status.files`.
    file_list: Vec<StatusEntry>,
    selected: usize,
    mode: Mode,
    focus: Focus,
    draft: String,
    /// Cursor inside `draft` as a char index (commit/branch/stash modals,
    /// the finder query, the jump-to-path line). Byte math is derived on
    /// demand so direct `draft` writes elsewhere can't break it: readers
    /// clamp to the current char count.
    draft_cursor: usize,
    /// Cursor inside the fuzzy file finder (`Mode::FindFile`).
    finder_selected: usize,
    /// Where the finder returns on Enter/Esc: the mode it was opened
    /// from (`Normal` or `FullDiff`), so `/` works fullscreen too.
    finder_return: Mode,
    /// Where the push/publish modals return on Enter/Esc (`Normal` or
    /// `FullDiff`), so `P` works fullscreen too.
    sync_return: Mode,
    /// Push awaiting confirmation (`Mode::ConfirmPush`): `(remote, branch)`
    /// to push with `git push` on Enter.
    pending_push: Option<(String, String)>,
    /// Screen cell where the confirm-push modal's `[Push] [Cancel]`
    /// buttons start (recorded while rendering, for mouse clicks).
    confirm_btn: Cell<Option<(u16, u16)>>,
    error: Option<String>,
    quit: bool,
    diff: Option<FileDiff>,
    /// Side-by-side rows paired from `diff`, computed once when the diff
    /// arrives. Frames and scroll steps read this instead of rerunning the
    /// word diffs over the whole diff every time.
    diff_rows: Vec<DiffRow>,
    /// (path, staged) the loaded/loading diff belongs to.
    diff_for: Option<(String, bool)>,
    /// The loaded diff is a whole-file view (clean file), not a real diff.
    diff_whole_file: bool,
    /// The whole-file view came from the empty-diff fallback (a listed file
    /// whose diff has no hunks, e.g. a mode-only change). Reset whenever the
    /// selected path changes.
    fallback_whole_file: bool,
    hunk: usize,
    /// Line cursor: index into `diff_rows` (both the fullscreen view and
    /// the right-side preview). `j/k`/`↑`/`↓` move it one row, `J`/`K`
    /// jump by hunk, `PgUp`/`PgDn` move it by 10. Nvim-style: the cursor
    /// walks inside the viewport first and the view only scrolls at the
    /// edge (see `ensure_cursor_visible`); transitions (open/close,
    /// reload) top-pin via `snap_scroll_to_cursor` instead.
    cursor: usize,
    /// Nvim-style block column: char index into the cursor row's logical
    /// text (`h`/`l` move it, clamped to the line; row moves keep it so
    /// it behaves like vim's sticky column). Rendered as one reversed
    /// cell in `ui.rs`.
    cursor_col: usize,
    diff_scroll: u16,
    /// Last rendered inner heights of the fullscreen diff and the
    /// right-side preview (recorded by the renderer, which only gets
    /// `&App` — same pattern as `draft_wrap_width`). Drives
    /// `ensure_cursor_visible`. Zero until the first frame, which reads
    /// as height 1 (top-pin) so the cursor is never stranded.
    full_view_h: Cell<usize>,
    prev_view_h: Cell<usize>,
    /// Nvim-style visual selection anchor (`v` charwise, `V` linewise).
    /// The live end is the line cursor; `y` yanks, `Esc` cancels.
    /// Cleared whenever the diff reloads.
    visual: Option<Visual>,
    branches: Option<Vec<BranchInfo>>,
    branch_selected: usize,
    log: Option<Vec<CommitInfo>>,
    log_selected: usize,
    /// Full detail for the selected commit (message, author, file stat),
    /// shown in the [5] panel while `Focus::Log` is active.
    commit_overview: Option<CommitOverview>,
    /// Oid the loaded/loading `commit_overview` belongs to, so a stale
    /// in-flight load for a since-abandoned selection is dropped on
    /// arrival instead of overwriting the new selection's panel.
    commit_overview_for: Option<String>,
    stash: Option<Vec<StashEntry>>,
    stash_selected: usize,
    /// Upstream tracking state (upstream ref, ahead/behind, remotes).
    /// Loaded at startup and after every mutation; drives `P` behavior.
    sync: Option<SyncStatus>,
    /// A push/pull/publish job is in flight ("pushing…", …). Shown in the
    /// status panel until it succeeds or fails.
    syncing: Option<String>,
    // List scroll offsets for the left-rail panels. `Cell` so the renderer
    // (which only gets `&App`) can follow the selection without a `&mut`.
    files_scroll: Cell<usize>,
    branch_scroll: Cell<usize>,
    log_scroll: Cell<usize>,
    stash_scroll: Cell<usize>,
    /// Content width of the commit box as last rendered. The renderer
    /// records it so ↑/↓ move by the same soft-wrapped rows the user sees.
    draft_wrap_width: Cell<usize>,
    /// Collapsed directory prefixes in the files tree (no trailing slash).
    collapsed: std::collections::HashSet<String>,
    /// Open (expanded) folder header the files cursor sits on, if any.
    /// `selected` then holds the folder's first file (its diff previews).
    /// Collapsed headers don't use this: their hidden anchor file does.
    dir_cursor: Option<String>,
    /// Directory picker for opening projects (`Mode::OpenProject`).
    open_browser: Option<OpenBrowser>,
    /// Rendered Markdown preview toggle (`m` for `.md` files).
    markdown_preview: bool,
    /// (path, staged) the loaded/loading Markdown text belongs to.
    md_for: Option<(String, bool)>,
    /// Full new-version text for the Markdown preview.
    md_text: Option<String>,
    /// A mutation just landed; the [5] diff preview (and Markdown) waits
    /// for the fresh status before reloading. Reloading the old
    /// `diff_for` immediately (e.g. stash push/pop) flashes stale content
    /// — a whole-file view instead of hunks or vice versa — and costs an
    /// extra worker roundtrip.
    pending_diff_reload: bool,
    /// Mouse-driven size overrides (drag the dividers): rail width plus
    /// per-panel heights in the left rail. `None` means default.
    layout_overrides: LayoutOverrides,
    /// Divider drag in progress, if any. Set on left-press on a divider,
    /// updated on drag events, cleared on release.
    drag: Option<DragTarget>,
    /// Last rendered main layout (recorded by the renderer, which only
    /// gets `&App` — same pattern as `draft_wrap_width`). Drives mouse
    /// hit-testing for panels, dividers, and footer buttons.
    last_layout: Cell<Option<ScreenLayout>>,
    /// Fullscreen diff pane geometry (recorded by its renderer): the pane
    /// rect plus the screen row of its clickable button bar.
    last_full_rect: Cell<Option<ratatui::layout::Rect>>,
    last_full_btn_y: Cell<u16>,
    /// Screen-row → content-row hit maps recorded while rendering, so
    /// clicks land on exactly the row the user saw (wrapping included).
    /// Fullscreen `(y, diff_rows index)`, preview `(y, diff_rows index)`,
    /// finder `(y, position in finder_matches)`, browser `(y, browser
    /// list index)`.
    full_hit: std::cell::RefCell<Vec<(u16, usize)>>,
    prev_hit: std::cell::RefCell<Vec<(u16, usize)>>,
    finder_hit: std::cell::RefCell<Vec<(u16, usize)>>,
    theme_hit: std::cell::RefCell<Vec<(u16, usize)>>,
    menu_hit: std::cell::RefCell<Vec<(u16, usize)>>,
    /// Actions menu: highlighted row among the filtered entries, and
    /// whether a destructive entry is waiting for its second Enter.
    menu_selected: usize,
    /// A menu action only the workspace can run (it owns the tabs).
    workspace_request: Option<WorkspaceRequest>,
    /// Where the actions menu was opened from (main view or fullscreen
    /// diff); closing it returns there.
    menu_return: Mode,
    /// Last left-press (time, column, row) and whether the current press
    /// is the second of a double-click. One click selects, a double-click
    /// opens: a click to refocus must not open what happens to be
    /// selected.
    last_click: Option<(Instant, u16, u16)>,
    double_click: bool,
    /// Diff row where a drag-select started, and whether the drag has
    /// begun selecting (it only does once the pointer leaves that row).
    drag_anchor: Option<usize>,
    drag_selecting: bool,
    /// First line shown on the help screen, and the view it was opened
    /// over.
    help_scroll: Cell<usize>,
    help_return: Mode,
    /// Last diff search (lowercased), kept so `n` / `N` keep working, and
    /// the view the search box was opened over.
    search: Option<String>,
    search_return: Mode,
    /// Merge / rebase / cherry-pick / revert in progress, if any.
    repo_state: RepoState,
    /// The commit box is rewording the last commit instead of making one.
    amending: bool,
    /// A commit being viewed file by file (Enter in the commits panel).
    commit_view: Option<CommitView>,
    /// Partly staged file whose staged half is shown instead of the
    /// unstaged one (`S` flips). Tied to the path, so moving to another
    /// file is back to the usual view.
    staged_side_for: Option<String>,
    /// How many commits the log panel asks for (`L` raises it).
    log_limit: usize,
    /// First line shown of the commit overview.
    overview_scroll: Cell<usize>,
    /// After staging or restoring a hunk the diff reloads from the top:
    /// this is the `(path, hunk)` to go back to once it has.
    hunk_after_reload: Option<(String, usize)>,
    /// Kind of each job still waiting for its result.
    inflight: HashMap<JobId, Pending>,
    /// The error line came from a failed job: the status refresh that
    /// follows must not wipe it before the user has read it.
    error_sticky: bool,
    /// Commit message kept while the box is closed: after Esc, and after
    /// Enter until the commit is known to have succeeded.
    commit_stash: String,
    /// The draft as it was when message generation started; a result only
    /// replaces the box if the user has not typed since.
    gen_draft: Option<String>,
    confirm: Option<Confirm>,
    /// `[ui] confirm_destructive`.
    confirm_destructive: bool,
    /// Buttons drawn by the open popup this frame, and the popup's area
    /// (a click outside a list popup closes it).
    clicks: RefCell<Vec<(Rect, UiClick)>>,
    modal_rect: Cell<Option<Rect>>,
    /// Keys produced by clicks while the workspace owns the keyboard
    /// (project browser, init confirm); it replays them.
    workspace_keys: Vec<KeyCode>,
    /// Theme picker: highlighted row (index into [`Theme::names`]) and the
    /// theme to restore on Esc.
    theme_selected: usize,
    theme_before: Theme,
    browser_hit: std::cell::RefCell<Vec<(u16, usize)>>,
}

/// Divider being dragged: the vertical rail/preview split, or the bottom
/// edge of rail panel `i` (0 status, 1 files, 2 branches, 3 commits;
/// the stash panel takes the remainder).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragTarget {
    RailV,
    Panel(usize),
    /// Dragging over diff rows: selects the lines from where the button
    /// went down to the pointer.
    Select,
}

/// Two presses on the same row this close together are a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(450);

/// Which click hit map to read/record (see [`App::set_hit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HitMap {
    Full,
    Preview,
    Finder,
    Browser,
    ThemePicker,
    ActionMenu,
}

/// Nvim-style visual selection: charwise (`v`) or linewise (`V`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisualMode {
    Charwise,
    Linewise,
}

/// Normalized visual selection `((start_row, start_col), (end_row,
/// end_col), linewise)`; see `App::visual_selection`.
pub(crate) type VisualSel = ((usize, usize), (usize, usize), bool);

/// What the mouse pointer should look like over a cell, for terminals that
/// let a program say (see `main::PointerShape`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    Default,
    /// Over something a click acts on.
    Hand,
    /// Over the divider between the side panels and the diff.
    ResizeColumns,
    /// Over a divider between two side panels.
    ResizeRows,
}

/// Whether screen cell `(col, row)` falls inside `rect`.
fn mouse_in(col: u16, row: u16, rect: ratatui::layout::Rect) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// Visual anchor: the fixed end of the selection. The live end is the
/// line cursor (`cursor`, `cursor_col`); see `App::visual_selection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Visual {
    pub(crate) anchor_row: usize,
    pub(crate) anchor_col: usize,
    pub(crate) mode: VisualMode,
}

/// One subdirectory row in the project browser.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    /// Shown dimmed after the name at the places level: a drive's volume
    /// name and free space, "home folder", "recent project".
    pub note: String,
    /// The directory itself contains `.git` (repo root): badged `[repo]`.
    pub is_repo_root: bool,
}

/// Which row of the browser list is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRow {
    /// `.`: open the browsed folder itself.
    Current,
    /// `..`: go up to the parent.
    Parent,
    /// A subdirectory: opens as a project when it is a repo, otherwise
    /// Enter descends into it for browsing.
    Dir(usize),
}

/// Directory picker behind `Mode::OpenProject`. Row 0 is always `.`
/// (open this folder), row 1 is `..` (parent), then the visible
/// subdirectories (all of them, or the type-to-filter matches).
///
/// Above the top of every drive sits the places level (an empty `cwd`):
/// no `.`/`..` there, just the recent projects, the home folder and the
/// drives, so another drive is reachable without typing its path.
#[derive(Debug, Clone)]
pub struct OpenBrowser {
    pub cwd: std::path::PathBuf,
    /// Projects listed first at the places level (the start screen's
    /// recent projects); empty for the in-app browser.
    pub recents: Vec<std::path::PathBuf>,
    pub entries: Vec<DirEntry>,
    /// Subset of `entries` shown: everything without a filter, the
    /// case-insensitive substring matches with one.
    pub view: Vec<DirEntry>,
    pub selected: usize,
    pub error: Option<String>,
    /// Jump-to-path line (`tab`): typing a path instead of browsing.
    pub editing_path: bool,
    /// Type-to-filter query: any typed char narrows this folder's list,
    /// so there is no separate "start searching" step.
    pub filter: String,
}

impl OpenBrowser {
    /// A browser at `start` (the places level when it is empty).
    pub fn new(start: std::path::PathBuf, recents: Vec<std::path::PathBuf>) -> Self {
        let mut browser = Self {
            cwd: start,
            recents,
            entries: Vec::new(),
            view: Vec::new(),
            // Row 0 is `.` (the folder itself); row 1 is `..`, then dirs.
            selected: 0,
            error: None,
            editing_path: false,
            filter: String::new(),
        };
        browser.rescan();
        browser
    }

    /// At the places level: above every drive, nothing to open as `.`.
    pub fn at_places(&self) -> bool {
        self.cwd.as_os_str().is_empty()
    }

    /// The `.` and `..` rows, which the places level does not have.
    fn fixed_rows(&self) -> usize {
        if self.at_places() {
            0
        } else {
            2
        }
    }

    /// Where `..` leads: the parent folder, the places level from the top
    /// of a drive, nowhere from the places level itself.
    pub fn parent(&self) -> Option<std::path::PathBuf> {
        if self.at_places() {
            return None;
        }
        match self.cwd.parent() {
            Some(parent) => Some(parent.to_path_buf()),
            None => Some(std::path::PathBuf::new()),
        }
    }

    /// Rows in the list: `.` + `..` + one per visible subdirectory.
    pub fn row_count(&self) -> usize {
        self.view.len() + self.fixed_rows()
    }

    pub fn row(&self, index: usize) -> BrowserRow {
        if self.at_places() {
            BrowserRow::Dir(index)
        } else if index == 0 {
            BrowserRow::Current
        } else if index == 1 {
            BrowserRow::Parent
        } else {
            BrowserRow::Dir(index - 2)
        }
    }

    pub fn selected_row(&self) -> BrowserRow {
        self.row(self.selected.min(self.row_count().saturating_sub(1)))
    }

    /// Absolute path the highlighted row points at (`.` and `..`
    /// included; `..` of `/` resolves to `/` itself).
    pub fn selected_path(&self) -> std::path::PathBuf {
        match self.selected_row() {
            BrowserRow::Current => self.cwd.clone(),
            BrowserRow::Parent => self.parent().unwrap_or_else(|| self.cwd.clone()),
            BrowserRow::Dir(i) => self
                .view
                .get(i)
                .map(|e| self.cwd.join(&e.name))
                .unwrap_or_else(|| self.cwd.clone()),
        }
    }

    /// Start substring-filtering the folder list (any typed char lands
    /// here; there is no separate search mode to enter).
    pub fn push_filter_char(&mut self, c: char) {
        self.filter.push(c);
        self.apply_filter();
    }

    pub fn pop_filter_char(&mut self) {
        self.filter.pop();
        self.apply_filter();
    }

    /// Drop the query and show the full list again.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.apply_filter();
    }

    /// Rebuild the visible rows from `entries` + `filter`. `.`/`..` always
    /// stay; a fresh filter jumps the highlight to the first match.
    fn apply_filter(&mut self) {
        if self.filter.is_empty() {
            self.view = self.entries.clone();
            return;
        }
        let q = self.filter.to_lowercase();
        self.view = self
            .entries
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&q))
            .cloned()
            .collect();
        if !self.view.is_empty() {
            // First match sits after `.` and `..`.
            self.selected = self.fixed_rows();
        }
        self.selected = self.selected.min(self.row_count().saturating_sub(1));
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        let cur = self.selected.min(n - 1) as isize;
        self.selected = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    /// Re-read `cwd` from disk (subdirectories only, sorted). Keeps the
    /// old listing when the directory cannot be read. Moving folders
    /// resets the cursor and drops any active filter.
    pub fn rescan(&mut self) {
        match self.list(&self.cwd) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
            }
            Err(e) => {
                self.entries = Vec::new();
                self.error = Some(e);
            }
        }
        self.filter.clear();
        self.view = self.entries.clone();
        self.selected = self.selected.min(self.row_count().saturating_sub(1));
    }

    /// Move the browser to `dir`, rescanning only when it reads cleanly.
    /// Returns false (and keeps the old folder) on unreadable targets.
    pub fn goto(&mut self, dir: std::path::PathBuf) -> bool {
        match self.list(&dir) {
            Ok(entries) => {
                self.cwd = dir;
                self.entries = entries;
                self.view = self.entries.clone();
                self.selected = 0;
                self.error = None;
                self.filter.clear();
                true
            }
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }
}

impl OpenBrowser {
    /// What `dir` holds: its subfolders, or the places for the empty path.
    fn list(&self, dir: &std::path::Path) -> Result<Vec<DirEntry>, String> {
        if dir.as_os_str().is_empty() {
            Ok(places(&self.recents))
        } else {
            read_subdirs(dir)
        }
    }
}

/// The places level, laid out like Explorer's "This PC": every drive (the
/// filesystem root elsewhere), the home folder, then the recent projects.
/// Names are full paths.
fn places(recents: &[std::path::PathBuf]) -> Vec<DirEntry> {
    let mut found: Vec<(std::path::PathBuf, String)> = crate::drives::list()
        .into_iter()
        .map(|drive| {
            let note = drive.note();
            (drive.root, note)
        })
        .collect();
    found.extend(git_tui_core::repo::home_dir().map(|home| (home, "home folder".to_string())));
    found.extend(
        recents
            .iter()
            .map(|path| (path.clone(), "recent project".to_string())),
    );
    let mut out: Vec<DirEntry> = Vec::new();
    for (path, note) in found {
        let name = path.display().to_string();
        if out.iter().all(|e| e.name != name) {
            out.push(DirEntry {
                name,
                note,
                is_repo_root: path.join(".git").exists(),
            });
        }
    }
    out
}

/// Subdirectories of `dir`, sorted by name. Files are hidden: only
/// folders can become projects.
fn read_subdirs(dir: &std::path::Path) -> Result<Vec<DirEntry>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let ft = entry
            .file_type()
            .map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        // Follow symlinked dirs so linked projects stay browsable.
        let is_dir = ft.is_dir() || (ft.is_symlink() && entry.path().is_dir());
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let is_repo_root = path.join(".git").exists();
        out.push(DirEntry {
            name,
            note: String::new(),
            is_repo_root,
        });
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(out)
}

/// Split an upstream shorthand (`origin/main`, `origin/feature/x`) into
/// its remote and branch. Remote names cannot contain `/`, so the first
/// slash is the boundary; a bare name with no slash pushes to `origin`.
fn split_upstream(upstream: &str) -> (&str, &str) {
    match upstream.find('/') {
        Some(i) => (&upstream[..i], &upstream[i + 1..]),
        None => ("origin", upstream),
    }
}

/// Cumulative ancestor prefixes: "a/b/c/f" -> ["a", "a/b", "a/b/c"].
fn ancestors_of(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, b) in path.bytes().enumerate() {
        if b == b'/' {
            out.push(path[..i].to_string());
        }
    }
    out
}

impl App {
    #[cfg(test)]
    pub fn new(queue: JobQueue) -> Self {
        Self::new_with_config(queue, Config::default())
    }

    pub fn new_with_config(queue: JobQueue, config: Config) -> Self {
        let mut app = Self {
            queue,
            keys: config.keys,
            theme: config.theme,
            llm: config.llm,
            generating: false,
            notice: None,
            config_path: None,
            llm_form: Default::default(),
            llm_cursors: [0; 4],
            llm_selected: 0,
            llm_models: Vec::new(),
            llm_models_loading: false,
            llm_models_error: None,
            llm_models_target: String::new(),
            llm_models_request: 0,
            repo_name: "repo".into(),
            status: None,
            file_list: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            focus: Focus::Status,
            draft: String::new(),
            draft_cursor: 0,
            finder_selected: 0,
            finder_return: Mode::Normal,
            sync_return: Mode::Normal,
            pending_push: None,
            confirm_btn: Cell::new(None),
            error: None,
            quit: false,
            diff: None,
            diff_rows: Vec::new(),
            diff_for: None,
            diff_whole_file: false,
            fallback_whole_file: false,
            hunk: 0,
            cursor: 0,
            cursor_col: 0,
            diff_scroll: 0,
            full_view_h: Cell::new(0),
            prev_view_h: Cell::new(0),
            visual: None,
            branches: None,
            branch_selected: 0,
            log: None,
            log_selected: 0,
            commit_overview: None,
            commit_overview_for: None,
            stash: None,
            stash_selected: 0,
            sync: None,
            syncing: None,
            files_scroll: Cell::new(0),
            branch_scroll: Cell::new(0),
            log_scroll: Cell::new(0),
            stash_scroll: Cell::new(0),
            // Default matches the commit modal's usual inner width (60 − 2
            // borders) until the renderer records the real one.
            draft_wrap_width: Cell::new(58),
            collapsed: Default::default(),
            dir_cursor: None,
            open_browser: None,
            markdown_preview: false,
            md_for: None,
            md_text: None,
            pending_diff_reload: false,
            layout_overrides: LayoutOverrides::default(),
            drag: None,
            last_layout: Cell::new(None),
            last_full_rect: Cell::new(None),
            last_full_btn_y: Cell::new(0),
            full_hit: std::cell::RefCell::new(Vec::new()),
            prev_hit: std::cell::RefCell::new(Vec::new()),
            finder_hit: std::cell::RefCell::new(Vec::new()),
            theme_hit: std::cell::RefCell::new(Vec::new()),
            menu_hit: std::cell::RefCell::new(Vec::new()),
            menu_selected: 0,
            workspace_request: None,
            menu_return: Mode::Normal,
            last_click: None,
            double_click: false,
            drag_anchor: None,
            drag_selecting: false,
            help_scroll: Cell::new(0),
            help_return: Mode::Normal,
            search: None,
            search_return: Mode::Normal,
            repo_state: RepoState::Clean,
            amending: false,
            commit_view: None,
            staged_side_for: None,
            log_limit: Self::LOG_LIMIT,
            overview_scroll: Cell::new(0),
            hunk_after_reload: None,
            inflight: HashMap::new(),
            error_sticky: false,
            commit_stash: String::new(),
            gen_draft: None,
            confirm: None,
            confirm_destructive: config.ui.confirm_destructive,
            clicks: RefCell::new(Vec::new()),
            modal_rect: Cell::new(None),
            workspace_keys: Vec::new(),
            theme_selected: 0,
            theme_before: config.theme,
            browser_hit: std::cell::RefCell::new(Vec::new()),
        };
        app.refresh();
        app.preload_panels();
        app
    }

    pub fn repo_name(&self) -> &str {
        &self.repo_name
    }

    pub fn set_repo_name(&mut self, name: String) {
        self.repo_name = name;
    }

    /// Open the project browser (`o`): pick a directory, Enter opens it.
    /// Starts at the current project's root so siblings are one step away.
    pub fn begin_open_project(&mut self, start: std::path::PathBuf) {
        self.mode = Mode::OpenProject;
        self.draft.clear();
        self.error = None;
        self.open_browser = Some(OpenBrowser::new(start, Vec::new()));
    }

    pub fn open_browser(&self) -> Option<&OpenBrowser> {
        self.open_browser.as_ref()
    }

    pub fn open_browser_mut(&mut self) -> Option<&mut OpenBrowser> {
        self.open_browser.as_mut()
    }

    pub fn push_draft_char(&mut self, c: char) {
        self.insert_draft_char(c);
    }

    pub fn pop_draft_char(&mut self) {
        self.delete_draft_before();
    }

    pub fn clear_draft(&mut self) {
        self.draft.clear();
        self.draft_cursor = 0;
    }

    /// Cursor position in `draft` (chars from the start), clamped so stale
    /// values after direct `draft` writes stay valid.
    pub fn draft_cursor(&self) -> usize {
        self.draft_cursor.min(self.draft.chars().count())
    }

    fn draft_byte_index(&self) -> usize {
        self.draft
            .char_indices()
            .nth(self.draft_cursor())
            .map(|(i, _)| i)
            .unwrap_or(self.draft.len())
    }

    /// Insert one char at the cursor (commit box, finder, jump line).
    pub fn insert_draft_char(&mut self, c: char) {
        let byte = self.draft_byte_index();
        self.draft.insert(byte, c);
        self.draft_cursor = self.draft_cursor() + 1;
    }

    /// Backspace: delete the char before the cursor.
    pub fn delete_draft_before(&mut self) {
        let cursor = self.draft_cursor();
        if cursor == 0 {
            return;
        }
        let byte = self.draft_byte_index();
        let prev = self.draft[..byte]
            .chars()
            .next_back()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        self.draft.drain(byte - prev..byte);
        self.draft_cursor = cursor - 1;
    }

    /// Delete key: delete the char under the cursor.
    pub fn delete_draft_after(&mut self) {
        let byte = self.draft_byte_index();
        if byte >= self.draft.len() {
            return;
        }
        let len = self.draft[byte..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        self.draft.drain(byte..byte + len);
        self.draft_cursor = self.draft_cursor();
    }

    pub fn move_draft_left(&mut self) {
        self.draft_cursor = self.draft_cursor().saturating_sub(1);
    }

    pub fn move_draft_right(&mut self) {
        self.draft_cursor = (self.draft_cursor() + 1).min(self.draft.chars().count());
    }

    pub fn move_draft_home(&mut self) {
        self.draft_cursor = 0;
    }

    pub fn move_draft_end(&mut self) {
        self.draft_cursor = self.draft.chars().count();
    }

    /// Up in the commit box: one visual (soft-wrapped) row up, preserving
    /// the display column when the target row is long enough.
    pub fn move_draft_up_line(&mut self) {
        self.move_draft_visual_row(-1);
    }

    /// Down in the commit box: one visual (soft-wrapped) row down, then
    /// the very end past the last row.
    pub fn move_draft_down_line(&mut self) {
        self.move_draft_visual_row(1);
    }

    /// Move the draft cursor by `delta` visual rows using the same wrap
    /// the renderer drew (`draft_wrap_width`). The display column is
    /// preserved; past the first/last row clamps to document start/end.
    fn move_draft_visual_row(&mut self, delta: i32) {
        use unicode_width::UnicodeWidthChar;
        let width = self.draft_wrap_width.get().max(1);
        let cursor = self.draft_cursor();
        let wrap = wrap_draft(&self.draft, cursor, width);
        if wrap.bounds.is_empty() {
            return;
        }
        let target = wrap.cursor_row as i32 + delta;
        if target < 0 {
            self.draft_cursor = 0;
            return;
        }
        let target = target as usize;
        if target >= wrap.bounds.len() {
            self.draft_cursor = self.draft.chars().count();
            return;
        }
        let (rs, re) = wrap.bounds[target];
        let chars: Vec<char> = self.draft.chars().collect();
        let mut col = 0usize;
        let mut idx = rs;
        while idx < re && idx < chars.len() {
            let w = chars[idx].width().unwrap_or(0);
            if col + w > wrap.cursor_col {
                break;
            }
            col += w;
            idx += 1;
        }
        self.draft_cursor = idx;
    }

    /// Recorded by the commit-box renderer each frame.
    pub(crate) fn set_draft_wrap_width(&self, width: usize) {
        self.draft_wrap_width.set(width.max(1));
    }

    /// Cancel the open-project flow entirely (Esc in the browser).
    pub fn cancel_open_project(&mut self) {
        self.mode = Mode::Normal;
        self.draft.clear();
        self.open_browser = None;
    }

    /// Move from the browser to the `git init` confirm step, rewriting the
    /// draft to the directory that would be initialized.
    pub fn confirm_init_prompt(&mut self, dir: String) {
        self.mode = Mode::ConfirmInit;
        self.draft = dir;
    }

    /// Close the open-project flow after a successful open/switch.
    pub fn finish_open_project(&mut self) {
        self.mode = Mode::Normal;
        self.draft.clear();
        self.open_browser = None;
    }

    /// Back out of the init-confirm step to the browser (keeps the listing).
    pub fn back_to_open_project(&mut self) {
        self.mode = Mode::OpenProject;
    }

    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
    }

    pub fn set_browser_error(&mut self, msg: String) {
        if let Some(b) = self.open_browser.as_mut() {
            b.error = Some(msg);
        } else {
            self.error = Some(msg);
        }
    }

    pub(crate) fn files_scroll(&self) -> usize {
        self.files_scroll.get()
    }

    pub(crate) fn set_files_scroll(&self, off: usize) {
        self.files_scroll.set(off);
    }

    pub(crate) fn branch_scroll(&self) -> usize {
        self.branch_scroll.get()
    }

    pub(crate) fn set_branch_scroll(&self, off: usize) {
        self.branch_scroll.set(off);
    }

    pub(crate) fn log_scroll(&self) -> usize {
        self.log_scroll.get()
    }

    pub(crate) fn set_log_scroll(&self, off: usize) {
        self.log_scroll.set(off);
    }

    pub(crate) fn stash_scroll(&self) -> usize {
        self.stash_scroll.get()
    }

    pub(crate) fn set_stash_scroll(&self, off: usize) {
        self.stash_scroll.set(off);
    }

    pub(crate) fn is_collapsed(&self, dir: &str) -> bool {
        self.collapsed.contains(dir)
    }

    /// Open folder header under the files cursor. `None` on a file or a
    /// collapsed header, and when the stored folder went stale (collapsed,
    /// folded away, or emptied by a refresh).
    fn cursor_dir_is(&self, dir: &str) -> bool {
        let prefix = format!("{dir}/");
        !self.collapsed.contains(dir)
            && !self.is_hidden_path(dir)
            && self.file_list.iter().any(|f| f.path.starts_with(&prefix))
    }

    pub(crate) fn cursor_dir(&self) -> Option<&str> {
        let dir = self.dir_cursor.as_deref()?;
        self.cursor_dir_is(dir).then_some(dir)
    }

    pub(crate) fn set_collapsed(&mut self, dir: &str, value: bool) {
        if value {
            self.collapsed.insert(dir.to_string());
        } else {
            self.collapsed.remove(dir);
        }
    }

    pub fn status(&self) -> Option<&RepoStatus> {
        self.status.as_ref()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_file(&self) -> Option<&StatusEntry> {
        self.file_list
            .get(self.selected.min(self.file_count().saturating_sub(1)))
    }

    /// One browsable tree entry by index (for the file finder).
    pub fn file_entry(&self, index: usize) -> Option<&StatusEntry> {
        self.file_list.get(index)
    }

    /// Every browsable file: changed first, then clean tracked files.
    /// The files panel renders this same list so the highlight always
    /// tracks the cursor (see `render_files_panel`).
    pub fn file_list(&self) -> &[StatusEntry] {
        &self.file_list
    }

    pub fn has_files(&self) -> bool {
        !self.file_list.is_empty()
    }

    /// Rebuild [`Self::file_list`] from a fresh status: changed files keep
    /// status order, then clean tracked files alphabetically.
    fn rebuild_file_list(&mut self) {
        let Some(st) = self.status.as_ref() else {
            self.file_list = Vec::new();
            self.selected = 0;
            return;
        };
        let mut list = st.files.clone();
        let changed: std::collections::HashSet<&str> =
            st.files.iter().map(|e| e.path.as_str()).collect();
        // tracked_files arrives sorted, so filtering preserves that order.
        list.extend(
            st.tracked_files
                .iter()
                .filter(|p| !changed.contains(p.as_str()))
                .map(|p| StatusEntry {
                    path: p.clone(),
                    state: FileState::Clean,
                }),
        );
        self.file_list = list;
        self.selected = self.selected.min(self.file_count().saturating_sub(1));
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Active key bindings (the actions menu shows each action's key).
    pub(crate) fn keys(&self) -> &KeyBindings {
        &self.keys
    }

    /// The LLM settings in use (the setup form edits them in one tab).
    pub fn llm(&self) -> &LlmConfig {
        &self.llm
    }

    /// Apply LLM settings saved in another project tab.
    pub fn set_llm(&mut self, llm: LlmConfig) {
        self.llm = llm;
    }

    /// Apply a theme chosen in another project tab (the picker is global).
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Highlighted row of the theme picker.
    pub fn theme_selected(&self) -> usize {
        self.theme_selected
    }

    /// `T`: open the theme picker on the active theme, over the main view
    /// so the preview shows real content.
    pub fn begin_theme_picker(&mut self) {
        if self.mode != Mode::Normal {
            return;
        }
        self.theme_before = self.theme;
        // `default` and `catppuccin` are the same palette; the first match
        // (`default`) wins, which is what the config would say too.
        self.theme_selected = Theme::names()
            .position(|name| Theme::by_name(name).is_ok_and(|t| t == self.theme))
            .unwrap_or(0);
        self.mode = Mode::ThemePicker;
        self.error = None;
        self.notice = None;
    }

    /// Move the picker selection (wrapping) and preview that theme.
    fn theme_picker_move(&mut self, delta: isize) {
        let count = Theme::names().count() as isize;
        let next = (self.theme_selected as isize + delta).rem_euclid(count);
        self.theme_picker_select(next as usize);
    }

    fn theme_picker_select(&mut self, index: usize) {
        if let Some(theme) = Theme::names()
            .nth(index)
            .and_then(|name| Theme::by_name(name).ok())
        {
            self.theme_selected = index;
            self.theme = theme;
        }
    }

    /// Enter: keep the previewed theme and persist `[theme] name`.
    fn confirm_theme_picker(&mut self) {
        self.mode = Mode::Normal;
        let Some(name) = Theme::names().nth(self.theme_selected) else {
            return;
        };
        match self.config_path.clone() {
            Some(path) => match Config::save_theme_to_path(&path, name) {
                Ok(()) => {
                    self.notice = Some(format!("theme {name} saved to {}", path.display()));
                }
                // The theme still applies; only persisting it failed.
                Err(e) => self.error = Some(format!("theme {name} not saved: {e:#}")),
            },
            None => self.notice = Some(format!("theme {name} set for this session")),
        }
    }

    /// Esc: back to the theme that was active when the picker opened.
    fn cancel_theme_picker(&mut self) {
        self.theme = self.theme_before;
        self.mode = Mode::Normal;
    }

    fn on_key_theme_picker(&mut self, key: KeyCode) {
        match key {
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => self.theme_picker_move(1),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => self.theme_picker_move(-1),
            KeyCode::Home => self.theme_picker_select(0),
            KeyCode::End => self.theme_picker_select(Theme::names().count().saturating_sub(1)),
            KeyCode::Enter => self.confirm_theme_picker(),
            KeyCode::Esc => self.cancel_theme_picker(),
            _ => {}
        }
    }

    /// `?`: open the actions menu for the focused panel, or for the
    /// fullscreen diff.
    pub fn begin_action_menu(&mut self) {
        if !matches!(self.mode, Mode::Normal | Mode::FullDiff) {
            return;
        }
        self.menu_return = self.mode;
        self.mode = Mode::ActionMenu;
        self.draft.clear();
        self.draft_cursor = 0;
        self.menu_selected = 0;
        self.error = None;
        self.notice = None;
    }

    /// Menu entries matching the filter: menu order with no filter, best
    /// match first otherwise. Labels are matched first; group names match
    /// too (`sync`, `stash`) and those entries follow.
    pub(crate) fn menu_matches(&self) -> Vec<MenuEntry> {
        let all = crate::menu::entries(self.menu_context(), self.operation_in_progress());
        let query = self.draft.trim().to_lowercase();
        if query.is_empty() {
            return all;
        }
        let ranked = |text: &dyn Fn(&MenuEntry) -> String| -> Vec<usize> {
            let haystack: Vec<String> = all.iter().map(|e| text(e).to_lowercase()).collect();
            let refs: Vec<&str> = haystack.iter().map(String::as_str).collect();
            crate::fuzzy::rank(&query, &refs)
                .into_iter()
                .map(|(i, _)| i)
                .collect()
        };
        let mut order = ranked(&|e| e.label.to_string());
        for i in ranked(&|e| format!("{} {}", e.group, e.label)) {
            if !order.contains(&i) {
                order.push(i);
            }
        }
        order.into_iter().map(|i| all[i]).collect()
    }

    fn menu_context(&self) -> MenuContext {
        if self.menu_return == Mode::FullDiff {
            MenuContext::FullDiff
        } else {
            MenuContext::Panel(self.focus)
        }
    }

    /// The view the actions menu was opened over.
    pub fn menu_return(&self) -> Mode {
        self.menu_return
    }

    /// Highlighted row among [`Self::menu_matches`].
    pub fn menu_selected(&self) -> usize {
        self.menu_selected
    }

    fn menu_move(&mut self, delta: isize) {
        let count = self.menu_matches().len() as isize;
        if count > 0 {
            self.menu_selected = (self.menu_selected as isize + delta).rem_euclid(count) as usize;
        }
    }

    fn close_action_menu(&mut self) {
        self.mode = self.menu_return;
        self.draft.clear();
        self.draft_cursor = 0;
    }

    /// Enter: run the highlighted entry. Destructive ones go through the
    /// same confirm box as their keys (see [`Self::request_danger`]).
    fn activate_menu_entry(&mut self) {
        let Some(entry) = self.menu_matches().get(self.menu_selected).copied() else {
            return;
        };
        self.close_action_menu();
        self.run_menu_action(entry.action);
    }

    fn run_menu_action(&mut self, action: MenuAction) {
        match action {
            // The creation prompts belong to their panel: move there first
            // so the button semantics (and Esc) behave as if opened by key.
            MenuAction::Footer(FooterAction::NewBranch) => {
                self.focus = Focus::Branches;
                self.maybe_load_branches();
                self.click_footer_button(FooterAction::NewBranch);
            }
            MenuAction::Footer(FooterAction::StashPush) => {
                self.focus = Focus::Stash;
                self.maybe_load_stash();
                self.click_footer_button(FooterAction::StashPush);
            }
            MenuAction::Footer(action) => self.click_footer_button(action),
            MenuAction::Refresh => self.refresh_all(),
            MenuAction::Help => self.begin_help(),
            MenuAction::Fetch => self.start_fetch(),
            MenuAction::ForcePush => self.request_danger(Danger::ForcePush),
            MenuAction::Amend => self.begin_amend(),
            MenuAction::Undo => self.undo_discard(),
            MenuAction::AbortOperation => self.request_danger(Danger::AbortOperation),
            MenuAction::ContinueOperation => {
                if let Err(e) = self.submit_job(AsyncJob::ContinueOperation) {
                    self.error = Some(e.to_string());
                }
            }
            MenuAction::MarkdownPreview => self.toggle_markdown_preview(),
            MenuAction::LlmSettings => self.begin_llm_settings(),
            MenuAction::Focus(focus) => {
                self.focus = focus;
                self.maybe_load_branches();
                self.maybe_load_log();
                self.maybe_load_stash();
            }
            MenuAction::Workspace(request) => self.workspace_request = Some(request),
            MenuAction::Quit => self.quit = true,
        }
    }

    /// A menu action the workspace has to carry out, if one was picked.
    pub(crate) fn take_workspace_request(&mut self) -> Option<WorkspaceRequest> {
        self.workspace_request.take()
    }

    /// Keys inside the actions menu: typing filters, `↑/↓` move, Enter
    /// runs.
    fn on_key_action_menu(&mut self, key: KeyCode) {
        match key {
            KeyCode::Up | KeyCode::BackTab => self.menu_move(-1),
            KeyCode::Down | KeyCode::Tab => self.menu_move(1),
            KeyCode::Left => self.move_draft_left(),
            KeyCode::Right => self.move_draft_right(),
            KeyCode::Home => self.move_draft_home(),
            KeyCode::End => self.move_draft_end(),
            KeyCode::Char(c) => {
                self.insert_draft_char(c);
                self.menu_selected = 0;
            }
            KeyCode::Backspace => {
                self.delete_draft_before();
                self.menu_selected = 0;
            }
            KeyCode::Delete => {
                self.delete_draft_after();
                self.menu_selected = 0;
            }
            KeyCode::Enter => self.activate_menu_entry(),
            KeyCode::Esc => self.close_action_menu(),
            _ => {}
        }
    }

    /// Click a menu row: select it, or run it when it is already selected
    /// (mouse double-click without a timer).
    fn click_action_menu(&mut self, row: u16) {
        let Some(index) = self.hit(HitMap::ActionMenu, row) else {
            return;
        };
        if index == self.menu_selected && self.double_click {
            self.activate_menu_entry();
        } else {
            self.menu_selected = index;
        }
    }

    /// Click a theme row: preview it, or keep it when it is already
    /// selected (mouse double-click without a timer).
    fn click_theme_picker(&mut self, row: u16) {
        let Some(index) = self.hit(HitMap::ThemePicker, row) else {
            return;
        };
        if index == self.theme_selected && self.double_click {
            self.confirm_theme_picker();
        } else {
            self.theme_picker_select(index);
        }
    }

    /// Quit regardless of bindings (Ctrl-C safety hatch in the event loop).
    pub fn request_quit(&mut self) {
        self.quit = true;
    }

    pub fn diff(&self) -> Option<&FileDiff> {
        self.diff.as_ref()
    }

    /// Side-by-side rows for the loaded diff, paired once on arrival.
    /// Empty when no diff is loaded.
    pub(crate) fn diff_rows(&self) -> &[DiffRow] {
        &self.diff_rows
    }

    /// Replace the loaded diff and re-pair its side-by-side rows.
    /// All `self.diff` writes go through here so the cache never stales.
    fn set_diff(&mut self, diff: Option<FileDiff>) {
        self.diff_rows = diff.as_ref().map(diff_rows).unwrap_or_default();
        self.diff = diff;
        self.cursor = self.cursor.min(self.diff_rows.len().saturating_sub(1));
        // A fresh diff shifts rows: drop any visual selection with it.
        self.visual = None;
    }

    /// Whether the loaded diff shows staged (`Some(true)`) or unstaged
    /// (`Some(false)`) changes; `None` when no diff is loaded.
    pub fn diff_viewing_staged(&self) -> Option<bool> {
        self.diff_for.as_ref().map(|(_, staged)| *staged)
    }

    /// The loaded diff is a whole-file view (clean file has no diff).
    pub fn diff_whole_file(&self) -> bool {
        self.diff_whole_file
    }

    /// Full new-version Markdown text, if loaded for the current target.
    pub fn markdown_text(&self) -> Option<&str> {
        self.md_text.as_deref()
    }

    /// Whether the preview should show for the selected file: toggled on
    /// and the selected path is Markdown.
    pub fn show_markdown_preview(&self) -> bool {
        self.markdown_preview
            && self
                .selected_file()
                .is_some_and(|f| crate::markdown::is_markdown_path(&f.path))
    }

    /// `m`: toggle rendered Markdown preview for `.md` files.
    pub fn toggle_markdown_preview(&mut self) {
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        if !crate::markdown::is_markdown_path(&file.path) {
            self.error =
                Some("markdown preview is only for .md files (press enter for diff)".into());
            return;
        }
        self.markdown_preview = !self.markdown_preview;
        self.diff_scroll = 0;
        // The Markdown view has no diff rows: drop any selection with it.
        self.visual = None;
        if !self.markdown_preview {
            self.snap_scroll_to_cursor();
        }
        if self.markdown_preview {
            self.maybe_load_markdown();
        }
    }

    /// Request full new-version text for the selected Markdown file,
    /// unless it is already loaded/loading.
    fn maybe_load_markdown(&mut self) {
        if !self.show_markdown_preview() {
            return;
        }
        let target = self.diff_target();
        if target == self.md_for && self.md_text.is_some() {
            return;
        }
        // Only (re)submit when the target changed or nothing is cached.
        if target == self.md_for {
            return;
        }
        self.md_for = target.clone();
        self.md_text = None;
        if let Some((path, staged)) = target {
            if let Err(e) = self.submit_job(AsyncJob::LoadMarkdown { path, staged }) {
                self.error = Some(e.to_string());
            }
        }
    }

    pub fn hunk(&self) -> usize {
        self.hunk
    }

    /// Line-cursor row into `diff_rows` (clamped; 0 when no diff loaded).
    pub fn cursor_row(&self) -> usize {
        self.cursor.min(self.diff_rows.len().saturating_sub(1))
    }

    /// Nvim-style block column: char index into the cursor row's text.
    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    /// Active visual selection, if any.
    pub fn visual(&self) -> Option<Visual> {
        self.visual
    }

    /// Normalized selection `((start_row, start_col), (end_row, end_col),
    /// linewise)` from the anchor and the live cursor, clamped to the
    /// loaded rows. `None` when visual mode is off or no diff is loaded.
    pub(crate) fn visual_selection(&self) -> Option<VisualSel> {
        let v = self.visual?;
        if self.diff_rows.is_empty() {
            return None;
        }
        let max = self.diff_rows.len() - 1;
        let (ar, ac) = (v.anchor_row.min(max), v.anchor_col);
        let (cr, cc) = (self.cursor.min(max), self.cursor_col);
        let ((r1, c1), (r2, c2)) = if (ar, ac) <= (cr, cc) {
            ((ar, ac), (cr, cc))
        } else {
            ((cr, cc), (ar, ac))
        };
        Some(((r1, c1), (r2, c2), v.mode == VisualMode::Linewise))
    }

    /// Recorded by the fullscreen renderer each frame.
    pub(crate) fn set_full_view_h(&self, h: usize) {
        self.full_view_h.set(h);
    }

    /// Recorded by the preview renderer each frame.
    pub(crate) fn set_prev_view_h(&self, h: usize) {
        self.prev_view_h.set(h);
    }

    #[cfg(test)]
    pub(crate) fn set_view_h_for_test(&mut self, full: usize, prev: usize) {
        self.full_view_h.set(full);
        self.prev_view_h.set(prev);
    }

    /// Mouse-driven layout overrides (divider drags).
    pub(crate) fn layout_overrides(&self) -> LayoutOverrides {
        self.layout_overrides
    }

    /// Recorded by the main renderer each frame for mouse hit-testing.
    pub(crate) fn set_last_layout(&self, layout: ScreenLayout) {
        self.last_layout.set(Some(layout));
    }

    /// Recorded by the fullscreen renderer each frame.
    pub(crate) fn set_last_full_rect(&self, rect: ratatui::layout::Rect, btn_y: u16) {
        self.last_full_rect.set(Some(rect));
        self.last_full_btn_y.set(btn_y);
    }

    /// Replace a click hit map (recorded while rendering).
    pub(crate) fn set_hit(&self, which: HitMap, rows: Vec<(u16, usize)>) {
        match which {
            HitMap::Full => *self.full_hit.borrow_mut() = rows,
            HitMap::Preview => *self.prev_hit.borrow_mut() = rows,
            HitMap::Finder => *self.finder_hit.borrow_mut() = rows,
            HitMap::Browser => *self.browser_hit.borrow_mut() = rows,
            HitMap::ThemePicker => *self.theme_hit.borrow_mut() = rows,
            HitMap::ActionMenu => *self.menu_hit.borrow_mut() = rows,
        }
    }

    fn hit(&self, which: HitMap, y: u16) -> Option<usize> {
        self.hit_map(which)
            .iter()
            .find(|(row_y, _)| *row_y == y)
            .map(|(_, i)| *i)
    }

    fn hit_map(&self, which: HitMap) -> std::cell::Ref<'_, Vec<(u16, usize)>> {
        match which {
            HitMap::Full => self.full_hit.borrow(),
            HitMap::Preview => self.prev_hit.borrow(),
            HitMap::Finder => self.finder_hit.borrow(),
            HitMap::Browser => self.browser_hit.borrow(),
            HitMap::ThemePicker => self.theme_hit.borrow(),
            HitMap::ActionMenu => self.menu_hit.borrow(),
        }
    }

    /// Highlight a browser list row (project browser mouse support).
    pub(crate) fn set_browser_selected(&mut self, index: usize) {
        if let Some(b) = self.open_browser.as_mut() {
            b.selected = index.min(b.row_count().saturating_sub(1));
        }
    }

    pub(crate) fn browser_selected_index(&self) -> usize {
        self.open_browser.as_ref().map(|b| b.selected).unwrap_or(0)
    }

    /// The pointer for cell `(col, row)`: a hand over buttons and list
    /// rows in popups, resize arrows over the panel dividers.
    pub(crate) fn pointer_at(&self, col: u16, row: u16) -> Pointer {
        let on_button = self
            .clicks
            .borrow()
            .iter()
            .any(|(rect, _)| mouse_in(col, row, *rect));
        if on_button {
            return Pointer::Hand;
        }
        let list = match self.mode {
            Mode::FindFile => Some(HitMap::Finder),
            Mode::ThemePicker => Some(HitMap::ThemePicker),
            Mode::ActionMenu => Some(HitMap::ActionMenu),
            Mode::OpenProject => Some(HitMap::Browser),
            _ => None,
        };
        if let Some(list) = list {
            return match self.hit(list, row) {
                Some(_) => Pointer::Hand,
                None => Pointer::Default,
            };
        }
        let buttons = footer_buttons(self);
        if self.mode == Mode::FullDiff {
            let Some(rect) = self.last_full_rect.get() else {
                return Pointer::Default;
            };
            let first = rect.x.saturating_add(1);
            let on = row == self.last_full_btn_y.get()
                && col >= first
                && footer_button_at(&buttons, (col - first) as usize).is_some();
            return if on { Pointer::Hand } else { Pointer::Default };
        }
        if self.mode != Mode::Normal {
            return Pointer::Default;
        }
        let Some(layout) = self.last_layout.get() else {
            return Pointer::Default;
        };
        let footer = layout.footer;
        if mouse_in(col, row, footer) {
            let on =
                row == footer.y && footer_button_at(&buttons, (col - footer.x) as usize).is_some();
            return if on { Pointer::Hand } else { Pointer::Default };
        }
        let dx = rail_divider_x(&layout);
        if row >= layout.status.y && row < footer.y && (col == dx || col + 1 == dx) {
            return Pointer::ResizeColumns;
        }
        if col < dx && panel_divider_ys(&layout).contains(&row) {
            return Pointer::ResizeRows;
        }
        Pointer::Default
    }

    pub(crate) fn browser_hit_at(&self, row: u16) -> Option<usize> {
        self.hit(HitMap::Browser, row)
    }

    pub fn diff_scroll(&self) -> u16 {
        self.diff_scroll
    }

    pub fn branches(&self) -> Option<&[BranchInfo]> {
        self.branches.as_deref()
    }

    pub fn branch_selected(&self) -> usize {
        self.branch_selected
    }

    pub fn log(&self) -> Option<&[CommitInfo]> {
        self.log.as_deref()
    }

    pub fn log_selected(&self) -> usize {
        self.log_selected
    }

    pub fn commit_overview(&self) -> Option<&CommitOverview> {
        self.commit_overview.as_ref()
    }

    pub fn stash(&self) -> Option<&[StashEntry]> {
        self.stash.as_deref()
    }

    pub fn stash_selected(&self) -> usize {
        self.stash_selected
    }

    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// File-list indices matching the finder query, best first. Empty
    /// query lists every file in tree order.
    pub fn finder_matches(&self) -> Vec<usize> {
        let query = self.draft.as_str();
        if query.is_empty() {
            return (0..self.file_list.len()).collect();
        }
        let paths: Vec<&str> = self.file_list.iter().map(|e| e.path.as_str()).collect();
        fuzzy::rank(query, &paths)
            .into_iter()
            .map(|(i, _)| i)
            .collect()
    }

    /// Clamped cursor into [`Self::finder_matches`] for the renderer.
    pub fn finder_cursor(&self) -> usize {
        self.finder_selected
            .min(self.finder_matches().len().saturating_sub(1))
    }

    /// Mode the open finder returns to on Enter/Esc.
    pub fn finder_return(&self) -> Mode {
        self.finder_return
    }

    /// Upstream tracking state, if loaded yet.
    pub fn sync(&self) -> Option<&SyncStatus> {
        self.sync.as_ref()
    }

    /// In-flight push/pull/publish label ("pushing…"), if any.
    pub fn syncing(&self) -> Option<&str> {
        self.syncing.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Whether an LLM commit-message job is in flight (commit modal shows
    /// "generating…").
    pub fn is_generating(&self) -> bool {
        self.generating
    }

    #[allow(dead_code)]
    pub fn llm_config(&self) -> &LlmConfig {
        &self.llm
    }

    /// Config file location for persisting the LLM setup form.
    pub fn set_config_path(&mut self, path: Option<std::path::PathBuf>) {
        self.config_path = path;
    }

    /// Success confirmation line (cleared on the next keypress).
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Highlighted row of the LLM setup form.
    pub fn llm_selected(&self) -> usize {
        self.llm_selected.min(LLM_FIELD_LABELS.len() - 1)
    }

    /// Display value of form row `i`: the live draft for the selected row,
    /// the stashed buffer otherwise.
    pub fn llm_field_value(&self, i: usize) -> &str {
        if i == self.llm_selected() {
            &self.draft
        } else {
            self.llm_form.get(i).map(String::as_str).unwrap_or("")
        }
    }

    /// Option list backing form row `i`: the known providers for row 0, the
    /// models the provider reported for row 1, and nothing (free text) for
    /// the key/base-URL rows. Row 1 is empty until a fetch lands, so the
    /// row starts as an ordinary text field.
    pub fn llm_row_options(&self, i: usize) -> Vec<&str> {
        match i {
            0 => git_tui_core::llm::PROVIDERS.to_vec(),
            1 => self.llm_models.iter().map(String::as_str).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether a model-list fetch is in flight (the row shows "loading…").
    pub fn llm_models_loading(&self) -> bool {
        self.llm_models_loading
    }

    /// Why the last model fetch failed, if it did.
    pub fn llm_models_error(&self) -> Option<&str> {
        self.llm_models_error.as_deref()
    }

    /// Provider config as the form currently stands, used for the model
    /// fetch (and nothing else: saving still goes through
    /// [`Self::save_llm_settings`]).
    fn llm_form_config(&self) -> LlmConfig {
        LlmConfig {
            provider: self.llm_field_value(0).trim().to_lowercase(),
            model: String::new(),
            api_key: self.llm_field_value(2).trim().to_string(),
            base_url: {
                let u = self.llm_field_value(3).trim().to_string();
                if u.is_empty() {
                    None
                } else {
                    Some(u)
                }
            },
        }
    }

    /// Ask the provider which models it offers, unless that exact request
    /// is already in flight or already answered. Called on every edit to
    /// the provider/key/base-URL rows, so switching provider or pasting a
    /// key refreshes the model row without an extra keypress.
    fn refresh_llm_models(&mut self) {
        let cfg = self.llm_form_config();
        let target = llm_fetch_target(&cfg);
        if target == self.llm_models_target {
            return;
        }
        self.llm_models_target = target;
        self.llm_models_request += 1;
        self.llm_models.clear();
        self.llm_models_error = None;
        // An unknown provider or a `custom` one without a base URL has no
        // list to ask for; the row stays free text.
        if !git_tui_core::llm::PROVIDERS.contains(&cfg.provider.as_str())
            || cfg.effective_base_url().is_empty()
        {
            self.llm_models_loading = false;
            return;
        }
        // Listing models needs the same key as calling them (except for
        // local ollama). Say so instead of firing a request that can only
        // come back 401.
        if !cfg.is_configured() {
            self.llm_models_loading = false;
            self.llm_models_error =
                Some("add an API key below to list the provider's models".into());
            return;
        }
        self.llm_models_loading = true;
        let id = self.llm_models_request;
        if let Err(e) = self.submit_job(AsyncJob::FetchModels { id, llm: cfg }) {
            self.llm_models_loading = false;
            self.error = Some(e.to_string());
        }
    }

    /// Fill the model row with a fetched list: keep the typed model when
    /// the provider still offers it, else adopt the first one.
    fn apply_llm_models(&mut self, models: Vec<String>) {
        self.llm_models_loading = false;
        self.llm_models_error = None;
        self.llm_models = models;
        let Some(first) = self.llm_models.first().cloned() else {
            return;
        };
        let current = self.llm_form[1].trim().to_string();
        if !current.is_empty() && self.llm_models.contains(&current) {
            return;
        }
        self.llm_form[1] = first;
        self.llm_cursors[1] = self.llm_form[1].chars().count();
        if self.llm_selected() == 1 {
            self.draft = self.llm_form[1].clone();
            self.draft_cursor = self.llm_cursors[1];
        }
    }

    /// `←/→` on a picker row: move to the neighbouring option (wrapping).
    /// A typed value outside the list starts from its near end.
    fn cycle_llm_option(&mut self, delta: isize) {
        let options = self.llm_row_options(self.llm_selected());
        if options.is_empty() {
            return;
        }
        let current = self.draft.trim().to_lowercase();
        let n = options.len() as isize;
        let next = match options.iter().position(|o| *o == current) {
            Some(i) => (i as isize + delta).rem_euclid(n) as usize,
            None if delta < 0 => options.len() - 1,
            None => 0,
        };
        self.draft = options[next].to_string();
        self.move_draft_end();
        if self.llm_selected() == 0 {
            self.refresh_llm_models();
        }
    }

    /// `←/→`: cycle options on the provider/model rows, move the text
    /// cursor on the free-text rows.
    fn on_llm_arrow(&mut self, delta: isize) {
        let row = self.llm_selected();
        if LLM_PICKER_ROWS.contains(&row) && !self.llm_row_options(row).is_empty() {
            self.cycle_llm_option(delta);
        } else if delta < 0 {
            self.move_draft_left();
        } else {
            self.move_draft_right();
        }
    }

    /// `A` in the file list: open the LLM setup form prefilled from the
    /// current `[llm]` config (or its defaults).
    pub fn begin_llm_settings(&mut self) {
        self.mode = Mode::LlmSettings;
        self.error = None;
        self.notice = None;
        self.llm_form = [
            self.llm.provider.clone(),
            self.llm.model.clone(),
            self.llm.api_key.clone(),
            self.llm.base_url.clone().unwrap_or_default(),
        ];
        self.llm_cursors = [0; 4];
        self.llm_selected = 0;
        self.draft = self.llm_form[0].clone();
        self.move_draft_end();
        self.llm_cursors[0] = self.draft_cursor();
        // Re-ask for the current provider's models: the row must never
        // show a list left over from the previous form.
        self.llm_models.clear();
        self.llm_models_error = None;
        self.llm_models_target.clear();
        self.refresh_llm_models();
    }

    /// Stash the live draft/cursor into the selected form row.
    fn stash_llm_field(&mut self) {
        let i = self.llm_selected();
        self.llm_form[i] = self.draft.clone();
        self.llm_cursors[i] = self.draft_cursor();
    }

    /// Move the form highlight, stashing/loading the row buffers.
    fn move_llm_selection(&mut self, delta: isize) {
        let n = LLM_FIELD_LABELS.len();
        self.stash_llm_field();
        let cur = self.llm_selected.min(n - 1) as isize;
        self.llm_selected = (cur + delta).clamp(0, n as isize - 1) as usize;
        let i = self.llm_selected;
        self.draft = self.llm_form[i].clone();
        self.draft_cursor = self.llm_cursors[i].min(self.draft.chars().count());
    }

    /// Back out without saving.
    pub fn cancel_llm_settings(&mut self) {
        self.mode = Mode::Normal;
        self.draft.clear();
        self.draft_cursor = 0;
    }

    /// Enter in the setup form: validate, apply to the session, and persist
    /// `[llm]` to the config file. Stays open with an error on bad input.
    pub fn save_llm_settings(&mut self) {
        self.stash_llm_field();
        let provider = self.llm_form[0].trim().to_lowercase();
        if !git_tui_core::llm::PROVIDERS.contains(&provider.as_str()) {
            self.error = Some(format!(
                "unknown provider {provider:?} (expected one of: {})",
                git_tui_core::llm::PROVIDERS.join(", ")
            ));
            return;
        }
        let model = self.llm_form[1].trim().to_string();
        if model.is_empty() {
            self.error =
                Some("no model set: pick one from the provider list or type a model id".into());
            return;
        }
        self.llm = LlmConfig {
            provider,
            model,
            api_key: self.llm_form[2].trim().to_string(),
            base_url: {
                let u = self.llm_form[3].trim().to_string();
                if u.is_empty() {
                    None
                } else {
                    Some(u)
                }
            },
        };
        if let Some(path) = self.config_path.clone() {
            if let Err(e) = Config::save_llm_to_path(&path, &self.llm) {
                self.error = Some(e.to_string());
                return;
            }
            self.notice = Some(format!("LLM settings saved to {}", path.display()));
        } else {
            self.notice = Some("LLM settings updated for this session".into());
        }
        self.mode = Mode::Normal;
        self.draft.clear();
        self.draft_cursor = 0;
        self.error = None;
    }

    /// Keys inside the LLM setup form. Tab/Up/Down switch rows; the draft
    /// line edits the selected row, except `←/→` on the provider/model rows
    /// which steps through their options instead. Enter saves; Esc cancels.
    fn on_key_llm_settings(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char(c) => self.insert_draft_char(c),
            KeyCode::Backspace => self.delete_draft_before(),
            KeyCode::Delete => self.delete_draft_after(),
            KeyCode::Left => self.on_llm_arrow(-1),
            KeyCode::Right => self.on_llm_arrow(1),
            KeyCode::Home => self.move_draft_home(),
            KeyCode::End => self.move_draft_end(),
            KeyCode::Up => self.move_llm_selection(-1),
            KeyCode::Down | KeyCode::Tab => self.move_llm_selection(1),
            KeyCode::BackTab => self.move_llm_selection(-1),
            KeyCode::Enter => self.save_llm_settings(),
            KeyCode::Esc => self.cancel_llm_settings(),
            _ => {}
        }
        // Provider, API key, and base URL decide which models are on
        // offer, so any edit to those rows re-asks the provider.
        if matches!(self.llm_selected(), 0 | 2 | 3) {
            self.refresh_llm_models();
        }
    }

    /// Ctrl+G in the commit box: reference every staged file (index vs
    /// HEAD) and ask the LLM provider for a Conventional-Commits message.
    /// The result arrives via `poll` as `GeneratedMessage` and replaces
    /// the draft (cursor jumps to the end for quick editing).
    pub fn begin_generate_commit_message(&mut self) {
        if self.mode != Mode::Committing || self.generating {
            return;
        }
        self.generating = true;
        self.gen_draft = Some(self.draft.clone());
        self.error = None;
        if let Err(e) = self.submit_job(AsyncJob::GenerateCommitMessage {
            llm: self.llm.clone(),
        }) {
            self.generating = false;
            self.gen_draft = None;
            self.error = Some(e.to_string());
        }
    }

    #[cfg(test)]
    pub(crate) fn set_status_for_test(&mut self, st: git_tui_core::status::RepoStatus) {
        self.status = Some(st);
        self.selected = 0;
        self.rebuild_file_list();
    }

    #[cfg(test)]
    pub(crate) fn set_diff_for_test(&mut self, diff: git_tui_core::diff::FileDiff, staged: bool) {
        self.diff_for = Some((diff.path.clone(), staged));
        self.diff_whole_file = false;
        self.set_diff(Some(diff));
        self.hunk = 0;
        self.cursor = 0;
        self.cursor_col = 0;
        self.diff_scroll = 0;
    }

    #[cfg(test)]
    pub(crate) fn set_whole_file_for_test(&mut self, whole: bool) {
        self.diff_whole_file = whole;
    }

    #[cfg(test)]
    pub(crate) fn set_branches_for_test(&mut self, branches: Vec<BranchInfo>) {
        self.branches = Some(branches);
        self.branch_selected = 0;
    }

    #[cfg(test)]
    pub(crate) fn set_branch_selected_for_test(&mut self, index: usize) {
        self.branch_selected = index;
    }

    #[cfg(test)]
    pub(crate) fn set_stash_selected_for_test(&mut self, index: usize) {
        self.stash_selected = index;
    }

    #[cfg(test)]
    pub(crate) fn set_log_for_test(&mut self, entries: Vec<CommitInfo>) {
        self.log = Some(entries);
        self.log_selected = 0;
    }

    #[cfg(test)]
    pub(crate) fn set_commit_overview_for_test(&mut self, overview: CommitOverview) {
        self.commit_overview_for = Some(overview.oid.clone());
        self.commit_overview = Some(overview);
    }

    #[cfg(test)]
    pub(crate) fn set_stash_for_test(&mut self, entries: Vec<StashEntry>) {
        self.stash = Some(entries);
        self.stash_selected = 0;
    }

    fn file_count(&self) -> usize {
        self.file_list.len()
    }

    fn refresh(&mut self) {
        if let Err(e) = self.submit_job(AsyncJob::RefreshStatus) {
            self.error = Some(e.to_string());
        }
    }

    /// Eagerly load every rail panel at startup so branches, log, and
    /// stash render data immediately instead of waiting for first focus.
    /// The channel is unbounded and reads are idempotent, so submitting
    /// up front is cheap; `maybe_load_*` stays as a backstop in case a
    /// submit here ever fails.
    fn preload_panels(&mut self) {
        let jobs = [
            AsyncJob::ListBranches,
            AsyncJob::ListLog {
                limit: Self::LOG_LIMIT,
            },
            AsyncJob::ListStash,
            AsyncJob::LoadSync,
            AsyncJob::LoadRepoState,
        ];
        for job in jobs {
            if let Err(e) = self.submit_job(job) {
                self.error = Some(e.to_string());
                return;
            }
        }
    }

    /// Dispatch a keypress with no modifier information (tests and simple
    /// drivers). An uppercase letter is taken as typed with Shift. The
    /// binary uses [`Self::on_key_with_modifiers`].
    #[allow(dead_code)]
    pub fn on_key(&mut self, key: KeyCode) {
        let shift = matches!(key, KeyCode::Char(c) if c.is_ascii_uppercase());
        self.on_key_with_modifiers(key, shift);
    }

    /// Modifier-aware dispatch (the event loop calls this). Shift+Down/Up
    /// mirror `J`/`K` (hunk jumps) in Normal and FullDiff modes; text
    /// modals and the finder keep plain arrow-key behavior. Keys pressed
    /// with Ctrl or Alt never arrive here: see [`Self::on_chord`].
    pub fn on_key_with_modifiers(&mut self, key: KeyCode, shift_held: bool) {
        self.clear_messages();
        // Shift+Right in the file list jumps to the right-side diff
        // preview (plain Right expands folders, so it must stay put).
        if self.mode == Mode::Normal
            && self.focus != Focus::Diff
            && key == KeyCode::Right
            && shift_held
        {
            self.focus = Focus::Diff;
            return;
        }
        // Shift+Down/Up behave exactly like `J`/`K` wherever those apply
        // (hunk jumps in the diff preview and fullscreen). Mapping to the
        // `J`/`K` KeyCodes — rather than duplicating each arm — keeps the
        // markdown-preview guards and clamps in lockstep automatically.
        // Gated to Normal/FullDiff so Shift+arrows keep editing text in
        // commit/branch/stash prompts and moving the finder selection.
        let key = match (key, shift_held) {
            (KeyCode::Down, true) if matches!(self.mode, Mode::Normal | Mode::FullDiff) => {
                KeyCode::Char('J')
            }
            (KeyCode::Up, true) if matches!(self.mode, Mode::Normal | Mode::FullDiff) => {
                KeyCode::Char('K')
            }
            _ => key,
        };
        self.on_key_inner(key);
    }

    /// Whether the current mode is editing [`Self::draft`] as text.
    fn has_text_input(&self) -> bool {
        matches!(
            self.mode,
            Mode::Committing
                | Mode::NewBranch
                | Mode::StashPush
                | Mode::SetUpstream
                | Mode::SetRemote
                | Mode::FindFile
                | Mode::LlmSettings
                | Mode::ActionMenu
                | Mode::SearchDiff
        )
    }

    /// A key pressed with Ctrl or Alt. In text boxes these are the usual
    /// readline editing keys; elsewhere a few navigation shortcuts. Unknown
    /// combinations do nothing (they never fall back to the bare key).
    pub fn on_chord(&mut self, chord: Chord) {
        self.clear_messages();
        // Ctrl+N / Ctrl+P are Down / Up everywhere.
        match chord {
            Chord::Ctrl('n') => return self.on_key_inner(KeyCode::Down),
            Chord::Ctrl('p') => return self.on_key_inner(KeyCode::Up),
            _ => {}
        }
        if self.has_text_input() {
            self.on_chord_text(chord);
            return;
        }
        if matches!(self.mode, Mode::Normal | Mode::FullDiff) {
            match chord {
                Chord::Ctrl('l') if self.mode == Mode::Normal => self.refresh_all(),
                Chord::Ctrl('d') => self.page(1, true),
                Chord::Ctrl('u') => self.page(-1, true),
                Chord::Ctrl('f') => self.begin_search(),
                _ => {}
            }
        } else if self.mode == Mode::Help {
            match chord {
                Chord::Ctrl('d') => self.help_scroll_by(10),
                Chord::Ctrl('u') => self.help_scroll_by(-10),
                _ => {}
            }
        }
    }

    // ---- help screen ----

    /// `F1`: every key on one screen.
    pub fn begin_help(&mut self) {
        if !matches!(self.mode, Mode::Normal | Mode::FullDiff) {
            return;
        }
        self.help_return = self.mode;
        self.help_scroll.set(0);
        self.mode = Mode::Help;
    }

    /// The view the help screen was opened over.
    pub fn help_return(&self) -> Mode {
        self.help_return
    }

    fn help_scroll_by(&mut self, delta: isize) {
        let next = (self.help_scroll.get() as isize)
            .saturating_add(delta)
            .max(0);
        self.help_scroll.set(next as usize);
    }

    /// First help line to show. Only the renderer knows how long the help
    /// is, so it passes the last valid offset and the stored one is pulled
    /// back to it: scrolling stops at the end instead of running past it.
    pub(crate) fn clamp_help_scroll(&self, max: usize) -> usize {
        let off = self.help_scroll.get().min(max);
        self.help_scroll.set(off);
        off
    }

    fn on_key_help(&mut self, key: KeyCode) {
        let k = self.keys.clone();
        if key == KeyCode::Esc
            || key == KeyCode::F(1)
            || key == KeyCode::Enter
            || k.project_close.contains(&key)
            || k.action_menu.contains(&key)
        {
            self.mode = self.help_return;
        } else if k.nav_down.contains(&key) {
            self.help_scroll_by(1);
        } else if k.nav_up.contains(&key) {
            self.help_scroll_by(-1);
        } else if k.scroll_down.contains(&key) {
            self.help_scroll_by(10);
        } else if k.scroll_up.contains(&key) {
            self.help_scroll_by(-10);
        } else if key == KeyCode::Char('g') || key == KeyCode::Home {
            self.help_scroll.set(0);
        } else if key == KeyCode::Char('G') || key == KeyCode::End {
            // The renderer pulls this back to the last page.
            self.help_scroll.set(usize::MAX / 2);
        }
    }

    // ---- search inside the diff ----

    /// `Ctrl+F`: search the open diff.
    fn begin_search(&mut self) {
        if self.diff_rows.is_empty() || self.show_markdown_preview() {
            self.error = Some("nothing to search: no diff is open".into());
            return;
        }
        self.search_return = self.mode;
        if self.mode == Mode::Normal {
            self.focus = Focus::Diff;
        }
        self.mode = Mode::SearchDiff;
        self.draft = self.search.clone().unwrap_or_default();
        self.move_draft_end();
    }

    /// The view the search box was opened over.
    pub fn search_return(&self) -> Mode {
        self.search_return
    }

    /// Enter in the search box: remember the query and go to the first
    /// match at or after the cursor.
    fn submit_search(&mut self) {
        let query = self.draft.trim().to_lowercase();
        self.mode = self.search_return;
        self.draft.clear();
        self.draft_cursor = 0;
        if query.is_empty() {
            self.search = None;
            return;
        }
        self.search = Some(query);
        self.search_step(1, true);
    }

    /// Diff rows whose text contains the current query.
    fn search_matches(&self) -> Vec<usize> {
        let (Some(query), Some(diff)) = (self.search.as_deref(), self.diff.as_ref()) else {
            return Vec::new();
        };
        self.diff_rows
            .iter()
            .enumerate()
            .filter(|(_, row)| cursor_line_text(diff, row).to_lowercase().contains(query))
            .map(|(i, _)| i)
            .collect()
    }

    /// `n` / `N`: next / previous match, wrapping. `inclusive` lets the
    /// row under the cursor count (a fresh search starts where you are).
    fn search_step(&mut self, direction: isize, inclusive: bool) {
        if self.search.is_none() {
            self.error = Some("no search yet: ctrl+f searches this diff".into());
            return;
        }
        let matches = self.search_matches();
        if matches.is_empty() {
            self.error = Some(format!(
                "no match for \"{}\"",
                self.search.as_deref().unwrap_or_default()
            ));
            return;
        }
        let here = self.cursor;
        let position = if direction > 0 {
            matches
                .iter()
                .position(|&row| row > here || (inclusive && row == here))
                .unwrap_or(0)
        } else {
            matches
                .iter()
                .rposition(|&row| row < here)
                .unwrap_or(matches.len() - 1)
        };
        self.cursor = matches[position];
        self.cursor_col = 0;
        self.sync_hunk_to_cursor();
        self.ensure_cursor_visible();
        self.notice = Some(format!("match {} of {}", position + 1, matches.len()));
    }

    // ---- top / bottom, files in fullscreen ----

    /// `g` / `G`: first or last row of the focused list, or the top or
    /// bottom of the diff.
    fn jump_to_end(&mut self, bottom: bool) {
        if self.mode == Mode::FullDiff || self.focus == Focus::Diff {
            if self.show_markdown_preview() {
                self.diff_scroll = 0;
                if bottom {
                    self.scroll_diff_by(isize::from(i16::MAX));
                }
            } else if !self.diff_rows.is_empty() {
                self.cursor = if bottom { self.diff_rows.len() - 1 } else { 0 };
                self.sync_hunk_to_cursor();
                self.ensure_cursor_visible();
            }
            return;
        }
        match self.focus {
            Focus::Status => {
                let target = {
                    let rows = visible_file_rows(&self.file_list, |d| self.collapsed.contains(d));
                    let row = if bottom { rows.last() } else { rows.first() };
                    row.map(|row| match row {
                        FileRow::File { index, .. } => (Some(*index), None),
                        FileRow::Dir { path, .. } => (None, Some(path.to_string())),
                    })
                };
                match target {
                    Some((Some(index), _)) => {
                        self.selected = index;
                        self.dir_cursor = None;
                        self.maybe_load_diff();
                    }
                    Some((_, Some(dir))) => {
                        let collapsed = self.collapsed.contains(&dir);
                        self.select_folder(&dir, collapsed);
                    }
                    _ => {}
                }
            }
            Focus::Branches => {
                self.branch_selected = if bottom {
                    self.branch_count().saturating_sub(1)
                } else {
                    0
                };
            }
            Focus::Log => {
                self.log_selected = if bottom {
                    self.log_count().saturating_sub(1)
                } else {
                    0
                };
            }
            Focus::Stash => {
                self.stash_selected = if bottom {
                    self.stash_count().saturating_sub(1)
                } else {
                    0
                };
            }
            Focus::Diff => {}
        }
    }

    /// `]` / `[` in the fullscreen diff: the next / previous file, without
    /// going back to the list. Folder rows are stepped over.
    fn step_file_in_fullscreen(&mut self, down: bool) {
        let before = self.selected;
        // A step can land on a folder header; a handful more reach a file.
        for _ in 0..64 {
            let at = (self.selected, self.dir_cursor.clone());
            self.step_tree(down);
            if self.dir_cursor.is_none() && self.selected != before {
                break;
            }
            if (self.selected, self.dir_cursor.clone()) == at {
                break;
            }
        }
        if self.selected == before {
            self.dir_cursor = None;
            self.notice = Some(if down { "last file" } else { "first file" }.into());
        }
        self.visual = None;
    }

    fn on_chord_text(&mut self, chord: Chord) {
        match chord {
            Chord::Ctrl('w' | 'h') | Chord::DeleteWord => self.delete_draft_word_before(),
            Chord::Ctrl('u') => {
                let byte = self.draft_byte_index();
                // In the commit box only the current line goes.
                let start = self.draft[..byte].rfind('\n').map_or(0, |i| i + 1);
                let removed = self.draft[start..byte].chars().count();
                // Read the cursor first: it is clamped to the text length.
                let cursor = self.draft_cursor();
                self.draft.drain(start..byte);
                self.draft_cursor = cursor.saturating_sub(removed);
            }
            Chord::Ctrl('k') => {
                let byte = self.draft_byte_index();
                let end = self.draft[byte..]
                    .find('\n')
                    .map_or(self.draft.len(), |i| byte + i);
                self.draft.drain(byte..end);
            }
            Chord::Ctrl('a') => self.move_draft_home(),
            Chord::Ctrl('e') => self.move_draft_end(),
            Chord::WordLeft | Chord::Alt('b') => self.draft_cursor = self.draft_word_start(),
            Chord::WordRight | Chord::Alt('f') => self.draft_cursor = self.draft_word_end(),
            Chord::Ctrl('g') if self.mode == Mode::Committing => {
                return self.begin_generate_commit_message()
            }
            Chord::Newline | Chord::Ctrl('j') if self.mode == Mode::Committing => {
                self.insert_draft_char('\n')
            }
            _ => return,
        }
        self.after_draft_edit();
    }

    /// Char index where the word before the cursor starts.
    fn draft_word_start(&self) -> usize {
        let chars: Vec<char> = self.draft.chars().collect();
        let mut i = self.draft_cursor().min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// Char index just past the word after the cursor.
    fn draft_word_end(&self) -> usize {
        let chars: Vec<char> = self.draft.chars().collect();
        let mut i = self.draft_cursor().min(chars.len());
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Ctrl+W: delete back to the start of the word before the cursor.
    fn delete_draft_word_before(&mut self) {
        let start = self.draft_word_start();
        let end = self.draft_cursor();
        let byte_of = |index: usize| {
            self.draft
                .char_indices()
                .nth(index)
                .map_or(self.draft.len(), |(b, _)| b)
        };
        let (from, to) = (byte_of(start), byte_of(end));
        self.draft.drain(from..to);
        self.draft_cursor = start;
    }

    /// What each text box does after its text changed.
    fn after_draft_edit(&mut self) {
        match self.mode {
            Mode::FindFile => self.finder_selected = 0,
            Mode::ActionMenu => self.menu_selected = 0,
            Mode::LlmSettings => {
                if matches!(self.llm_selected(), 0 | 2 | 3) {
                    self.refresh_llm_models();
                }
            }
            _ => {}
        }
    }

    /// Pasted text. It only ever becomes text: in the commit box with its
    /// line breaks, in one-line boxes joined onto one line, and nowhere
    /// else (a paste into the file list must not run as commands).
    pub fn on_paste(&mut self, text: &str) {
        self.clear_messages();
        if !self.has_text_input() {
            self.notice =
                Some("paste ignored: open a text box first (c for the commit box)".into());
            return;
        }
        let multiline = self.mode == Mode::Committing;
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        // A trailing line break is how a copied line ends, not a request
        // to submit.
        for c in text.trim_end_matches('\n').chars() {
            match c {
                '\n' if multiline => self.insert_draft_char('\n'),
                '\n' | '\t' => self.insert_draft_char(' '),
                c if c.is_control() => {}
                c => self.insert_draft_char(c),
            }
        }
        self.after_draft_edit();
    }

    /// Reload status and the side panels without blanking the diff: used
    /// when the terminal regains focus, where usually nothing changed and
    /// a "loading…" flash would only be noise. A changed file list still
    /// reloads the diff through the normal target check.
    pub fn refresh_quietly(&mut self) {
        self.refresh();
        self.reload_branches();
        self.reload_log();
        self.reload_stash();
        self.reload_sync();
        // Same file, new content: the target is unchanged, so ask again.
        self.reload_open_diff();
    }

    /// Re-request the diff that is currently shown, keeping it on screen
    /// until the fresh one arrives.
    fn reload_open_diff(&mut self) {
        let Some((path, staged)) = self.diff_for.clone() else {
            return;
        };
        if self.commit_view.is_some() {
            // A commit never changes: nothing to reload.
            return;
        }
        let job = if self.diff_whole_file {
            AsyncJob::LoadFile { path }
        } else {
            AsyncJob::LoadDiff { path, staged }
        };
        if let Err(e) = self.submit_job(job) {
            self.error = Some(e.to_string());
        }
    }

    /// `r` / Ctrl+L: reload everything shown, not only the file list.
    pub fn refresh_all(&mut self) {
        self.reload_after_change();
    }

    /// Page the focused view by what is on screen: a whole page for
    /// PgUp/PgDn, half of one for Ctrl+D / Ctrl+U.
    fn page(&mut self, direction: isize, half: bool) {
        let view = if self.mode == Mode::FullDiff {
            self.full_view_h.get()
        } else {
            self.prev_view_h.get()
        };
        // No frame drawn yet (or a tiny pane): the old fixed step.
        let rows = match view {
            0..=3 => 10,
            h if half => (h / 2).max(1),
            h => h - 1,
        } as isize;
        let delta = direction * rows;
        if self.mode == Mode::FullDiff {
            self.move_cursor_or_scroll(delta);
        } else if self.focus == Focus::Log {
            // The right side shows the selected commit, not a diff.
            self.overview_scroll_by(delta);
        } else if self.focus == Focus::Diff && !self.show_markdown_preview() {
            self.move_cursor(delta);
        } else {
            self.scroll_diff_by(delta);
        }
    }

    /// Start of a frame: forget last frame's popup buttons.
    pub(crate) fn begin_frame(&self) {
        self.clicks.borrow_mut().clear();
        self.modal_rect.set(None);
    }

    /// Record the open popup's area (a click outside closes list popups).
    pub(crate) fn set_modal_rect(&self, rect: Rect) {
        self.modal_rect.set(Some(rect));
    }

    /// Record a button drawn inside the open popup.
    pub(crate) fn add_click(&self, rect: Rect, click: UiClick) {
        self.clicks.borrow_mut().push((rect, click));
    }

    /// A key the mouse produced. Modes the workspace owns get it there.
    fn press(&mut self, key: KeyCode) {
        if matches!(self.mode, Mode::OpenProject | Mode::ConfirmInit) {
            self.workspace_keys.push(key);
        } else {
            self.on_key_with_modifiers(key, false);
        }
    }

    /// Keys for the workspace to replay (see [`Self::press`]).
    pub(crate) fn take_workspace_keys(&mut self) -> Vec<KeyCode> {
        std::mem::take(&mut self.workspace_keys)
    }

    /// Record a left-press and work out whether it completes a
    /// double-click. Returns that, for callers outside the app.
    pub(crate) fn note_click(&mut self, col: u16, row: u16) -> bool {
        let now = Instant::now();
        self.double_click = self.last_click.is_some_and(|(at, c, r)| {
            now.duration_since(at) < DOUBLE_CLICK && r == row && c.abs_diff(col) <= 1
        });
        // A third press starts over instead of chaining doubles.
        self.last_click = if self.double_click {
            None
        } else {
            Some((now, col, row))
        };
        self.double_click
    }

    /// Right-click: select what is under the pointer, then open the
    /// actions menu for it (the menu is already scoped to the panel).
    pub fn on_right_click(&mut self, col: u16, row: u16) {
        self.double_click = false;
        match self.mode {
            Mode::Normal => {
                if let Some(layout) = self.last_layout.get() {
                    if row < layout.footer.y {
                        self.click_normal(col, row, &layout);
                    }
                }
            }
            Mode::FullDiff => {
                if let Some(idx) = self.hit(HitMap::Full, row) {
                    self.point_cursor_at(idx);
                }
            }
            _ => return,
        }
        self.drag = None;
        self.begin_action_menu();
    }

    /// Popup handling shared by every mode with one open: buttons first,
    /// then "click outside closes" for the popups where closing loses
    /// nothing. Returns whether the click was consumed.
    pub(crate) fn click_popup(&mut self, col: u16, row: u16) -> bool {
        let button = self
            .clicks
            .borrow()
            .iter()
            .find(|(rect, _)| mouse_in(col, row, *rect))
            .map(|(_, click)| *click);
        if let Some(click) = button {
            match click {
                UiClick::Key(key) => self.press(key),
                UiClick::Chord(chord) => self.on_chord(chord),
            }
            return true;
        }
        let outside = self
            .modal_rect
            .get()
            .is_some_and(|rect| !mouse_in(col, row, rect));
        let closes_on_outside = matches!(
            self.mode,
            Mode::FindFile
                | Mode::ThemePicker
                | Mode::ActionMenu
                | Mode::Confirm
                | Mode::ConfirmPush
                | Mode::Committing
                | Mode::Help
                | Mode::SearchDiff
        );
        if outside && closes_on_outside {
            self.press(KeyCode::Esc);
            return true;
        }
        false
    }

    fn on_key_inner(&mut self, key: KeyCode) {
        if self.mode == Mode::Confirm {
            match key {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') => self.accept_confirm(),
                KeyCode::Esc | KeyCode::Char('n' | 'N' | 'q') => self.cancel_confirm(),
                _ => {}
            }
            return;
        }
        if self.mode == Mode::Help {
            self.on_key_help(key);
            return;
        }
        if self.mode == Mode::LlmSettings {
            // Nothing else may leak in here: the form owns every
            // key until Enter saves or Esc cancels.
            self.on_key_llm_settings(key);
            return;
        }
        if self.mode == Mode::ThemePicker {
            self.on_key_theme_picker(key);
            return;
        }
        if self.mode == Mode::ActionMenu {
            self.on_key_action_menu(key);
            return;
        }
        if self.mode == Mode::FullDiff {
            self.on_key_full_diff(key);
            return;
        }
        if self.mode == Mode::ConfirmPush {
            match key {
                KeyCode::Enter => self.confirm_push(),
                KeyCode::Esc => self.cancel_push_confirm(),
                _ => {}
            }
            return;
        }
        if self.mode == Mode::FindFile {
            match key {
                KeyCode::Up => self.finder_move(-1),
                KeyCode::Down => self.finder_move(1),
                KeyCode::Left => self.move_draft_left(),
                KeyCode::Right => self.move_draft_right(),
                KeyCode::Char(c) => {
                    self.insert_draft_char(c);
                    self.finder_selected = 0;
                }
                KeyCode::Backspace => {
                    self.delete_draft_before();
                    self.finder_selected = 0;
                }
                KeyCode::Delete => {
                    self.delete_draft_after();
                    self.finder_selected = 0;
                }
                KeyCode::Home => self.move_draft_home(),
                KeyCode::End => self.move_draft_end(),
                KeyCode::Enter => {
                    self.expand_selected();
                    self.submit_finder();
                }
                KeyCode::Esc => {
                    self.mode = self.finder_return;
                    self.draft.clear();
                }
                _ => {}
            }
            return;
        }
        if self.mode != Mode::Normal {
            match key {
                KeyCode::Char(c) => self.insert_draft_char(c),
                KeyCode::Backspace => self.delete_draft_before(),
                KeyCode::Delete => self.delete_draft_after(),
                KeyCode::Left => self.move_draft_left(),
                KeyCode::Right => self.move_draft_right(),
                KeyCode::Up if self.mode == Mode::Committing => self.move_draft_up_line(),
                KeyCode::Down if self.mode == Mode::Committing => self.move_draft_down_line(),
                KeyCode::Home => self.move_draft_home(),
                KeyCode::End => self.move_draft_end(),
                KeyCode::Enter => match self.mode {
                    Mode::Committing => self.submit_commit(),
                    Mode::NewBranch => self.submit_new_branch(),
                    Mode::StashPush => self.submit_stash_push(),
                    Mode::SetUpstream => self.submit_push_upstream(),
                    Mode::SetRemote => self.submit_publish(),
                    Mode::SearchDiff => self.submit_search(),
                    // FullDiff, FindFile, and ConfirmPush return before
                    // reaching here. OpenProject/ConfirmInit submit
                    // through `Workspace` (it owns all projects), so they
                    // are no-ops here. LlmSettings is routed to
                    // `on_key_llm_settings` above.
                    Mode::Normal
                    | Mode::FullDiff
                    | Mode::FindFile
                    | Mode::OpenProject
                    | Mode::ConfirmInit
                    | Mode::ConfirmPush
                    | Mode::LlmSettings
                    | Mode::ThemePicker
                    | Mode::ActionMenu
                    | Mode::Confirm
                    | Mode::Help => {}
                },
                KeyCode::Esc => {
                    if self.mode == Mode::Committing && !self.amending {
                        // Closing the box is not throwing the text away.
                        // (An amend box holds the old message: nothing new.)
                        self.commit_stash = std::mem::take(&mut self.draft);
                    }
                    self.mode = match self.mode {
                        Mode::SetUpstream | Mode::SetRemote => self.sync_return,
                        Mode::SearchDiff => self.search_return,
                        _ => Mode::Normal,
                    };
                    self.draft.clear();
                    self.draft_cursor = 0;
                }
                _ => {}
            }
            return;
        }
        // Binding dispatch (first match wins; modal keys above stay fixed).
        // Cloned: small vecs, and it keeps the borrow checker happy while
        // the arms below take &mut self.
        // Esc with an active selection only leaves visual mode.
        if key == KeyCode::Esc && self.visual.is_some() {
            self.visual = None;
            return;
        }
        let k = self.keys.clone();
        if key == KeyCode::F(1) {
            self.begin_help();
        } else if key == KeyCode::BackTab {
            // Shift+Tab: the same cycle as Tab, backwards.
            self.focus = match self.focus {
                Focus::Status | Focus::Diff => Focus::Stash,
                Focus::Branches => Focus::Status,
                Focus::Log => Focus::Branches,
                Focus::Stash => Focus::Log,
            };
            self.maybe_load_branches();
            self.maybe_load_log();
            self.maybe_load_stash();
        } else if key == KeyCode::Char('g') || (key == KeyCode::Home && self.focus != Focus::Diff) {
            self.jump_to_end(false);
        } else if key == KeyCode::Char('G') || (key == KeyCode::End && self.focus != Focus::Diff) {
            self.jump_to_end(true);
        } else if key == KeyCode::Char('S') {
            self.toggle_staged_side();
        } else if key == KeyCode::Char('L') && self.focus == Focus::Log {
            self.load_more_log();
        } else if key == KeyCode::Char('n') && self.focus == Focus::Diff {
            self.search_step(1, false);
        } else if key == KeyCode::Char('N') && self.focus == Focus::Diff {
            self.search_step(-1, false);
        } else if k.focus_next.contains(&key) {
            // Tab cycles the left rail only (Status -> Branches -> Log ->
            // Stash -> Status). The right-side Diff preview is reached via
            // `5` / Shift+Right, never via Tab; Tab from Diff drops back
            // to the file list.
            self.focus = match self.focus {
                Focus::Status => Focus::Branches,
                Focus::Branches => Focus::Log,
                Focus::Log => Focus::Stash,
                Focus::Stash => Focus::Status,
                Focus::Diff => Focus::Status,
            };
            self.maybe_load_branches();
            self.maybe_load_log();
            self.maybe_load_stash();
        } else if self.focus == Focus::Status && key == KeyCode::Left {
            self.toggle_folder();
        } else if self.focus == Focus::Status && key == KeyCode::Right {
            self.expand_at_cursor();
        } else if k.focus_status.contains(&key) {
            self.focus = Focus::Status;
        } else if k.focus_branches.contains(&key) {
            self.focus = Focus::Branches;
            self.maybe_load_branches();
        } else if k.focus_log.contains(&key) {
            self.focus = Focus::Log;
            self.maybe_load_log();
        } else if k.focus_stash.contains(&key) {
            self.focus = Focus::Stash;
            self.maybe_load_stash();
        } else if k.focus_diff.contains(&key) {
            self.focus = Focus::Diff;
        } else if k.nav_down.contains(&key) {
            self.move_down();
        } else if k.nav_up.contains(&key) {
            self.move_up();
        } else if key == KeyCode::Char('h') && self.focus == Focus::Diff {
            self.move_column(-1);
        } else if key == KeyCode::Char('l') && self.focus == Focus::Diff {
            self.move_column(1);
        } else if (key == KeyCode::Char('0') || key == KeyCode::Home) && self.focus == Focus::Diff {
            self.column_home();
        } else if key == KeyCode::End && self.focus == Focus::Diff {
            self.column_end();
        } else if key == KeyCode::Char('v') && self.focus == Focus::Diff {
            self.begin_visual(VisualMode::Charwise);
        } else if key == KeyCode::Char('V') && self.focus == Focus::Diff {
            self.begin_visual(VisualMode::Linewise);
        } else if key == KeyCode::Char('y') && self.focus == Focus::Diff {
            self.begin_yank();
        } else if key == KeyCode::Char('J') && self.focus == Focus::Diff {
            // Hunk jump in the preview too: the cursor snaps to the next
            // hunk top (same as fullscreen `J`).
            if !self.show_markdown_preview() {
                self.select_hunk(self.hunk.saturating_add(1));
            }
        } else if key == KeyCode::Char('K') && self.focus == Focus::Diff {
            // Hunk jump in the preview too: the cursor snaps to the
            // previous hunk top (same as fullscreen `K`).
            if !self.show_markdown_preview() {
                self.select_hunk(self.hunk.saturating_sub(1));
            }
        } else if (key == KeyCode::Char('x') || key == KeyCode::Char('X'))
            && self.focus == Focus::Diff
        {
            // Restore the hunk under the cursor (revert just that hunk).
            self.request_danger(Danger::RestoreHunk);
        } else if k.checkout.contains(&key) && self.focus == Focus::Branches {
            self.checkout_selected_branch();
        } else if k.stash_pop.contains(&key) && self.focus == Focus::Stash {
            self.pop_selected_stash();
        } else if key == KeyCode::Enter && self.focus == Focus::Log {
            self.open_commit_view();
        } else if key == KeyCode::Enter && matches!(self.focus, Focus::Status | Focus::Diff) {
            // Enter on a file opens it fullscreen (checkout/pop own Enter
            // in their own panels, so no conflict). Works from the file
            // list and from the focused diff preview on the right.
            self.open_full_diff();
        } else if k.stage.contains(&key) {
            if self.focus == Focus::Status {
                self.toggle_stage();
            } else if self.focus == Focus::Diff {
                // Same as fullscreen space: stage the hunk under the cursor.
                self.stage_selected_hunk();
            }
        } else if k.discard.contains(&key) {
            if self.focus == Focus::Status {
                self.request_danger(Danger::DiscardSelected);
            }
        } else if k.branch_new.contains(&key) && self.focus == Focus::Branches {
            self.mode = Mode::NewBranch;
            self.draft.clear();
        } else if k.stash_push.contains(&key) && self.focus == Focus::Stash {
            self.mode = Mode::StashPush;
            self.draft.clear();
        } else if k.branch_delete.contains(&key) && self.focus == Focus::Branches {
            self.request_danger(Danger::DeleteBranch);
        } else if k.stash_drop.contains(&key) && self.focus == Focus::Stash {
            self.request_danger(Danger::DropStash);
        } else if k.scroll_up.contains(&key) {
            self.page(-1, false);
        } else if k.scroll_down.contains(&key) {
            self.page(1, false);
        } else if k.commit.contains(&key) {
            self.open_commit_box();
        } else if k.toggle_markdown_preview.contains(&key) {
            self.toggle_markdown_preview();
        } else if k.llm_settings.contains(&key) {
            self.begin_llm_settings();
        } else if k.theme_picker.contains(&key) {
            self.begin_theme_picker();
        } else if k.action_menu.contains(&key) {
            self.begin_action_menu();
        } else if k.find_files.contains(&key) {
            self.open_finder();
        } else if k.refresh.contains(&key) {
            self.refresh_all();
        } else if k.sync_pull.contains(&key) {
            self.start_pull();
        } else if k.sync_push.contains(&key) {
            self.start_push();
        } else if k.sync_fetch.contains(&key) {
            self.start_fetch();
        } else if k.undo.contains(&key) {
            self.undo_discard();
        } else if k.quit.contains(&key) {
            self.quit = true;
        }
        // A selection belongs to the diff panel: leaving it drops it (it
        // used to linger and swallow the next Esc).
        if self.mode == Mode::Normal && self.focus != Focus::Diff {
            self.visual = None;
        }
    }

    /// Keys inside the fullscreen diff overlay. `j/k`/`↑`/`↓` move the
    /// line cursor, `h/l`/`←`/`→` move the nvim-style block column,
    /// `0`/`Home`/`End` jump it, `J`/`K` jump by hunk (the cursor snaps to
    /// the hunk top), `v`/`V` select, `y` yanks, `x` restores the hunk
    /// under the cursor, `/` finds another file without leaving
    /// fullscreen; Esc leaves visual mode first, then closes back to the
    /// file list.
    fn on_key_full_diff(&mut self, key: KeyCode) {
        let k = self.keys.clone();
        if self.commit_view.is_some() {
            // Read-only: the keys that change the working tree say so,
            // closing and file stepping act on the commit.
            let closes = key == KeyCode::Esc && self.visual.is_none()
                || (k.project_close.contains(&key) && !k.quit.contains(&key));
            if closes {
                return self.close_commit_view();
            }
            if key == KeyCode::Char(']') {
                return self.step_commit_view(true);
            }
            if key == KeyCode::Char('[') {
                return self.step_commit_view(false);
            }
            let changes_things = k.stage.contains(&key)
                || k.discard.contains(&key)
                || matches!(key, KeyCode::Char('x' | 'X' | 'S'))
                || k.find_files.contains(&key)
                || k.toggle_markdown_preview.contains(&key);
            if changes_things {
                self.notice = Some("this is a commit, shown read-only (esc goes back)".into());
                return;
            }
        }
        if key == KeyCode::Esc {
            if self.visual.is_some() {
                self.visual = None;
            } else {
                self.mode = Mode::Normal;
                self.snap_scroll_to_cursor();
            }
        } else if k.action_menu.contains(&key) {
            self.begin_action_menu();
        } else if key == KeyCode::F(1) {
            self.begin_help();
        } else if key == KeyCode::Char(']') {
            self.step_file_in_fullscreen(true);
        } else if key == KeyCode::Char('[') {
            self.step_file_in_fullscreen(false);
        } else if key == KeyCode::Char('g') {
            self.jump_to_end(false);
        } else if key == KeyCode::Char('G') {
            self.jump_to_end(true);
        } else if key == KeyCode::Char('S') {
            self.toggle_staged_side();
        } else if key == KeyCode::Char('n') {
            self.search_step(1, false);
        } else if key == KeyCode::Char('N') {
            self.search_step(-1, false);
        } else if k.toggle_markdown_preview.contains(&key) {
            self.toggle_markdown_preview();
        } else if k.find_files.contains(&key) {
            self.open_finder();
        } else if key == KeyCode::Up {
            // Arrows move the line cursor so long single-hunk diffs stay
            // viewable line by line; `J`/`K` below jump by hunk.
            self.move_cursor_or_scroll(-1);
        } else if key == KeyCode::Down || k.nav_down.contains(&key) {
            self.move_cursor_or_scroll(1);
        } else if k.nav_up.contains(&key) {
            self.move_cursor_or_scroll(-1);
        } else if key == KeyCode::Char('h') || key == KeyCode::Left {
            self.move_column_or_scroll(-1);
        } else if key == KeyCode::Char('l') || key == KeyCode::Right {
            self.move_column_or_scroll(1);
        } else if key == KeyCode::Char('0') || key == KeyCode::Home {
            self.column_home();
        } else if key == KeyCode::End {
            self.column_end();
        } else if key == KeyCode::Char('v') {
            self.begin_visual(VisualMode::Charwise);
        } else if key == KeyCode::Char('V') {
            self.begin_visual(VisualMode::Linewise);
        } else if key == KeyCode::Char('y') {
            self.begin_yank();
        } else if k.stage.contains(&key) {
            self.stage_selected_hunk();
        } else if key == KeyCode::Char('x') || key == KeyCode::Char('X') {
            self.request_danger(Danger::RestoreHunk);
        } else if k.discard.contains(&key) {
            self.request_danger(Danger::DiscardLoaded);
        } else if k.project_close.contains(&key) && !k.quit.contains(&key) {
            // `q` closes what is in front: the diff, not the project.
            self.mode = Mode::Normal;
            self.visual = None;
            self.snap_scroll_to_cursor();
        } else if k.scroll_up.contains(&key) {
            self.page(-1, false);
        } else if k.scroll_down.contains(&key) {
            self.page(1, false);
        } else if key == KeyCode::Char('J') && !self.show_markdown_preview() {
            self.select_hunk(self.hunk.saturating_add(1));
        } else if key == KeyCode::Char('K') && !self.show_markdown_preview() {
            self.select_hunk(self.hunk.saturating_sub(1));
        } else if k.sync_pull.contains(&key) {
            self.start_pull();
        } else if k.sync_push.contains(&key) {
            self.start_push();
        } else if k.sync_fetch.contains(&key) {
            self.start_fetch();
        } else if k.quit.contains(&key) {
            self.quit = true;
        }
    }

    /// Enter on a commit: open what it changed, file by file, in the
    /// fullscreen diff (`]` / `[` step through its files).
    fn open_commit_view(&mut self) {
        let Some(commit) = self.selected_commit() else {
            return;
        };
        let files: Vec<String> = match self.commit_overview.as_ref() {
            Some(overview) if overview.oid == commit.oid => {
                overview.files.iter().map(|f| f.path.clone()).collect()
            }
            _ => {
                self.notice = Some("still loading this commit — try again in a moment".into());
                return;
            }
        };
        if files.is_empty() {
            self.notice = Some("this commit changed no files".into());
            return;
        }
        self.commit_view = Some(CommitView {
            oid: commit.oid.clone(),
            id: commit.oid.chars().take(7).collect(),
            files,
            index: 0,
        });
        self.mode = Mode::FullDiff;
        self.load_commit_view_file();
    }

    /// The commit being viewed, if any.
    pub(crate) fn commit_view(&self) -> Option<&CommitView> {
        self.commit_view.as_ref()
    }

    fn load_commit_view_file(&mut self) {
        let Some(view) = self.commit_view.as_ref() else {
            return;
        };
        let path = view.files[view.index.min(view.files.len() - 1)].clone();
        let oid = view.oid.clone();
        self.diff_for = Some((path.clone(), false));
        self.diff_whole_file = false;
        self.fallback_whole_file = false;
        self.set_diff(None);
        self.visual = None;
        self.hunk = 0;
        self.cursor = 0;
        self.cursor_col = 0;
        self.diff_scroll = 0;
        if let Err(e) = self.submit_job(AsyncJob::LoadCommitDiff { oid, path }) {
            self.error = Some(e.to_string());
        }
    }

    fn step_commit_view(&mut self, down: bool) {
        let Some(view) = self.commit_view.as_mut() else {
            return;
        };
        let last = view.files.len() - 1;
        let next = if down {
            (view.index + 1).min(last)
        } else {
            view.index.saturating_sub(1)
        };
        if next == view.index {
            self.notice = Some(if down { "last file" } else { "first file" }.into());
            return;
        }
        view.index = next;
        self.load_commit_view_file();
    }

    /// Leave the commit view: back to the commits panel and the working
    /// tree's own diff.
    fn close_commit_view(&mut self) {
        self.commit_view = None;
        self.mode = Mode::Normal;
        self.visual = None;
        self.diff_for = None;
        self.set_diff(None);
        self.maybe_load_diff();
    }

    /// Enter on a file: open its diff fullscreen.
    fn open_full_diff(&mut self) {
        if self.selected_file().is_some() {
            self.mode = Mode::FullDiff;
            self.snap_scroll_to_cursor();
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            Focus::Status => self.step_tree(true),
            Focus::Branches => {
                self.branch_selected = self
                    .branch_selected
                    .saturating_add(1)
                    .min(self.branch_count().saturating_sub(1));
            }
            Focus::Log => {
                self.log_selected = self
                    .log_selected
                    .saturating_add(1)
                    .min(self.log_count().saturating_sub(1));
            }
            Focus::Stash => {
                self.stash_selected = self
                    .stash_selected
                    .saturating_add(1)
                    .min(self.stash_count().saturating_sub(1));
            }
            // Right-side preview: navigation moves the line cursor
            // (PgUp/PgDn below page by 10 regardless of focus).
            Focus::Diff => {
                self.move_cursor_or_scroll(1);
            }
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            Focus::Status => self.step_tree(false),
            Focus::Branches => {
                self.branch_selected = self.branch_selected.saturating_sub(1);
            }
            Focus::Log => {
                self.log_selected = self.log_selected.saturating_sub(1);
            }
            Focus::Stash => {
                self.stash_selected = self.stash_selected.saturating_sub(1);
            }
            Focus::Diff => {
                self.move_cursor_or_scroll(-1);
            }
        }
    }

    /// Whether `path` sits under a collapsed dir (i.e. its tree rows are
    /// folded away; only the `▶` header renders).
    fn is_hidden_path(&self, path: &str) -> bool {
        ancestors_of(path)
            .into_iter()
            .any(|dir| self.collapsed.contains(dir.as_str()))
    }

    pub(crate) fn is_hidden_index(&self, index: usize) -> bool {
        self.file_list
            .get(index)
            .is_some_and(|f| self.is_hidden_path(&f.path))
    }

    /// Tree navigation: one row at a time through exactly the rows the
    /// files panel shows (`visible_file_rows`), so every folder header,
    /// open (`▼`) or collapsed (`▶`), is a cursor stop for Space (stage
    /// the whole folder) and Left/Right (fold). Clamps at both ends.
    ///
    /// - File row: select it.
    /// - Open header: `dir_cursor` marks it; `selected` becomes the
    ///   folder's first file, so the preview shows something useful.
    /// - Collapsed header: `selected` becomes a hidden anchor inside it
    ///   (the first file going down, the last going up) and the header
    ///   takes the highlight.
    fn step_tree(&mut self, down: bool) {
        enum Target {
            File(usize),
            Dir(String),
        }
        let target = {
            let rows = visible_file_rows(&self.file_list, |d| self.collapsed.contains(d));
            let Some(pos) = self.tree_cursor_row(&rows) else {
                return;
            };
            let next = if down {
                pos.checked_add(1)
            } else {
                pos.checked_sub(1)
            };
            match next.and_then(|i| rows.get(i)) {
                Some(FileRow::File { index, .. }) => Target::File(*index),
                Some(FileRow::Dir { path, .. }) => Target::Dir(path.to_string()),
                None => return,
            }
        };
        match target {
            Target::File(index) => {
                self.selected = index;
                self.dir_cursor = None;
            }
            Target::Dir(dir) if self.collapsed.contains(&dir) => {
                if let Some(anchor) = self.folder_anchor(&dir, down) {
                    self.selected = anchor;
                }
                self.dir_cursor = None;
            }
            Target::Dir(dir) => {
                // An open header previews the folder's first file.
                let prefix = format!("{dir}/");
                if let Some(first) = self
                    .file_list
                    .iter()
                    .position(|f| f.path.starts_with(&prefix))
                {
                    self.selected = first;
                }
                self.dir_cursor = Some(dir);
            }
        }
        self.maybe_load_diff();
    }

    /// Index in `rows` of the cursor: the open header it sits on, the
    /// collapsed header hiding the selected file, or the file's own row.
    fn tree_cursor_row(&self, rows: &[FileRow]) -> Option<usize> {
        let dir_row = |dir: &str| {
            rows.iter()
                .position(|r| matches!(r, FileRow::Dir { path, .. } if *path == dir))
        };
        if let Some(dir) = self.cursor_dir() {
            return dir_row(dir);
        }
        let file = self.file_list.get(self.selected)?;
        if let Some(header) = self.collapsed_dir_for(&file.path) {
            return dir_row(&header);
        }
        rows.iter()
            .position(|r| matches!(r, FileRow::File { index, .. } if *index == self.selected))
    }

    /// File to select for folder `dir`. Going down (`first`): the first
    /// file under it after the cursor, else its first file. Going up: the
    /// last file under it before the cursor, else its last file.
    fn folder_anchor(&self, dir: &str, first: bool) -> Option<usize> {
        let prefix = format!("{dir}/");
        let under: Vec<usize> = (0..self.file_list.len())
            .filter(|&i| self.file_list[i].path.starts_with(&prefix))
            .collect();
        if first {
            under
                .iter()
                .copied()
                .find(|&i| i > self.selected)
                .or_else(|| under.first().copied())
        } else {
            under
                .iter()
                .copied()
                .rev()
                .find(|&i| i < self.selected)
                .or_else(|| under.last().copied())
        }
    }

    /// Toggle collapse on the deepest collapsed-capable ancestor of the
    /// selected file (or expand when that ancestor is collapsed). Only
    /// the deepest directory flips so sibling subtrees stay visible.
    fn toggle_folder(&mut self) {
        // On an open header: fold that folder. The cursor stays on its
        // (now `▶`) header via the hidden anchor, like any collapse.
        if let Some(dir) = self.dir_cursor.take() {
            if self.cursor_dir_is(&dir) {
                self.set_collapsed(&dir, true);
                return;
            }
        }
        let Some(file) = self.selected_file() else {
            return;
        };
        let dirs: Vec<String> = ancestors_of(&file.path);
        let Some(deepest) = dirs.last().cloned() else {
            self.error = Some("nothing to collapse: file has no parent folder".into());
            return;
        };
        let collapsed = !self.collapsed.contains(&deepest);
        self.set_collapsed(&deepest, collapsed);
        // Stay put when collapsing under the cursor: the cursor becomes a
        // hidden anchor and the `▶` header takes the highlight, so Space
        // still stages the whole dir and Right still expands it. The next
        // Up/Down escapes to the nearest visible file (skipped both ways),
        // so hidden files never render while navigating.
    }

    /// Right on a file: expand every collapsed ancestor so the file is
    /// visible (no-op already visible).
    fn expand_selected(&mut self) {
        let Some(file) = self.selected_file() else {
            return;
        };
        let path = file.path.clone();
        for i in 0..path.len() {
            if path.as_bytes()[i] == b'/' {
                self.set_collapsed(&path[..i], false);
            }
        }
    }

    /// Right on the file list: open the fold at the cursor.
    ///
    /// - Hidden anchor (just collapsed, or a finder jump): expand its own
    ///   collapsed ancestors, like `expand_selected`.
    /// - Visible file: expand the folded region immediately below the
    ///   cursor, else the one immediately above. This is the way back in
    ///   after Up/Down skipped away from a `▶` header, since the highlight
    ///   never rests on hidden files while navigating.
    fn expand_at_cursor(&mut self) {
        if self.cursor_dir().is_some() {
            // Already open; don't unfold a neighbouring folder instead.
            return;
        }
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        if self.is_hidden_path(&file.path) {
            self.expand_selected();
            return;
        }
        if self.selected + 1 < self.file_count() && self.is_hidden_index(self.selected + 1) {
            let path = self.file_list[self.selected + 1].path.clone();
            if let Some(dir) = self.collapsed_dir_for(&path) {
                self.set_collapsed(&dir, false);
                return;
            }
        }
        if self.selected > 0 && self.is_hidden_index(self.selected - 1) {
            let path = self.file_list[self.selected - 1].path.clone();
            if let Some(dir) = self.collapsed_dir_for(&path) {
                self.set_collapsed(&dir, false);
            }
        }
    }

    fn hunk_count(&self) -> usize {
        self.diff.as_ref().map(|d| d.hunks.len()).unwrap_or(0)
    }

    fn select_hunk(&mut self, index: usize) {
        let clamped = index.min(self.hunk_count().saturating_sub(1));
        self.hunk = clamped;
        self.cursor = self.hunk_start_row(clamped) as usize;
        self.cursor = self.cursor.min(self.diff_rows.len().saturating_sub(1));
        // Snap the selected hunk to the top of the view.
        self.diff_scroll = self.hunk_start_row(clamped);
    }

    /// Point the line cursor at `row` without moving the viewport (mouse
    /// clicks land on a visible row; snapping would yank it to the top).
    fn point_cursor_at(&mut self, row: usize) {
        if self.diff_rows.is_empty() || self.show_markdown_preview() {
            return;
        }
        self.cursor = row.min(self.diff_rows.len().saturating_sub(1));
        self.cursor_col = 0;
        self.sync_hunk_to_cursor();
        self.ensure_cursor_visible();
    }

    /// Jump to hunk `index` like [`Self::select_hunk`] but keep the
    /// viewport stable (mouse clicks on a hunk header).
    fn point_hunk_at(&mut self, index: usize) {
        let clamped = index.min(self.hunk_count().saturating_sub(1));
        self.hunk = clamped;
        self.cursor =
            (self.hunk_start_row(clamped) as usize).min(self.diff_rows.len().saturating_sub(1));
        self.cursor_col = 0;
        self.ensure_cursor_visible();
    }

    /// Left-press at screen cell `(col, row)`: start a divider drag, or
    /// click a panel / footer button. No-op while a text modal owns the
    /// keyboard (commit, branch, stash, LLM, push/pull prompts), except
    /// the push confirm box, whose buttons are clickable.
    pub fn on_mouse_down(&mut self, col: u16, row: u16) {
        self.note_click(col, row);
        // A release lost outside the window must not leave a drag armed.
        self.drag = None;
        self.drag_anchor = None;
        self.drag_selecting = false;
        if !matches!(self.mode, Mode::Normal | Mode::FullDiff) && self.click_popup(col, row) {
            return;
        }
        match self.mode {
            Mode::FindFile => {
                self.click_finder(row);
                return;
            }
            Mode::ConfirmPush => {
                self.click_confirm_push(col, row);
                return;
            }
            Mode::ThemePicker => {
                self.click_theme_picker(row);
                return;
            }
            Mode::ActionMenu => {
                self.click_action_menu(row);
                return;
            }
            Mode::Normal | Mode::FullDiff => {}
            _ => return,
        }
        if self.mode == Mode::FullDiff {
            self.click_full(col, row);
            return;
        }
        let Some(layout) = self.last_layout.get() else {
            return;
        };
        let footer = layout.footer;
        if row == footer.y && mouse_in(col, row, footer) {
            let buttons = footer_buttons(self);
            if let Some(action) =
                footer_button_at(&buttons, (col.saturating_sub(footer.x)) as usize)
            {
                self.error = None;
                self.notice = None;
                self.click_footer_button(action);
            }
            return;
        }
        if mouse_in(col, row, footer) {
            return;
        }
        // Divider drags win over panel clicks (1-cell grip tolerance).
        let dx = rail_divider_x(&layout);
        let in_body = row >= layout.status.y && row < footer.y;
        if in_body && (col == dx || col.saturating_add(1) == dx) {
            self.drag = Some(DragTarget::RailV);
            return;
        }
        for (i, dy) in panel_divider_ys(&layout).iter().enumerate() {
            if row == *dy && col < dx {
                self.drag = Some(DragTarget::Panel(i));
                return;
            }
        }
        self.click_normal(col, row, &layout);
    }

    /// Left-drag at `(col, row)`: resize while a divider drag is active.
    /// Dividers only exist in the normal layout, so a drag that outlives
    /// a mode switch is dropped.
    pub fn on_mouse_drag(&mut self, col: u16, row: u16) {
        let Some(target) = self.drag else {
            return;
        };
        if target == DragTarget::Select {
            self.drag_select(row);
            return;
        }
        if self.mode != Mode::Normal {
            self.drag = None;
            return;
        }
        let Some(layout) = self.last_layout.get() else {
            return;
        };
        match target {
            DragTarget::RailV => {
                let origin_x = layout.status.x;
                let full_w = layout
                    .diff
                    .x
                    .saturating_add(layout.diff.width)
                    .saturating_sub(origin_x);
                let low = crate::ui::MIN_RAIL_W.min(full_w);
                let high = full_w
                    .saturating_sub(20)
                    .max(low)
                    .min(crate::ui::MAX_RAIL_W.max(low));
                let mut want = col.saturating_sub(origin_x);
                // With the diff focused the rail is drawn at two thirds of
                // its stored width: store what puts the edge under the
                // pointer.
                if self.focus == Focus::Diff {
                    want = want.saturating_mul(3) / 2;
                }
                self.layout_overrides.rail_w = Some(want.clamp(low, high.max(low)));
            }
            DragTarget::Panel(i) => {
                let rect = [layout.status, layout.files, layout.branches, layout.commits][i];
                let want = row
                    .saturating_sub(rect.y)
                    .saturating_add(1)
                    .max(crate::ui::MIN_PANEL_H);
                self.layout_overrides.heights[i] = Some(want);
            }
            DragTarget::Select => {}
        }
    }

    /// Dragging over the diff selects whole lines, from the row the button
    /// went down on to the row under the pointer. Past the top or bottom
    /// edge the view scrolls a line at a time. The selection is the same
    /// one `V` makes, so `y` or `[Copy]` yanks it.
    fn drag_select(&mut self, row: u16) {
        let map = match self.mode {
            Mode::FullDiff => HitMap::Full,
            Mode::Normal => HitMap::Preview,
            _ => {
                self.drag = None;
                return;
            }
        };
        let Some(anchor) = self.drag_anchor else {
            return;
        };
        let target = match self.hit(map, row) {
            Some(idx) => idx,
            None => {
                let (first, last) = {
                    let hits = self.hit_map(map);
                    match (hits.first(), hits.last()) {
                        (Some(first), Some(last)) => (first.0, last.0),
                        _ => return,
                    }
                };
                if row < first {
                    self.cursor.saturating_sub(1)
                } else if row > last {
                    self.cursor + 1
                } else {
                    return;
                }
            }
        };
        if !self.drag_selecting {
            if target == anchor {
                return;
            }
            self.drag_selecting = true;
            self.point_cursor_at(anchor);
            self.visual = Some(Visual {
                anchor_row: self.cursor_row(),
                anchor_col: 0,
                mode: VisualMode::Linewise,
            });
        }
        self.point_cursor_at(target);
    }

    /// Left-release: end any drag. A drag-selection stays for `y`.
    pub fn on_mouse_up(&mut self) {
        if self.drag_selecting && self.visual.is_some() {
            self.notice = Some("selected: y or [Copy] copies, esc clears".into());
        }
        self.drag = None;
        self.drag_anchor = None;
        self.drag_selecting = false;
    }

    /// Scroll the diff view by `delta` lines and keep the line cursor
    /// inside what is shown (the view moves, the cursor follows, like
    /// scrolling in an editor).
    fn wheel_diff(&mut self, delta: isize) {
        self.scroll_diff_by(delta);
        if self.show_markdown_preview() || self.diff_rows.is_empty() {
            return;
        }
        let scroll = self.diff_scroll as usize;
        let last = self.diff_rows.len() - 1;
        if self.mode == Mode::FullDiff {
            let h = self.full_view_h.get().max(1);
            self.cursor = self
                .cursor
                .clamp(scroll.min(last), (scroll + h - 1).min(last));
        } else {
            let h = self.prev_view_h.get().max(1);
            let start_of = |row: usize| crate::ui::rows_unified_len(&self.diff_rows[..row]);
            let mut cursor = self.cursor.min(last);
            while cursor < last && start_of(cursor) < scroll {
                cursor += 1;
            }
            while cursor > 0
                && start_of(cursor) + crate::ui::unified_row_count(&self.diff_rows[cursor])
                    > scroll + h
            {
                cursor -= 1;
            }
            self.cursor = cursor;
        }
        self.sync_hunk_to_cursor();
    }

    /// Scroll wheel at `(col, row)`: `up == true` is wheel-up. It scrolls
    /// whatever is under the pointer and never moves focus (panels resize
    /// with focus, which would slide them out from under the pointer).
    pub fn on_wheel(&mut self, col: u16, row: u16, up: bool) {
        let step: isize = if up { -1 } else { 1 };
        match self.mode {
            Mode::FindFile => return self.finder_move(step),
            Mode::ActionMenu => {
                let last = self.menu_matches().len().saturating_sub(1) as isize;
                self.menu_selected = (self.menu_selected as isize + step).clamp(0, last) as usize;
                return;
            }
            Mode::ThemePicker => {
                let last = Theme::names().count().saturating_sub(1) as isize;
                let next = (self.theme_selected as isize + step).clamp(0, last);
                return self.theme_picker_select(next as usize);
            }
            Mode::Help => return self.help_scroll_by(step * 3),
            Mode::FullDiff => return self.wheel_diff(step * 3),
            Mode::Normal => {}
            _ => return,
        }
        let Some(layout) = self.last_layout.get() else {
            return;
        };
        if mouse_in(col, row, layout.files) {
            self.step_tree(!up);
        } else if mouse_in(col, row, layout.branches) {
            let last = self.branch_count().saturating_sub(1) as isize;
            self.branch_selected = (self.branch_selected as isize + step).clamp(0, last) as usize;
        } else if mouse_in(col, row, layout.commits) {
            let last = self.log_count().saturating_sub(1) as isize;
            self.log_selected = (self.log_selected as isize + step).clamp(0, last) as usize;
        } else if mouse_in(col, row, layout.stash) {
            let last = self.stash_count().saturating_sub(1) as isize;
            self.stash_selected = (self.stash_selected as isize + step).clamp(0, last) as usize;
        } else if mouse_in(col, row, layout.diff) {
            if self.focus == Focus::Log {
                self.overview_scroll_by(step * 3);
            } else {
                self.wheel_diff(step * 3);
            }
        }
    }

    /// Click inside the normal (non-fullscreen) layout.
    fn click_normal(&mut self, col: u16, row: u16, layout: &ScreenLayout) {
        self.error = None;
        self.notice = None;
        if mouse_in(col, row, layout.status) {
            self.focus = Focus::Status;
        } else if mouse_in(col, row, layout.files) {
            self.focus = Focus::Status;
            self.click_file_row(layout, col, row);
        } else if mouse_in(col, row, layout.branches) {
            self.focus = Focus::Branches;
            let scroll = self.branch_scroll.get();
            let count = self.branch_count();
            let before = self.branch_selected;
            let hit = self.click_list_row(row, layout.branches, scroll, count, |app, i| {
                app.branch_selected = i;
            });
            // Double-click a branch to check it out.
            if hit && self.double_click && self.branch_selected == before {
                self.checkout_selected_branch();
            }
        } else if mouse_in(col, row, layout.commits) {
            self.focus = Focus::Log;
            let scroll = self.log_scroll.get();
            let count = self.log_count();
            let before = self.log_selected;
            let hit = self.click_list_row(row, layout.commits, scroll, count, |app, i| {
                app.log_selected = i;
            });
            if hit && self.double_click && self.log_selected == before {
                self.open_commit_view();
            }
        } else if mouse_in(col, row, layout.stash) {
            self.focus = Focus::Stash;
            let scroll = self.stash_scroll.get();
            let count = self.stash_count();
            self.click_list_row(row, layout.stash, scroll, count, |app, i| {
                app.stash_selected = i;
            });
        } else if mouse_in(col, row, layout.diff) {
            // With the commits panel focused this area shows the commit
            // overview, not diff rows: there is nothing to point at.
            if self.focus == Focus::Log {
                return;
            }
            self.focus = Focus::Diff;
            if let Some(idx) = self.hit(HitMap::Preview, row) {
                self.click_diff_row(idx);
            }
        }
    }

    /// A press on a diff row (preview or fullscreen): a header jumps to
    /// that hunk, any other row takes the line cursor and may start a
    /// drag-selection.
    fn click_diff_row(&mut self, idx: usize) {
        if self.show_markdown_preview() {
            return;
        }
        if let Some(crate::ui::DiffRow::Header { index }) = self.diff_rows.get(idx) {
            let index = *index;
            self.point_hunk_at(index);
        } else {
            self.point_cursor_at(idx);
            self.drag = Some(DragTarget::Select);
            self.drag_anchor = Some(self.cursor);
        }
    }

    /// Click a 1-per-line list panel (branches, stash): map the screen
    /// row to an item index and select it.
    fn click_list_row(
        &mut self,
        row: u16,
        rect: ratatui::layout::Rect,
        scroll: usize,
        count: usize,
        select: impl FnOnce(&mut Self, usize),
    ) -> bool {
        // The title and bottom border belong to the panel, not to a row.
        if count == 0 || row <= rect.y || row >= rect.y + rect.height.saturating_sub(1) {
            return false;
        }
        let inner = (row - rect.y - 1) as usize;
        if inner.saturating_add(scroll) >= count {
            return false;
        }
        select(self, inner + scroll);
        true
    }

    /// Click a files-tree row: select it. A double-click opens the file
    /// fullscreen; a click on a folder's arrow folds or unfolds it.
    fn click_file_row(&mut self, layout: &ScreenLayout, col: u16, row: u16) {
        let rect = layout.files;
        // The title and bottom border belong to the panel, not to a row.
        if row <= rect.y || row >= rect.y + rect.height.saturating_sub(1) {
            return;
        }
        let inner = (row - rect.y - 1) as usize;
        let rows = visible_file_rows(&self.file_list, |d| self.collapsed.contains(d));
        let pos = inner.saturating_add(self.files_scroll.get());
        let Some(target) = rows.get(pos).cloned() else {
            return;
        };
        match target {
            FileRow::File { index, .. } => {
                if index == self.selected && self.dir_cursor.is_none() {
                    if self.double_click {
                        self.open_full_diff();
                    }
                } else {
                    self.selected = index;
                    self.dir_cursor = None;
                    self.maybe_load_diff();
                }
            }
            FileRow::Dir { path, depth } => {
                let dir = path.to_string();
                // Border, the two-cell selection marker, the indent, then
                // the arrow and the space after it.
                let arrow_end = rect.x as usize + 1 + 2 + depth * 2 + 2;
                let on_arrow = (col as usize) < arrow_end;
                let was_collapsed = self.collapsed.contains(&dir);
                let already_here = self.cursor_dir().is_some_and(|d| d == dir);
                if on_arrow {
                    self.select_folder(&dir, !was_collapsed);
                    self.set_collapsed(&dir, !was_collapsed);
                } else if already_here && self.double_click {
                    // Double-click an open header: its first file, fullscreen.
                    self.open_full_diff();
                } else {
                    self.select_folder(&dir, was_collapsed);
                }
            }
        }
    }

    /// Put the cursor on folder `dir`: on its header when it is (or will
    /// be) open, on the hidden anchor that highlights the `▶` row when
    /// collapsed.
    fn select_folder(&mut self, dir: &str, collapsed: bool) {
        if collapsed {
            if let Some(anchor) = self.folder_anchor(dir, true) {
                self.selected = anchor;
            }
            self.dir_cursor = None;
        } else {
            let prefix = format!("{dir}/");
            if let Some(first) = self
                .file_list
                .iter()
                .position(|f| f.path.starts_with(&prefix))
            {
                self.selected = first;
            }
            self.dir_cursor = Some(dir.to_string());
        }
        self.maybe_load_diff();
    }

    /// Click inside the fullscreen diff: move the cursor to the clicked
    /// row (a header click jumps to that hunk without snapping the view),
    /// or fire the button bar.
    fn click_full(&mut self, col: u16, row: u16) {
        self.error = None;
        self.notice = None;
        if let Some(rect) = self.last_full_rect.get() {
            let btn_y = self.last_full_btn_y.get();
            if row == btn_y
                && col >= rect.x.saturating_add(1)
                && col < rect.x.saturating_add(rect.width.saturating_sub(1))
            {
                let buttons = footer_buttons(self);
                if let Some(action) = footer_button_at(
                    &buttons,
                    (col.saturating_sub(rect.x.saturating_add(1))) as usize,
                ) {
                    self.click_footer_button(action);
                }
                return;
            }
        }
        let Some(idx) = self.hit(HitMap::Full, row) else {
            return;
        };
        self.click_diff_row(idx);
    }

    /// Click the push confirm box's `[Push]` / `[Cancel]` buttons
    /// (recorded while rendering as `(x, y)` of the `[Push]` start).
    /// Clicks anywhere else in the box do nothing.
    fn click_confirm_push(&mut self, col: u16, row: u16) {
        // `[Push] ` is 7 cells, a 1-cell gap, then `[Cancel] ` (9 cells).
        const PUSH_W: u16 = 7;
        const CANCEL_W: u16 = 9;
        let Some((x, y)) = self.confirm_btn.get() else {
            return;
        };
        if row != y || col < x {
            return;
        }
        let off = col - x;
        if off < PUSH_W {
            self.confirm_push();
        } else if (PUSH_W + 1..PUSH_W + 1 + CANCEL_W).contains(&off) {
            self.cancel_push_confirm();
        }
    }

    /// Click a finder match row: select it, or open it when it is already
    /// selected (mouse double-click without a timer).
    fn click_finder(&mut self, row: u16) {
        let Some(pos) = self.hit(HitMap::Finder, row) else {
            return;
        };
        if pos == self.finder_cursor() && self.double_click {
            self.expand_selected();
            self.submit_finder();
        } else {
            self.finder_selected = pos.min(self.finder_matches().len().saturating_sub(1));
        }
    }

    /// Fire a footer button (mouse) with the same semantics as its key.
    pub(crate) fn click_footer_button(&mut self, action: FooterAction) {
        if self.commit_view.is_some() && self.mode == Mode::FullDiff {
            match action {
                FooterAction::CloseDiff => return self.close_commit_view(),
                FooterAction::StageHunk
                | FooterAction::RestoreHunk
                | FooterAction::Discard
                | FooterAction::Find => {
                    self.notice = Some("this is a commit, shown read-only (esc goes back)".into());
                    return;
                }
                _ => {}
            }
        }
        match action {
            FooterAction::Stage => {
                if self.focus == Focus::Status {
                    self.toggle_stage();
                }
            }
            FooterAction::Discard => {
                if self.mode == Mode::FullDiff {
                    self.request_danger(Danger::DiscardLoaded);
                } else if self.focus == Focus::Status {
                    self.request_danger(Danger::DiscardSelected);
                }
            }
            FooterAction::Commit => {
                if self.mode == Mode::Normal {
                    self.open_commit_box();
                }
            }
            FooterAction::Pull => self.start_pull(),
            FooterAction::Push => self.start_push(),
            FooterAction::Find => self.open_finder(),
            FooterAction::Theme => self.begin_theme_picker(),
            FooterAction::Menu => self.begin_action_menu(),
            FooterAction::Copy => self.begin_yank(),
            FooterAction::OpenDiff => self.open_full_diff(),
            FooterAction::CloseDiff => {
                if self.mode == Mode::FullDiff {
                    self.mode = Mode::Normal;
                    self.snap_scroll_to_cursor();
                }
            }
            FooterAction::StageHunk => self.stage_selected_hunk(),
            FooterAction::RestoreHunk => self.request_danger(Danger::RestoreHunk),
            FooterAction::Checkout => {
                if self.focus == Focus::Branches {
                    self.checkout_selected_branch();
                }
            }
            FooterAction::NewBranch => {
                if self.focus == Focus::Branches {
                    self.mode = Mode::NewBranch;
                    self.draft.clear();
                }
            }
            FooterAction::DeleteBranch => {
                if self.focus == Focus::Branches {
                    self.request_danger(Danger::DeleteBranch);
                }
            }
            FooterAction::StashPop => {
                if self.focus == Focus::Stash {
                    self.pop_selected_stash();
                }
            }
            FooterAction::StashPush => {
                if self.focus == Focus::Stash {
                    self.mode = Mode::StashPush;
                    self.draft.clear();
                }
            }
            FooterAction::StashDrop => {
                if self.focus == Focus::Stash {
                    self.request_danger(Danger::DropStash);
                }
            }
        }
    }

    /// Move the line cursor by `delta` rows, clamped to the loaded rows.
    /// The hunk follows the cursor (nearest header at or above it) so
    /// `space` stages the hunk under the cursor; the view follows at the
    /// edge only (`ensure_cursor_visible`), nvim-style. No-op with no
    /// diff or in Markdown preview (which owns plain scrolling instead).
    fn move_cursor(&mut self, delta: isize) {
        if self.diff_rows.is_empty() || self.show_markdown_preview() {
            return;
        }
        let max = self.diff_rows.len().saturating_sub(1) as isize;
        let cur = (self.cursor as isize).clamp(0, max);
        self.cursor = (cur + delta).clamp(0, max) as usize;
        self.sync_hunk_to_cursor();
        self.ensure_cursor_visible();
    }

    /// Nvim-style viewport follow: the cursor walks freely inside the
    /// visible window and the view scrolls only once it would leave it
    /// (top edge pins, bottom edge advances minimally). Fullscreen counts
    /// side-by-side rows, the preview unified lines (a del/add pair keeps
    /// both of its lines visible). Unknown height (no frame rendered yet)
    /// reads as 1, i.e. top-pin, so the cursor is never stranded.
    fn ensure_cursor_visible(&mut self) {
        if self.show_markdown_preview() || self.diff_rows.is_empty() {
            return;
        }
        if self.mode == Mode::FullDiff {
            let h = self.full_view_h.get().max(1);
            let scroll = self.diff_scroll as usize;
            if self.cursor < scroll {
                self.diff_scroll = self.cursor.min(u16::MAX as usize) as u16;
            } else if self.cursor + 1 > scroll + h {
                self.diff_scroll = (self.cursor + 1 - h).min(u16::MAX as usize) as u16;
            }
        } else {
            let h = self.prev_view_h.get().max(1);
            let c = self.cursor_row();
            let start = crate::ui::rows_unified_len(&self.diff_rows[..c]);
            let end = start + crate::ui::unified_row_count(&self.diff_rows[c]);
            let scroll = self.diff_scroll as usize;
            if start < scroll {
                self.diff_scroll = start.min(u16::MAX as usize) as u16;
            } else if end > scroll + h {
                self.diff_scroll = end.saturating_sub(h).min(u16::MAX as usize) as u16;
            }
        }
    }

    /// Hunk under the cursor: nearest hunk header at or above it.
    fn sync_hunk_to_cursor(&mut self) {
        let mut hunk = 0;
        for (i, row) in self.diff_rows.iter().enumerate() {
            if i > self.cursor {
                break;
            }
            if let DiffRow::Header { index } = row {
                hunk = *index;
            }
        }
        self.hunk = hunk.min(self.hunk_count().saturating_sub(1));
    }

    /// Keep the cursor visible by top-pinning the view on its row:
    /// side-by-side rows fullscreen, unified lines in the preview.
    fn snap_scroll_to_cursor(&mut self) {
        if self.show_markdown_preview() {
            return;
        }
        if self.mode == Mode::FullDiff {
            self.diff_scroll = (self.cursor as u16).min(self.diff_full_max());
        } else {
            let off = crate::ui::rows_unified_len(
                &self.diff_rows[..self.cursor.min(self.diff_rows.len())],
            );
            self.diff_scroll = (off as u16).min(self.diff_preview_max());
        }
    }

    /// Line-cursor step, or plain scroll when the Markdown preview owns
    /// the view (it has no diff rows to point at).
    fn move_cursor_or_scroll(&mut self, delta: isize) {
        if self.show_markdown_preview() {
            self.scroll_diff_by(delta);
        } else {
            self.move_cursor(delta);
        }
    }

    /// Nvim-style `h`/`l`: move the block column within the cursor row's
    /// text, clamped to its last char (like `$`). Row moves never touch
    /// it, so it sticks across short/long lines like vim's column.
    /// No-op with no diff or in Markdown preview.
    fn move_column(&mut self, delta: isize) {
        if self.show_markdown_preview() {
            return;
        }
        let max = self.cursor_text_len().saturating_sub(1) as isize;
        // No text under the block (blank side): nowhere to move.
        if self.cursor_text_len() == 0 {
            return;
        }
        let cur = (self.cursor_col as isize).clamp(0, max);
        self.cursor_col = (cur + delta).clamp(0, max) as usize;
    }

    /// Char count of the text under the block cursor (0 with no diff).
    fn cursor_text_len(&self) -> usize {
        match (self.diff_rows.get(self.cursor_row()), self.diff.as_ref()) {
            (Some(row), Some(diff)) => cursor_line_text(diff, row).chars().count(),
            _ => 0,
        }
    }

    /// Nvim-style `0`/`Home`: block to the line start.
    fn column_home(&mut self) {
        if self.show_markdown_preview() {
            return;
        }
        if self.diff_rows.get(self.cursor_row()).is_some() {
            self.cursor_col = 0;
        }
    }

    /// Nvim-style `End`: block to the line's last char.
    fn column_end(&mut self) {
        if self.show_markdown_preview() {
            return;
        }
        self.cursor_col = self.cursor_text_len().saturating_sub(1);
    }

    /// Block-column step, or plain scroll when the Markdown preview owns
    /// the view.
    fn move_column_or_scroll(&mut self, delta: isize) {
        if self.show_markdown_preview() {
            self.scroll_diff_by(delta);
        } else {
            self.move_column(delta);
        }
    }

    /// `v`/`V`: enter visual mode anchoring at the cursor (charwise /
    /// linewise). Same key again leaves it; the other key flips the mode
    /// keeping the anchor, like nvim. Motions extend the selection since
    /// its live end is the line cursor.
    fn begin_visual(&mut self, mode: VisualMode) {
        if self.diff_rows.is_empty() || self.show_markdown_preview() {
            return;
        }
        match self.visual {
            Some(v) if v.mode == mode => self.visual = None,
            Some(mut v) => {
                v.mode = mode;
                self.visual = Some(v);
            }
            None => {
                self.visual = Some(Visual {
                    anchor_row: self.cursor_row(),
                    anchor_col: self.cursor_col,
                    mode,
                })
            }
        }
    }

    /// Char slice `[from..=to]` without cutting UTF-8 boundaries.
    fn slice_chars(text: &str, from: usize, to_incl: usize) -> String {
        if from > to_incl {
            return String::new();
        }
        text.chars().skip(from).take(to_incl - from + 1).collect()
    }

    /// Text the next yank would take: the selection (linewise whole rows
    /// with trailing newlines, charwise endpoint slices joined by `\n`),
    /// or the cursor line when visual mode is off. `None` when empty.
    fn yank_text(&self) -> Option<String> {
        let diff = self.diff.as_ref()?;
        if self.diff_rows.is_empty() {
            return None;
        }
        let text = match self.visual_selection() {
            None => {
                let line = cursor_line_text(diff, &self.diff_rows[self.cursor_row()]);
                if line.is_empty() {
                    return None;
                }
                format!("{line}\n")
            }
            Some(((r1, c1), (r2, c2), linewise)) => {
                if linewise {
                    let mut s = String::new();
                    for r in r1..=r2 {
                        s.push_str(&cursor_line_text(diff, &self.diff_rows[r]));
                        s.push('\n');
                    }
                    s
                } else if r1 == r2 {
                    Self::slice_chars(&cursor_line_text(diff, &self.diff_rows[r1]), c1, c2)
                } else {
                    let mut s = String::new();
                    let first = cursor_line_text(diff, &self.diff_rows[r1]);
                    s.push_str(&first.chars().skip(c1).collect::<String>());
                    for r in r1 + 1..r2 {
                        s.push('\n');
                        s.push_str(&cursor_line_text(diff, &self.diff_rows[r]));
                    }
                    s.push('\n');
                    s.push_str(&Self::slice_chars(
                        &cursor_line_text(diff, &self.diff_rows[r2]),
                        0,
                        c2,
                    ));
                    s
                }
            }
        };
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// `y`: yank the selection (or the cursor line) to the system
    /// clipboard via OSC 52 and leave visual mode. Empty selections
    /// report instead of clobbering the clipboard.
    fn begin_yank(&mut self) {
        let Some(text) = self.yank_text() else {
            self.error = Some("nothing to yank".into());
            return;
        };
        self.visual = None;
        if text.ends_with('\n') {
            let lines = text.lines().count();
            self.notice = Some(format!(
                "yanked {} line{}",
                lines,
                if lines == 1 { "" } else { "s" }
            ));
        } else {
            self.notice = Some(format!("yanked {} chars", text.chars().count()));
        }
        if !yank_to_clipboard(&text) {
            self.error = Some("yanked, but the terminal refused the clipboard (OSC 52)".into());
        }
    }

    /// Rendered row offset where hunk `index` starts (its header row in the
    /// side-by-side rows cached from the loaded diff).
    fn hunk_start_row(&self, index: usize) -> u16 {
        crate::ui::rows_hunk_start(&self.diff_rows, index)
    }
    /// Last valid preview scroll offset (unified lines), so scrolling the
    /// [5] tab stops on the last line instead of running into blank space.
    fn diff_preview_max(&self) -> u16 {
        crate::ui::rows_unified_len(&self.diff_rows)
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16
    }

    /// Last valid fullscreen offset (side-by-side rows).
    fn diff_full_max(&self) -> u16 {
        self.diff_rows
            .len()
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16
    }

    /// Scroll the diff by `delta` lines, clamped to the content of the
    /// currently shown view (unified preview in Normal, side-by-side rows
    /// in FullDiff). Markdown preview lines depend on the render width,
    /// so they use a generous cap and the renderer clamps the slice.
    fn scroll_diff_by(&mut self, delta: isize) {
        let max = if self.show_markdown_preview() {
            self.md_text
                .as_ref()
                .map(|t| (t.lines().count() * 4 + 16).min(u16::MAX as usize) as u16)
                .unwrap_or(u16::MAX)
        } else if self.mode == Mode::FullDiff {
            self.diff_full_max()
        } else {
            self.diff_preview_max()
        };
        let cur = self.diff_scroll as isize;
        self.diff_scroll = (cur + delta).clamp(0, max as isize) as u16;
    }

    fn log_count(&self) -> usize {
        self.log.as_ref().map(|l| l.len()).unwrap_or(0)
    }

    fn selected_commit(&self) -> Option<CommitInfo> {
        self.log
            .as_ref()?
            .get(self.log_selected.min(self.log_count().saturating_sub(1)))
            .cloned()
    }

    /// Request the selected commit's detail unless it is already
    /// loaded/loading (matches `maybe_load_diff`'s per-tick, idempotent
    /// shape — cheap to call from `poll` every frame).
    fn maybe_load_commit_overview(&mut self) {
        let Some(commit) = self.selected_commit() else {
            self.commit_overview = None;
            self.commit_overview_for = None;
            return;
        };
        if self.commit_overview_for.as_deref() == Some(commit.oid.as_str()) {
            return;
        }
        self.commit_overview_for = Some(commit.oid.clone());
        self.commit_overview = None;
        self.overview_scroll.set(0);
        if let Err(e) = self.submit_job(AsyncJob::LoadCommitOverview { oid: commit.oid }) {
            self.error = Some(e.to_string());
        }
    }

    /// Space: stage unless already fully staged (then unstage). When the
    /// highlight sits on a directory header, open (`▼`, `dir_cursor`) or
    /// collapsed (the selected file is hidden inside it), the whole
    /// directory is staged/unstaged so its files are ready to commit
    /// together.
    fn toggle_stage(&mut self) {
        if let Some(dir) = self.cursor_dir().map(str::to_string) {
            self.toggle_stage_dir(&dir);
            return;
        }
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        if let Some(dir) = self.collapsed_dir_for(&file.path) {
            self.toggle_stage_dir(&dir);
        } else {
            self.toggle_stage_file(&file);
        }
    }

    /// Shallowest collapsed ancestor of `path` (the header the UI
    /// highlights when this file is hidden), or `None` when visible.
    /// Mirrors the highlight fallback in `ui.rs`.
    fn collapsed_dir_for(&self, path: &str) -> Option<String> {
        ancestors_of(path)
            .into_iter()
            .find(|dir| self.collapsed.contains(dir.as_str()))
    }

    /// Space on one visible file: stage unless already fully staged.
    fn toggle_stage_file(&mut self, file: &StatusEntry) {
        if file.state == FileState::Conflicted {
            // Staging is how git is told a conflict is resolved; the
            // worker refuses while the markers are still in the file.
            if let Err(e) = self.submit_job(AsyncJob::ResolveFile {
                path: file.path.clone(),
            }) {
                self.error = Some(e.to_string());
            }
            return;
        }
        if file.state == FileState::Clean {
            self.error = Some(format!("{} is unchanged — nothing to stage", file.path));
            return;
        }
        let job = if file.state == FileState::Staged {
            AsyncJob::UnstageFile {
                path: file.path.clone(),
            }
        } else {
            AsyncJob::StageFile {
                path: file.path.clone(),
            }
        };
        if let Err(e) = self.submit_job(job) {
            self.error = Some(e.to_string());
        }
    }

    /// Space on a directory header: stage every stageable file
    /// beneath it, or unstage them all when every one is already staged.
    /// Conflicted files abort the whole directory like the single-file
    /// case; clean files are skipped silently.
    fn toggle_stage_dir(&mut self, dir: &str) {
        let prefix = format!("{dir}/");
        let under: Vec<(String, FileState)> = self
            .file_list
            .iter()
            .filter(|e| e.path.starts_with(&prefix))
            .map(|e| (e.path.clone(), e.state))
            .collect();
        if let Some((path, _)) = under.iter().find(|(_, s)| *s == FileState::Conflicted) {
            self.error = Some(format!("conflicted: resolve markers in {path} first"));
            return;
        }
        let to_stage: Vec<String> = under
            .iter()
            .filter(|(_, s)| !matches!(s, FileState::Clean | FileState::Staged))
            .map(|(p, _)| p.clone())
            .collect();
        if !to_stage.is_empty() {
            for path in to_stage {
                if let Err(e) = self.submit_job(AsyncJob::StageFile { path }) {
                    self.error = Some(e.to_string());
                    return;
                }
            }
            return;
        }
        let staged: Vec<String> = under
            .iter()
            .filter(|(_, s)| *s == FileState::Staged)
            .map(|(p, _)| p.clone())
            .collect();
        if !staged.is_empty() {
            for path in staged {
                if let Err(e) = self.submit_job(AsyncJob::UnstageFile { path }) {
                    self.error = Some(e.to_string());
                    return;
                }
            }
            return;
        }
        self.error = Some(format!("nothing to stage under {dir}/"));
    }

    /// `d`: discard all changes in the selected file (staged and unstaged),
    /// restoring it to HEAD; untracked files are deleted. When the
    /// highlight sits on a directory header, open (`▼`, `dir_cursor`) or
    /// collapsed (the selected file is hidden inside it), every changed
    /// file beneath it is discarded so the folder is clean again.
    fn plan_discard_selected(&self) -> Result<(DangerAction, String), String> {
        if let Some(dir) = self.cursor_dir() {
            return self.plan_discard_dir(dir);
        }
        let Some(file) = self.selected_file() else {
            return Err("no file selected".into());
        };
        match self.collapsed_dir_for(&file.path) {
            Some(dir) => self.plan_discard_dir(&dir),
            None => Self::plan_discard_file(file),
        }
    }

    /// One visible file: discard its changes (or delete it when
    /// untracked / staged-new).
    fn plan_discard_file(file: &StatusEntry) -> Result<(DangerAction, String), String> {
        let prompt = match file.state {
            FileState::Conflicted => {
                return Err(format!(
                    "conflicted: resolve markers in {} first",
                    file.path
                ))
            }
            FileState::Clean => {
                return Err(format!("{} is unchanged — nothing to discard", file.path))
            }
            FileState::Untracked => format!("Delete untracked file {}?", file.path),
            _ => format!("Discard all changes to {}?", file.path),
        };
        Ok((DangerAction::DiscardFiles(vec![file.path.clone()]), prompt))
    }

    /// A directory header: every changed file beneath it. A conflicted
    /// file refuses the whole directory like the single-file case; clean
    /// files are skipped silently.
    fn plan_discard_dir(&self, dir: &str) -> Result<(DangerAction, String), String> {
        let prefix = format!("{dir}/");
        let under: Vec<&StatusEntry> = self
            .file_list
            .iter()
            .filter(|e| e.path.starts_with(&prefix))
            .collect();
        if let Some(e) = under.iter().find(|e| e.state == FileState::Conflicted) {
            return Err(format!("conflicted: resolve markers in {} first", e.path));
        }
        let dirty: Vec<String> = under
            .iter()
            .filter(|e| e.state != FileState::Clean)
            .map(|e| e.path.clone())
            .collect();
        if dirty.is_empty() {
            return Err(format!("nothing to discard under {dir}/"));
        }
        let untracked = under
            .iter()
            .filter(|e| e.state == FileState::Untracked)
            .count();
        let mut prompt = format!(
            "Discard changes to {} file{} under {dir}/?",
            dirty.len(),
            if dirty.len() == 1 { "" } else { "s" }
        );
        if untracked > 0 {
            prompt.push_str(&format!(" {untracked} untracked will be deleted."));
        }
        Ok((DangerAction::DiscardFiles(dirty), prompt))
    }

    /// The file open in the fullscreen diff.
    fn plan_discard_loaded(&self) -> Result<(DangerAction, String), String> {
        let Some((path, _)) = self.diff_for.clone() else {
            return Err("no diff loaded".into());
        };
        // The file list knows the state (clean vs changed), including
        // mode-only changes shown via the whole-file fallback.
        match self.file_list.iter().find(|f| f.path == path) {
            Some(file) => Self::plan_discard_file(file),
            None => {
                let prompt = format!("Discard all changes to {path}?");
                Ok((DangerAction::DiscardFiles(vec![path]), prompt))
            }
        }
    }

    /// The hunk under the cursor (preview or fullscreen): reverting just
    /// that hunk's region leaves every other hunk untouched. Unstaged
    /// hunks revert the workdir toward the index; staged hunks revert the
    /// index toward HEAD.
    fn plan_restore_hunk(&self) -> Result<(DangerAction, String), String> {
        if self.show_markdown_preview() {
            return Err("leave the Markdown preview (m) to restore a hunk".into());
        }
        let Some((path, staged)) = self.diff_for.clone() else {
            return Err("no diff loaded".into());
        };
        if self.diff_whole_file {
            return Err("nothing to restore: file has no hunks".into());
        }
        let Some(diff) = self.diff.as_ref() else {
            return Err("no diff loaded".into());
        };
        if diff.binary {
            return Err("cannot restore hunk of binary file".into());
        }
        let has_changes = diff.hunks.get(self.hunk).is_some_and(|h| {
            h.lines.iter().any(|l| {
                l.kind == git_tui_core::diff::LineKind::Add
                    || l.kind == git_tui_core::diff::LineKind::Del
            })
        });
        if !has_changes {
            return Err("nothing to restore in this hunk".into());
        }
        let prompt = format!(
            "Restore hunk {} of {path}? Its changes are thrown away.",
            self.hunk + 1
        );
        Ok((
            DangerAction::RestoreHunk {
                path,
                hunk_index: self.hunk,
                staged,
            },
            prompt,
        ))
    }

    fn plan_delete_branch(&self) -> Result<(DangerAction, String), String> {
        let Some(branch) = self.selected_branch() else {
            return Err("no branch selected".into());
        };
        let prompt = format!(
            "Delete branch {}? This is a forced delete, merged or not.",
            branch.name
        );
        Ok((DangerAction::DeleteBranch(branch.name), prompt))
    }

    fn plan_force_push(&self) -> Result<(DangerAction, String), String> {
        let branch = match self.status.as_ref() {
            Some(st) if !st.branch.starts_with('(') => st.branch.clone(),
            Some(_) => return Err("nothing to push: HEAD is not on a branch".into()),
            None => return Err("still loading — try again in a moment".into()),
        };
        let Some(upstream) = self.sync.as_ref().and_then(|s| s.upstream.clone()) else {
            return Err("no upstream to force-push to: push normally first (P)".into());
        };
        let (remote, remote_branch) = split_upstream(&upstream);
        // The tracked branch may have another name than the local one.
        let refspec = if remote_branch.is_empty() || remote_branch == branch {
            branch.clone()
        } else {
            format!("{branch}:{remote_branch}")
        };
        let prompt = format!(
            "Force-push {branch} over {upstream}? Commits there that you do not have are \
             lost (refused if someone pushed since your last fetch)."
        );
        Ok((
            DangerAction::ForcePush {
                remote: remote.to_string(),
                branch: refspec,
            },
            prompt,
        ))
    }

    fn plan_drop_stash(&self) -> Result<(DangerAction, String), String> {
        let Some(entry) = self.selected_stash() else {
            return Err("no stash selected".into());
        };
        let prompt = format!("Drop stash \"{}\"?", entry.message);
        Ok((DangerAction::DropStash(entry.index), prompt))
    }

    /// Ask to throw work away. Resolves the request against the current
    /// selection, then either opens the confirm box naming the target or,
    /// with `[ui] confirm_destructive = false`, runs it at once.
    pub(crate) fn request_danger(&mut self, danger: Danger) {
        let plan = match danger {
            Danger::DiscardSelected => self.plan_discard_selected(),
            Danger::DiscardLoaded => self.plan_discard_loaded(),
            Danger::RestoreHunk => self.plan_restore_hunk(),
            Danger::DeleteBranch => self.plan_delete_branch(),
            Danger::DropStash => self.plan_drop_stash(),
            Danger::ForcePush => self.plan_force_push(),
            Danger::AbortOperation => match self.operation_in_progress() {
                Some(operation) => Ok((
                    DangerAction::AbortOperation,
                    format!(
                        "{operation}: abort it? Everything goes back to before it started; \
                         conflict resolutions made so far are lost."
                    ),
                )),
                None => Err("nothing to abort: no merge or rebase is in progress".into()),
            },
        };
        let (action, prompt) = match plan {
            Ok(plan) => plan,
            Err(message) => {
                self.error = Some(message);
                return;
            }
        };
        // Reverting a staged hunk only edits the index: the change stays
        // in the working tree, so there is nothing to lose.
        let harmless = matches!(action, DangerAction::RestoreHunk { staged: true, .. });
        if self.confirm_destructive && !harmless {
            self.confirm = Some(Confirm {
                action,
                prompt,
                back: self.mode,
                opened: Instant::now(),
            });
            self.mode = Mode::Confirm;
        } else if self.mutation_in_flight() {
            // The list is about to change under the cursor: a second press
            // would otherwise land on whatever moves into this row.
            self.notice = Some("wait: the previous change is still being applied".into());
        } else {
            self.run_danger(action);
        }
    }

    /// The question the confirm box is asking, while it is open.
    pub fn confirm_prompt(&self) -> Option<&str> {
        self.confirm.as_ref().map(|c| c.prompt.as_str())
    }

    /// The view the confirm box was opened over.
    pub fn confirm_back(&self) -> Mode {
        self.confirm.as_ref().map_or(Mode::Normal, |c| c.back)
    }

    fn accept_confirm(&mut self) {
        if self
            .confirm
            .as_ref()
            .is_some_and(|c| c.opened.elapsed() < CONFIRM_ARM)
        {
            return;
        }
        let Some(confirm) = self.confirm.take() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = confirm.back;
        self.run_danger(confirm.action);
    }

    fn cancel_confirm(&mut self) {
        self.mode = self.confirm.take().map_or(Mode::Normal, |c| c.back);
    }

    fn run_danger(&mut self, action: DangerAction) {
        let jobs: Vec<AsyncJob> = match action {
            // One job: one undo puts the whole set back.
            DangerAction::DiscardFiles(paths) => vec![AsyncJob::DiscardFiles { paths }],
            DangerAction::ForcePush { remote, branch } => {
                self.syncing = Some("force-pushing…".into());
                vec![AsyncJob::PushForce { remote, branch }]
            }
            DangerAction::AbortOperation => vec![AsyncJob::AbortOperation],
            DangerAction::RestoreHunk {
                path,
                hunk_index,
                staged,
            } => {
                self.hunk_after_reload = Some((path.clone(), hunk_index));
                vec![AsyncJob::DiscardHunk {
                    path,
                    hunk_index,
                    staged,
                }]
            }
            DangerAction::DeleteBranch(name) => vec![AsyncJob::DeleteBranch { name }],
            DangerAction::DropStash(index) => vec![AsyncJob::StashDrop { index }],
        };
        for job in jobs {
            if let Err(e) = self.submit_job(job) {
                self.error = Some(e.to_string());
                return;
            }
        }
    }

    fn submit_commit(&mut self) {
        if self.generating {
            self.notice = Some("still generating a message…".into());
            return;
        }
        if self.draft.trim().is_empty() {
            self.notice = Some("type a commit message first".into());
            return;
        }
        let message = self.draft.clone();
        let job = if self.amending {
            AsyncJob::Amend { message }
        } else {
            AsyncJob::Commit { message }
        };
        match self.submit_job(job) {
            Ok(()) => {
                self.mode = Mode::Normal;
                // Kept until the commit is known to have succeeded.
                self.commit_stash = std::mem::take(&mut self.draft);
                self.draft_cursor = 0;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// `c`: open the commit box. A message left there earlier (Esc, or a
    /// commit that failed) is still in it.
    fn open_commit_box(&mut self) {
        self.amending = false;
        self.mode = Mode::Committing;
        self.draft = self.commit_stash.clone();
        self.move_draft_end();
        if !self.draft.is_empty() {
            self.notice = Some("your earlier message is back in the box".into());
        }
    }

    /// Stage the selected hunk of the loaded (unstaged) diff.
    fn stage_selected_hunk(&mut self) {
        let Some((path, staged)) = self.diff_for.clone() else {
            self.error = Some("no diff loaded".into());
            return;
        };
        if staged {
            self.error = Some(
                "hunk is already staged — switch to status (1) and space toggles the file".into(),
            );
            return;
        }
        let Some(diff) = self.diff.as_ref() else {
            self.error = Some("no diff loaded".into());
            return;
        };
        // Whole-file views (clean files) have no changes to stage.
        let has_changes = diff.hunks.get(self.hunk).is_some_and(|h| {
            h.lines.iter().any(|l| {
                l.kind == git_tui_core::diff::LineKind::Add
                    || l.kind == git_tui_core::diff::LineKind::Del
            })
        });
        if !has_changes {
            self.error = Some("nothing to stage in this hunk".into());
            return;
        }
        // The next hunk takes this one's index once it is staged.
        self.hunk_after_reload = Some((path.clone(), self.hunk));
        let job = AsyncJob::StageHunk {
            path,
            hunk_index: self.hunk,
        };
        if let Err(e) = self.submit_job(job) {
            self.error = Some(e.to_string());
        }
    }

    /// Which diff belongs to the selected file: fully staged files show the
    /// staged diff, clean files show the whole file, everything else the
    /// unstaged one.
    fn diff_target(&self) -> Option<(String, bool)> {
        let file = self.selected_file()?;
        let staged = match file.state {
            FileState::Staged => true,
            // Both halves exist: the unstaged one unless `S` asked.
            FileState::BothStagedAndUnstaged => {
                self.staged_side_for.as_deref() == Some(file.path.as_str())
            }
            _ => false,
        };
        Some((file.path.clone(), staged))
    }

    /// `S`: flip between the staged and the unstaged half of a partly
    /// staged file (the only kind that has both).
    fn toggle_staged_side(&mut self) {
        let Some(path) = self
            .selected_file()
            .filter(|f| f.state == FileState::BothStagedAndUnstaged)
            .map(|f| f.path.clone())
        else {
            self.notice =
                Some("only a partly staged file has a staged and an unstaged side".into());
            return;
        };
        let showing_staged = self.staged_side_for.as_deref() == Some(path.as_str());
        self.staged_side_for = if showing_staged { None } else { Some(path) };
        self.notice = Some(
            if !showing_staged {
                "showing what is staged (S flips back, x unstages a hunk)"
            } else {
                "showing what is not staged yet"
            }
            .into(),
        );
        self.maybe_load_diff();
    }

    /// Whether the selected file needs a whole-file view (clean).
    fn selected_is_clean(&self) -> bool {
        self.selected_file()
            .is_some_and(|f| f.state == FileState::Clean)
    }
    /// Request the selected file's diff unless it is already loaded/loading.
    /// Automatic: files with no changes load the whole workdir file (their
    /// diff would be empty); changed files load the staged/unstaged diff.
    /// A listed file whose diff arrives empty (e.g. a mode-only change)
    /// falls back to the whole file instead of showing "(no changes)".
    fn maybe_load_diff(&mut self) {
        if self.commit_view.is_some() {
            return;
        }
        let target = self.diff_target();
        // A new path drops the empty-diff fallback; it applies per file.
        let path_changed =
            target.as_ref().map(|(p, _)| p) != self.diff_for.as_ref().map(|(p, _)| p);
        if path_changed {
            self.fallback_whole_file = false;
        }
        let whole = self.selected_is_clean() || self.fallback_whole_file;
        if target == self.diff_for && whole == self.diff_whole_file {
            return;
        }
        self.diff_for = target.clone();
        // Stale view: show loading until the fresh diff arrives.
        self.set_diff(None);
        self.hunk = 0;
        self.cursor = 0;
        self.cursor_col = 0;
        self.diff_scroll = 0;
        self.diff_whole_file = whole;
        if let Some((path, staged)) = target {
            let job = if whole {
                AsyncJob::LoadFile { path }
            } else {
                AsyncJob::LoadDiff { path, staged }
            };
            if let Err(e) = self.submit_job(job) {
                self.error = Some(e.to_string());
            }
        }
    }

    /// Reload the [5] diff preview from the *fresh* status after a
    /// mutation (stash push/pop, stage, commit, …). It recomputes the
    /// target (path, staged flag, whole-file vs diff) instead of
    /// re-requesting a stale `diff_for`, so a stash that cleans the tree
    /// shows the whole file and a pop shows the restored hunks — no flash
    /// of wrong content, no wasted worker roundtrip.
    fn force_reload_diff_for_fresh_status(&mut self) {
        if self.commit_view.is_some() {
            return self.load_commit_view_file();
        }
        let target = self.diff_target();
        let path_changed =
            target.as_ref().map(|(p, _)| p) != self.diff_for.as_ref().map(|(p, _)| p);
        if path_changed {
            self.fallback_whole_file = false;
        }
        let whole = self.selected_is_clean() || self.fallback_whole_file;
        self.diff_for = target.clone();
        // Stale view: show loading until the fresh diff arrives.
        self.set_diff(None);
        self.hunk = 0;
        self.cursor = 0;
        self.cursor_col = 0;
        self.diff_scroll = 0;
        self.diff_whole_file = whole;
        if let Some((path, staged)) = target {
            let job = if whole {
                AsyncJob::LoadFile { path }
            } else {
                AsyncJob::LoadDiff { path, staged }
            };
            if let Err(e) = self.submit_job(job) {
                self.error = Some(e.to_string());
            }
        }
        // Markdown follows the same fresh target.
        self.md_for = None;
        self.md_text = None;
        self.maybe_load_markdown();
    }

    fn branch_count(&self) -> usize {
        self.branches.as_ref().map(|b| b.len()).unwrap_or(0)
    }

    fn selected_branch(&self) -> Option<BranchInfo> {
        self.branches
            .as_ref()?
            .get(
                self.branch_selected
                    .min(self.branch_count().saturating_sub(1)),
            )
            .cloned()
    }

    /// Request the branch list unless already loaded/loading.
    fn maybe_load_branches(&mut self) {
        if self.focus == Focus::Branches && self.branches.is_none() {
            if let Err(e) = self.submit_job(AsyncJob::ListBranches) {
                self.error = Some(e.to_string());
            }
        }
    }

    fn reload_branches(&mut self) {
        if self.branches.is_some() {
            // Forget the snapshot so the fresh list replaces it wholesale.
            self.branches = None;
            if let Err(e) = self.submit_job(AsyncJob::ListBranches) {
                self.error = Some(e.to_string());
            }
        }
    }

    /// History depth the log panel starts with.
    const LOG_LIMIT: usize = 100;

    /// `L` in the commits panel: ask for more history.
    fn load_more_log(&mut self) {
        if self.log_count() < self.log_limit {
            self.notice = Some(format!("that is all of it: {} commits", self.log_count()));
            return;
        }
        self.log_limit += 400;
        // Keep the list on screen while the longer one loads.
        if let Err(e) = self.submit_job(AsyncJob::ListLog {
            limit: self.log_limit,
        }) {
            self.error = Some(e.to_string());
        } else {
            self.notice = Some(format!("loading up to {} commits…", self.log_limit));
        }
    }

    /// First line of the commit overview to show; the renderer passes the
    /// last valid offset and the stored one is pulled back to it.
    pub(crate) fn clamp_overview_scroll(&self, max: usize) -> usize {
        let off = self.overview_scroll.get().min(max);
        self.overview_scroll.set(off);
        off
    }

    fn overview_scroll_by(&mut self, delta: isize) {
        let next = (self.overview_scroll.get() as isize)
            .saturating_add(delta)
            .max(0);
        self.overview_scroll.set(next as usize);
    }

    /// Request history unless already loaded/loading.
    fn maybe_load_log(&mut self) {
        if self.focus == Focus::Log && self.log.is_none() {
            if let Err(e) = self.submit_job(AsyncJob::ListLog {
                limit: self.log_limit,
            }) {
                self.error = Some(e.to_string());
            }
        }
    }

    fn reload_log(&mut self) {
        if self.log.is_some() {
            self.log = None;
            if let Err(e) = self.submit_job(AsyncJob::ListLog {
                limit: self.log_limit,
            }) {
                self.error = Some(e.to_string());
            }
        }
    }

    fn submit_new_branch(&mut self) {
        if self.draft.trim().is_empty() {
            return;
        }
        let job = AsyncJob::CreateBranch {
            name: self.draft.clone(),
        };
        match self.submit_job(job) {
            Ok(()) => {
                self.mode = Mode::Normal;
                self.draft.clear();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn checkout_selected_branch(&mut self) {
        let Some(branch) = self.selected_branch() else {
            return;
        };
        if let Err(e) = self.submit_job(AsyncJob::CheckoutBranch { name: branch.name }) {
            self.error = Some(e.to_string());
        }
    }

    fn stash_count(&self) -> usize {
        self.stash.as_ref().map(|s| s.len()).unwrap_or(0)
    }

    fn selected_stash(&self) -> Option<StashEntry> {
        self.stash
            .as_ref()?
            .get(
                self.stash_selected
                    .min(self.stash_count().saturating_sub(1)),
            )
            .cloned()
    }

    /// Request the stash list unless already loaded/loading.
    fn maybe_load_stash(&mut self) {
        if self.focus == Focus::Stash && self.stash.is_none() {
            if let Err(e) = self.submit_job(AsyncJob::ListStash) {
                self.error = Some(e.to_string());
            }
        }
    }

    fn reload_stash(&mut self) {
        if self.stash.is_some() {
            self.stash = None;
            if let Err(e) = self.submit_job(AsyncJob::ListStash) {
                self.error = Some(e.to_string());
            }
        }
    }

    /// `/`: open the fuzzy file finder on the browsable tree. Works
    /// from `Normal` and from `FullDiff`; Enter/Esc return there.
    fn open_finder(&mut self) {
        self.finder_return = self.mode;
        self.mode = Mode::FindFile;
        self.draft.clear();
        self.finder_selected = self.selected.min(self.file_count().saturating_sub(1));
    }

    fn finder_move(&mut self, delta: isize) {
        let n = self.finder_matches().len();
        if n == 0 {
            self.finder_selected = 0;
            return;
        }
        let cur = self.finder_selected.min(n - 1) as isize;
        self.finder_selected = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    /// Enter in the finder: jump the file cursor to the chosen match,
    /// expanding any collapsed ancestors so the match is actually visible,
    /// and return where the finder was opened from (staying fullscreen
    /// when opened fullscreen, with the new file's diff loading).
    fn submit_finder(&mut self) {
        let matches = self.finder_matches();
        let Some(&index) = matches.get(self.finder_cursor()) else {
            return;
        };
        self.selected = index;
        self.dir_cursor = None;
        self.expand_selected();
        self.focus = Focus::Status;
        self.mode = self.finder_return;
        self.draft.clear();
        self.finder_selected = 0;
        self.maybe_load_diff();
    }

    fn submit_stash_push(&mut self) {
        if self.draft.trim().is_empty() {
            return;
        }
        let job = AsyncJob::StashPush {
            message: self.draft.clone(),
        };
        match self.submit_job(job) {
            Ok(()) => {
                self.mode = Mode::Normal;
                self.draft.clear();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn pop_selected_stash(&mut self) {
        let Some(entry) = self.selected_stash() else {
            return;
        };
        if let Err(e) = self.submit_job(AsyncJob::StashPop { index: entry.index }) {
            self.error = Some(e.to_string());
        }
    }

    /// `p`, lazygit-style: pull the current branch (`git pull`, honoring
    /// the user's pull.rebase/pull.ff config). Runs on the worker thread;
    /// the status panel shows "pulling…" until it lands or fails.
    /// `f`: fetch, so the ahead/behind counts reflect the remote.
    fn start_fetch(&mut self) {
        self.syncing = Some("fetching…".into());
        if let Err(e) = self.submit_job(AsyncJob::Fetch) {
            self.syncing = None;
            self.error = Some(e.to_string());
        }
    }

    /// `u`: put back what the last discard threw away.
    fn undo_discard(&mut self) {
        if let Err(e) = self.submit_job(AsyncJob::UndoDiscard) {
            self.error = Some(e.to_string());
        }
    }

    /// Reword (and add staged changes to) the last commit: the commit box
    /// opens with its message once the worker has read it.
    fn begin_amend(&mut self) {
        if let Err(e) = self.submit_job(AsyncJob::LoadLastCommitMessage) {
            self.error = Some(e.to_string());
        }
    }

    /// The commit box is amending the last commit.
    pub fn is_amending(&self) -> bool {
        self.amending && self.mode == Mode::Committing
    }

    fn start_pull(&mut self) {
        self.syncing = Some("pulling…".into());
        if let Err(e) = self.submit_job(AsyncJob::Pull) {
            self.syncing = None;
            self.error = Some(e.to_string());
        }
    }

    /// `P`, lazygit-style: push the current branch. With an upstream a
    /// confirmation box shows the remote/branch first (Enter pushes, Esc
    /// cancels); without one it asks for the remote (`-u`); with no
    /// remotes at all it asks for the `origin` URL (publish).
    fn start_push(&mut self) {
        let Some(branch) = self.status.as_ref().map(|st| st.branch.clone()) else {
            self.error = Some("still loading — try again in a moment".into());
            return;
        };
        if branch.starts_with('(') {
            self.error =
                Some("nothing to push: HEAD is not on a branch (check one out first)".into());
            return;
        }
        let Some(sync) = self.sync.clone() else {
            self.reload_sync();
            self.error = Some("loading remotes — press P again in a moment".into());
            return;
        };
        self.sync_return = self.mode;
        if sync.remotes.is_empty() {
            self.mode = Mode::SetRemote;
            self.draft.clear();
            self.draft_cursor = 0;
        } else if let Some(upstream) = sync.upstream {
            let (remote, _) = split_upstream(&upstream);
            self.pending_push = Some((remote.to_string(), branch));
            self.mode = Mode::ConfirmPush;
        } else {
            self.mode = Mode::SetUpstream;
            self.draft = "origin".to_string();
            self.move_draft_end();
        }
    }

    /// Push details awaiting confirmation, if `P` opened the confirm box.
    pub fn pending_push(&self) -> Option<(String, String)> {
        self.pending_push.clone()
    }

    #[cfg(test)]
    pub(crate) fn set_pending_push_for_test(&mut self, remote: &str, branch: &str) {
        self.pending_push = Some((remote.to_string(), branch.to_string()));
        self.mode = Mode::ConfirmPush;
    }

    /// Recorded while rendering the confirm-push modal: screen cell where
    /// its `[Push] [Cancel]` buttons start.
    pub(crate) fn set_confirm_btn(&self, pos: Option<(u16, u16)>) {
        self.confirm_btn.set(pos);
    }

    /// Enter in the push confirm box: push to the tracked upstream.
    fn confirm_push(&mut self) {
        let Some((remote, branch)) = self.pending_push.clone() else {
            self.mode = self.sync_return;
            return;
        };
        // The confirm box showed where it goes; the job resolves the
        // tracked branch itself, so a differently named upstream is pushed
        // to instead of a new remote branch being created.
        let _ = (remote, branch);
        match self.submit_job(AsyncJob::PushUpstream) {
            Ok(()) => {
                self.mode = self.sync_return;
                self.pending_push = None;
                self.syncing = Some("pushing…".into());
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Esc in the push confirm box: back out without pushing.
    fn cancel_push_confirm(&mut self) {
        self.mode = self.sync_return;
        self.pending_push = None;
    }

    /// Enter in `SetUpstream`: push `-u <remote> <branch>`.
    fn submit_push_upstream(&mut self) {
        let remote = self.draft.trim().to_string();
        if remote.is_empty() {
            return;
        }
        let Some(branch) = self.status.as_ref().map(|st| st.branch.clone()) else {
            self.error = Some("still loading — try again in a moment".into());
            return;
        };
        match self.submit_job(AsyncJob::Push {
            remote,
            branch,
            set_upstream: true,
        }) {
            Ok(()) => {
                self.mode = self.sync_return;
                self.draft.clear();
                self.draft_cursor = 0;
                self.syncing = Some("pushing…".into());
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Enter in `SetRemote`: point `origin` at the typed URL and push `-u`.
    /// The "not yet on GitHub" flow: create the empty repo on the host,
    /// paste its URL here, and the branch is published.
    fn submit_publish(&mut self) {
        let url = self.draft.trim().to_string();
        if url.is_empty() {
            return;
        }
        let Some(branch) = self.status.as_ref().map(|st| st.branch.clone()) else {
            self.error = Some("still loading — try again in a moment".into());
            return;
        };
        match self.submit_job(AsyncJob::Publish {
            remote: "origin".to_string(),
            url,
            branch,
        }) {
            Ok(()) => {
                self.mode = self.sync_return;
                self.draft.clear();
                self.draft_cursor = 0;
                self.syncing = Some("publishing…".into());
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Refresh upstream tracking state (cheap local reads, no network).
    fn reload_sync(&mut self) {
        for job in [AsyncJob::LoadSync, AsyncJob::LoadRepoState] {
            if let Err(e) = self.submit_job(job) {
                self.error = Some(e.to_string());
            }
        }
    }

    /// Operation in progress, as a heading for the menu and status line.
    pub fn operation_in_progress(&self) -> Option<&'static str> {
        match self.repo_state {
            RepoState::Clean => None,
            RepoState::Merge => Some("Merge in progress"),
            RepoState::Rebase => Some("Rebase in progress"),
            RepoState::CherryPick => Some("Cherry-pick in progress"),
            RepoState::Revert => Some("Revert in progress"),
            RepoState::Bisect => Some("Bisect in progress"),
            RepoState::Other => Some("Operation in progress"),
        }
    }

    /// Drain finished jobs without blocking (event loop calls this every
    /// frame). A [`AsyncResult::MutationDone`] invalidates the snapshot, so
    /// it refreshes status and reloads the focused diff rather than patching
    /// either incrementally.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some((id, result)) = self.queue.try_recv_tagged() {
            let pending = self.inflight.remove(&id).unwrap_or(Pending::Other);
            self.apply_result(result, pending);
            changed = true;
        }
        // After a mutation the [5] preview waits for the fresh status
        // (see `pending_diff_reload`): loading the old target now would
        // flash stale content and waste a worker roundtrip.
        if !self.pending_diff_reload {
            self.maybe_load_diff();
            self.maybe_load_markdown();
        }
        self.maybe_load_commit_overview();
        changed
    }

    /// Whether the worker still owes a result for a submitted job.
    pub fn busy(&self) -> bool {
        self.queue.has_pending()
    }

    /// Submit a job and remember what kind it is until its result arrives.
    fn submit_job(&mut self, job: AsyncJob) -> Result<(), GitError> {
        let kind = match &job {
            AsyncJob::Commit { .. } | AsyncJob::Amend { .. } => Pending::Commit,
            AsyncJob::Push { .. }
            | AsyncJob::Pull
            | AsyncJob::Fetch
            | AsyncJob::PushUpstream
            | AsyncJob::PushForce { .. }
            | AsyncJob::Publish { .. } => Pending::Sync,
            AsyncJob::DiscardFile { .. }
            | AsyncJob::DiscardFiles { .. }
            | AsyncJob::DiscardHunk { .. } => Pending::Discard,
            AsyncJob::GenerateCommitMessage { .. } => Pending::Generate,
            AsyncJob::StageHunk { .. }
            | AsyncJob::StageFile { .. }
            | AsyncJob::ResolveFile { .. }
            | AsyncJob::UnstageFile { .. }
            | AsyncJob::UndoDiscard
            | AsyncJob::AbortOperation
            | AsyncJob::ContinueOperation
            | AsyncJob::CreateBranch { .. }
            | AsyncJob::CheckoutBranch { .. }
            | AsyncJob::DeleteBranch { .. }
            | AsyncJob::StashPush { .. }
            | AsyncJob::StashPop { .. }
            | AsyncJob::StashDrop { .. } => Pending::Mutation,
            _ => Pending::Other,
        };
        let id = self.queue.submit_tagged(job)?;
        if kind != Pending::Other {
            self.inflight.insert(id, kind);
        }
        Ok(())
    }

    /// A change to the repository has been queued and not yet applied.
    fn mutation_in_flight(&self) -> bool {
        self.inflight
            .values()
            .any(|kind| matches!(kind, Pending::Mutation | Pending::Discard | Pending::Commit))
    }

    /// A new action dismisses the previous error/notice; failures of the
    /// new action arrive later via `poll` and replace them.
    fn clear_messages(&mut self) {
        self.error = None;
        self.notice = None;
        self.error_sticky = false;
    }

    /// Everything the repository shows, reloaded: after any job that
    /// changed it, or that failed part-way and may have.
    fn reload_after_change(&mut self) {
        self.refresh();
        // Defer the [5] preview (+ Markdown) until the fresh status lands:
        // stash push/pop changes the target itself (clean <-> dirty,
        // whole-file <-> hunks). Show loading now so stale hunks never
        // linger.
        self.pending_diff_reload = true;
        self.set_diff(None);
        self.hunk = 0;
        self.cursor = 0;
        self.cursor_col = 0;
        self.diff_scroll = 0;
        self.md_text = None;
        self.reload_branches();
        self.reload_log();
        self.reload_stash();
        self.reload_sync();
    }

    /// Feed one result in as if a job of no particular kind produced it.
    #[cfg(test)]
    fn apply(&mut self, result: AsyncResult) {
        self.apply_result(result, Pending::Other);
    }

    fn apply_result(&mut self, result: AsyncResult, pending: Pending) {
        match result {
            AsyncResult::Status(st) => {
                self.status = Some(st);
                self.rebuild_file_list();
                if !self.error_sticky {
                    self.error = None;
                }
                if self.pending_diff_reload {
                    self.pending_diff_reload = false;
                    self.force_reload_diff_for_fresh_status();
                }
            }
            AsyncResult::Diff(d) => {
                // Drop overtaken loads: only the latest target counts
                // (jobs run FIFO, so a newer LoadDiff may follow).
                if self.diff_for.as_ref().is_some_and(|(p, _)| *p == d.path) {
                    if d.hunks.is_empty() && !d.binary && !self.diff_whole_file {
                        // Listed but no content diff (e.g. mode-only
                        // change): show the whole file automatically.
                        // Binary files skip this: their bytes are never
                        // drawn, the diff pane shows a notice instead.
                        self.fallback_whole_file = true;
                        self.set_diff(None);
                        self.hunk = 0;
                        self.diff_scroll = 0;
                        self.diff_whole_file = true;
                        let job = AsyncJob::LoadFile { path: d.path };
                        if let Err(e) = self.submit_job(job) {
                            self.error = Some(e.to_string());
                        }
                    } else {
                        let back_to = self
                            .hunk_after_reload
                            .take()
                            .filter(|(path, _)| *path == d.path)
                            .map(|(_, hunk)| hunk);
                        self.set_diff(Some(d));
                        if let Some(hunk) = back_to {
                            if self.hunk_count() > 0 {
                                self.select_hunk(hunk);
                            }
                        }
                        self.sync_hunk_to_cursor();
                        if self.show_markdown_preview() {
                            // The fresh content may be shorter: keep the
                            // offset inside the new content.
                            self.diff_scroll = self.diff_scroll.min(self.diff_preview_max());
                        } else {
                            self.snap_scroll_to_cursor();
                        }
                    }
                }
            }
            AsyncResult::Markdown { path, staged, text } => {
                if self
                    .md_for
                    .as_ref()
                    .is_some_and(|(p, s)| *p == path && *s == staged)
                {
                    self.md_text = Some(text);
                    self.diff_scroll = 0;
                }
            }
            AsyncResult::Branches(b) => {
                self.branch_selected = self.branch_selected.min(b.len().saturating_sub(1));
                self.branches = Some(b);
            }
            AsyncResult::Log(entries) => {
                self.log_selected = self.log_selected.min(entries.len().saturating_sub(1));
                self.log = Some(entries);
            }
            AsyncResult::CommitOverview(overview) => {
                // Drop overtaken loads: only the still-selected oid counts.
                if self.commit_overview_for.as_deref() == Some(overview.oid.as_str()) {
                    self.commit_overview = Some(overview);
                }
            }
            AsyncResult::Stash(entries) => {
                self.stash_selected = self.stash_selected.min(entries.len().saturating_sub(1));
                self.stash = Some(entries);
            }
            AsyncResult::SyncStatus(st) => {
                self.sync = Some(st);
            }
            AsyncResult::Models { id, models } => {
                // Only the newest request may write the row: an earlier
                // one can still land after the form moved on.
                if id == self.llm_models_request {
                    self.apply_llm_models(models);
                }
            }
            AsyncResult::ModelsError { id, error } => {
                if id == self.llm_models_request {
                    self.llm_models_loading = false;
                    self.llm_models.clear();
                    self.llm_models_error = Some(error);
                }
            }
            AsyncResult::GeneratedMessage(msg) => {
                self.generating = false;
                let started_from = self.gen_draft.take();
                // The user may have Esc'd while the network call was in
                // flight, or kept typing: only replace the text that was
                // there when they asked.
                if self.mode == Mode::Committing && started_from.as_deref() == Some(&self.draft) {
                    self.draft = msg;
                    self.move_draft_end();
                    self.error = None;
                } else if self.mode == Mode::Committing {
                    self.notice =
                        Some("generated message dropped: you edited the box meanwhile".into());
                } else if self.commit_stash.trim().is_empty() {
                    // Closed meanwhile with nothing typed: keep the result
                    // for the next time the box opens.
                    self.commit_stash = msg;
                }
            }
            AsyncResult::MutationDone => {
                match pending {
                    Pending::Sync => self.syncing = None,
                    // The message is in the history now.
                    Pending::Commit => self.commit_stash.clear(),
                    Pending::Discard => {
                        let key = crate::menu::key_hint(MenuAction::Undo, &self.keys);
                        self.notice = Some(format!("discarded ({key} puts it back)"));
                    }
                    _ => {}
                }
                self.reload_after_change();
            }
            AsyncResult::Undone(count) => {
                self.notice = Some(format!(
                    "put back {count} file{}",
                    if count == 1 { "" } else { "s" }
                ));
                self.reload_after_change();
            }
            AsyncResult::RepoState(state) => self.repo_state = state,
            AsyncResult::LastCommitMessage(message) => match message {
                Some(message) if self.mode == Mode::Normal => {
                    self.amending = true;
                    self.mode = Mode::Committing;
                    self.draft = message.trim_end().to_string();
                    self.move_draft_end();
                }
                Some(_) => {}
                None => self.error = Some("nothing to amend: no commits yet".into()),
            },
            AsyncResult::Superseded => {}
            AsyncResult::Error(e) => {
                // EmptyCommit on its own doesn't say how to fix it.
                let message = match e {
                    GitError::EmptyCommit => {
                        "nothing to commit: press space on a file to stage it, then c to commit"
                            .to_string()
                    }
                    _ => e.to_string(),
                };
                match pending {
                    Pending::Generate => {
                        self.generating = false;
                        self.gen_draft = None;
                    }
                    Pending::Sync => {
                        self.syncing = None;
                        // A pull that stopped on conflicts changed plenty.
                        self.reload_after_change();
                    }
                    Pending::Commit => {
                        // Give the message back instead of losing it.
                        if self.mode == Mode::Normal && !self.commit_stash.is_empty() {
                            let amending = self.amending;
                            self.open_commit_box();
                            self.amending = amending;
                        }
                        self.reload_after_change();
                    }
                    Pending::Mutation | Pending::Discard => self.reload_after_change(),
                    // A failed load leaves status unchanged: drop any
                    // deferred reload so the preview recovers via the
                    // normal `maybe_load_diff` path on the next poll.
                    Pending::Other => self.pending_diff_reload = false,
                }
                self.error = Some(message);
                self.error_sticky = true;
            }
        }
    }
}

/// Identity of a model-fetch request: provider plus the base URL and key
/// that change which list comes back. The key itself is never included in
/// the string beyond a yes/no, so nothing secret reaches logs or tests.
fn llm_fetch_target(cfg: &LlmConfig) -> String {
    use std::hash::{Hash, Hasher};
    // A digest, so a different key is a different request without the key
    // itself ever being part of the string.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cfg.effective_api_key().hash(&mut hasher);
    format!(
        "{}|{}|{:x}",
        cfg.provider,
        cfg.base_url.clone().unwrap_or_default(),
        hasher.finish()
    )
}

/// Push `text` to the system clipboard with an OSC 52 escape. Terminal-
/// native (kitty, foot, alacritty, wezterm, tmux passthrough, SSH), no
/// windowing libraries needed. Returns whether the write succeeded.
fn yank_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    let encoded = base64_encode(text.as_bytes());
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b]52;c;{encoded}\x07")
        .and_then(|_| out.flush())
        .is_ok()
}

/// Minimal standard base64 (no external crate): 3 bytes -> 4 chars.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct Fixture {
        _dir: tempfile::TempDir,
        app: App,
    }

    fn init_repo() -> (tempfile::TempDir, git2::Repository) {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test User").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        // Git for Windows enables autocrlf system-wide; keep checkouts byte-exact.
        cfg.set_str("core.autocrlf", "false").unwrap();
        // Commits go through the git CLI: a global signing key or hooks
        // path must not reach into the tests.
        cfg.set_str("commit.gpgsign", "false").unwrap();
        cfg.set_str("core.hooksPath", ".git/no-hooks").unwrap();
        (dir, repo)
    }

    fn commit_file(repo: &git2::Repository, path: &str, contents: &str, msg: &str) {
        let full = repo.workdir().unwrap().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, contents).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(path)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = repo.signature().unwrap();
        let parents: Vec<git2::Commit> = match repo.head() {
            Ok(h) => vec![h.peel_to_commit().unwrap()],
            Err(_) => vec![],
        };
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &refs)
            .unwrap();
    }

    /// Settle until `pred` holds on the latest status (drives `poll`).
    fn wait_for(app: &mut App, mut pred: impl FnMut(&RepoStatus) -> bool) -> RepoStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(st) = app.status() {
                if pred(st) {
                    return st.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out; last: {:?}",
                app.status()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_error(app: &mut App) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(e) = app.error() {
                return e.to_string();
            }
            assert!(Instant::now() < deadline, "timed out waiting for error");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Repo with each named file committed then dirtied (all Unstaged).
    fn harness(names: &[&str]) -> Fixture {
        let (dir, repo) = init_repo();
        for name in names {
            commit_file(&repo, name, "base\n", "init");
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(repo.workdir().unwrap().join(name))
                .unwrap();
            f.write_all(b"dirty\n").unwrap();
        }
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(path).unwrap()),
        };
        let n = names.len();
        wait_for(&mut fx.app, |st| st.files.len() == n);
        fx
    }

    #[test]
    fn left_collapses_only_deepest_parent() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        fx.app.on_key(KeyCode::Left);
        assert!(
            fx.app.is_collapsed("src/nested"),
            "deepest parent should collapse"
        );
        assert!(
            !fx.app.is_collapsed("src"),
            "sibling subtree must stay expanded"
        );
    }

    #[test]
    fn folders_collapse_expand_and_skip_hidden_files() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        // Left collapses src/; the cursor stays as a hidden anchor so the
        // `▶ src/` header takes the highlight (cursor is on the folder).
        fx.app.on_key(KeyCode::Left);
        assert!(fx.app.is_collapsed("src"));
        // Down escapes the anchor to the next visible file.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "z.txt",
            "collapsed src/ must be skipped when moving down"
        );
        // Up from below steps onto the folded header (cursor back on the
        // folder), never showing its hidden files as rows.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "src/nested/b.rs",
            "moving up must stop on the collapsed header's anchor"
        );
        assert!(
            fx.app.is_hidden_index(fx.app.selected()),
            "anchor must be hidden so the header takes the highlight"
        );
        // Right on the header expands it back.
        fx.app.on_key(KeyCode::Right);
        assert!(
            !fx.app.is_collapsed("src"),
            "Right on the header must expand it"
        );
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "src/nested/b.rs",
            "expand restores the anchored file"
        );
        fx.app.on_key(KeyCode::Left);
        assert!(
            fx.app.is_collapsed("src/nested"),
            "Left collapses the deepest parent"
        );
        fx.app.on_key(KeyCode::Right);
        assert!(
            !fx.app.is_collapsed("src/nested"),
            "Right on the hidden anchor expands it back"
        );
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "src/nested/b.rs",
            "expand + collapse + expand restores visibility"
        );
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Esc);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected_file().unwrap().path, "z.txt");
    }

    #[test]
    fn finder_reveals_a_file_inside_collapsed_ancestors() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs"]);
        // Collapse src/ (both files fold away; the cursor stays anchored).
        fx.app.on_key(KeyCode::Left);
        assert!(fx.app.is_collapsed("src"));
        fx.app.on_key(KeyCode::Char('/'));
        for c in "b.rs".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        assert!(
            !fx.app.is_collapsed("src"),
            "jumping to a match must expand its ancestors"
        );
        fx.app.on_key(KeyCode::Down);
        assert_eq!(
            fx.app.selected_file().unwrap().path,
            "src/nested/b.rs",
            "at the end of the list Down clamps"
        );
    }

    #[test]
    fn up_escapes_hidden_anchor_to_nearest_visible() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        // Move onto b.rs, then collapse its deepest parent: the cursor is
        // now a hidden anchor on the `▶ nested/` header.
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        fx.app.on_key(KeyCode::Left);
        assert!(fx.app.is_collapsed("src/nested"));
        // Up escapes to the nearest visible file above, not into hiding.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        // Down jumps back over the folded region to z.txt.
        fx.app.on_key(KeyCode::Down);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.selected_file().unwrap().path, "z.txt");
    }

    #[test]
    fn selection_moves_with_jk_and_clamps() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        assert_eq!(fx.app.selected(), 0);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected(), 2);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected(), 2, "clamps at end");
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.selected(), 1);
        fx.app.on_key(KeyCode::Char('k'));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.selected(), 0, "clamps at start");
    }

    #[test]
    fn space_on_unstaged_stages_the_file() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        let entry = st.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, FileState::Staged);
    }

    #[test]
    fn space_on_staged_unstages_the_file() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Unstaged)
        });
        let entry = st.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, FileState::Unstaged);
    }

    #[test]
    fn space_on_collapsed_dir_stages_everything_under_it() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        // Collapse src/; the cursor stays on hidden src/a.rs, so the
        // header takes the highlight and Space acts on the whole dir.
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            let under: Vec<_> = st
                .files
                .iter()
                .filter(|e| e.path.starts_with("src/"))
                .collect();
            under.len() == 2 && under.iter().all(|e| e.state == FileState::Staged)
        });
        let outside = st.files.iter().find(|e| e.path == "z.txt").unwrap();
        assert_eq!(
            outside.state,
            FileState::Unstaged,
            "files outside the dir must be left alone"
        );
    }

    #[test]
    fn cursor_stops_on_open_folder_headers_both_ways() {
        // Rows: ▼ src/, a.rs, ▼ nested/, b.rs, c.rs, z.txt
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "src/nested/c.rs", "z.txt"]);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        assert_eq!(fx.app.cursor_dir(), None);
        // Down from a.rs stops on the open `nested/` header, previewing
        // its first file.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.cursor_dir(), Some("src/nested"));
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        // Down again enters the folder.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.cursor_dir(), None);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/nested/b.rs");
        // Up goes back onto the header, then a.rs, then the top `src/`.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), Some("src/nested"));
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), None);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), Some("src"));
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        // Clamps at the top.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), Some("src"));
    }

    #[test]
    fn space_on_open_folder_header_stages_only_that_folder() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "src/nested/c.rs", "z.txt"]);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.cursor_dir(), Some("src/nested"));
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .filter(|e| e.path.starts_with("src/nested/"))
                .all(|e| e.state == FileState::Staged)
        });
        for outside in ["src/a.rs", "z.txt"] {
            let e = st.files.iter().find(|e| e.path == outside).unwrap();
            assert_eq!(e.state, FileState::Unstaged, "{outside} must be left alone");
        }
        // The folder stays open and the cursor stays on its header, so a
        // second Space unstages it again.
        assert!(!fx.app.is_collapsed("src/nested"));
        assert_eq!(fx.app.cursor_dir(), Some("src/nested"));
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .filter(|e| e.path.starts_with("src/nested/"))
                .all(|e| e.state == FileState::Unstaged)
        });
    }

    #[test]
    fn left_on_open_header_folds_it_and_right_leaves_it_open() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_dir(), Some("src"));
        // Right on an already-open header changes nothing.
        fx.app.on_key(KeyCode::Right);
        assert!(!fx.app.is_collapsed("src") && !fx.app.is_collapsed("src/nested"));
        assert_eq!(fx.app.cursor_dir(), Some("src"));
        // Left folds exactly that folder (not the file's deepest parent);
        // the cursor stays on the now-collapsed header.
        fx.app.on_key(KeyCode::Left);
        assert!(fx.app.is_collapsed("src"));
        assert!(!fx.app.is_collapsed("src/nested"));
        assert!(fx.app.is_hidden_index(fx.app.selected()));
        // Right re-opens it.
        fx.app.on_key(KeyCode::Right);
        assert!(!fx.app.is_collapsed("src"));
    }

    #[test]
    fn space_on_collapsed_dir_unstages_when_everything_staged() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs"]);
        fx.app.on_key(KeyCode::Left);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files.len() == 2 && st.files.iter().all(|e| e.state == FileState::Staged)
        });
        // Everything under src/ is staged, so Space unstages the whole dir.
        fx.app.on_key(KeyCode::Char(' '));
        let st = wait_for(&mut fx.app, |st| {
            st.files.len() == 2 && st.files.iter().all(|e| e.state == FileState::Unstaged)
        });
        assert!(
            st.files.iter().all(|e| e.state == FileState::Unstaged),
            "got: {:?}",
            st.files
        );
    }

    #[test]
    fn space_on_collapsed_dir_with_conflict_reports_error() {
        let mut fx = harness(&[]);
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![
                StatusEntry {
                    path: "src/a.rs".into(),
                    state: FileState::Unstaged,
                },
                StatusEntry {
                    path: "src/b.rs".into(),
                    state: FileState::Conflicted,
                },
            ],
            tracked_files: vec!["src/a.rs".into(), "src/b.rs".into()],
        });
        fx.app.rebuild_file_list();
        fx.app.set_collapsed("src", true);
        fx.app.on_key(KeyCode::Char(' '));
        let err = fx.app.error().expect("expected conflict error");
        assert!(err.contains("conflicted"), "got: {err}");
        assert!(err.contains("src/b.rs"), "got: {err}");
    }

    #[test]
    fn space_on_collapsed_dir_with_nothing_to_stage_reports_error() {
        let mut fx = harness(&[]);
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![],
            tracked_files: vec!["src/a.rs".into()],
        });
        fx.app.rebuild_file_list();
        fx.app.set_collapsed("src", true);
        fx.app.on_key(KeyCode::Char(' '));
        let err = fx.app.error().expect("expected nothing-to-stage error");
        assert!(err.contains("under src/"), "got: {err}");
    }

    #[test]
    fn space_with_no_files_does_nothing() {
        let mut fx = harness(&[]);
        fx.app.on_key(KeyCode::Char(' '));
        assert!(fx.app.error().is_none());
        assert_eq!(fx.app.selected(), 0);
    }

    #[test]
    fn d_on_unstaged_discards_the_file() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Enter);
        let st = wait_for(&mut fx.app, |st| st.files.iter().all(|e| e.path != "a.txt"));
        assert!(
            st.files.iter().all(|e| e.path != "a.txt"),
            "got: {:?}",
            st.files
        );
    }

    #[test]
    fn d_on_staged_discards_index_and_workdir_changes() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Enter);
        let st = wait_for(&mut fx.app, |st| st.files.iter().all(|e| e.path != "a.txt"));
        assert!(
            st.files.iter().all(|e| e.path != "a.txt"),
            "got: {:?}",
            st.files
        );
    }

    #[test]
    fn d_on_untracked_deletes_the_file() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "base\n", "init");
        std::fs::write(repo.workdir().unwrap().join("new.txt"), "new\n").unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(path).unwrap()),
        };
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "new.txt" && e.state == FileState::Untracked)
        });
        // Changed files sort before clean ones, so the untracked file is
        // already selected.
        assert_eq!(fx.app.selected_file().unwrap().path, "new.txt");
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Enter);
        wait_for(&mut fx.app, |st| {
            st.files.iter().all(|e| e.path != "new.txt")
        });
        assert!(!fx._dir.path().join("new.txt").exists());
    }

    #[test]
    fn d_on_clean_file_reports_error() {
        let mut fx = harness(&[]);
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![],
            tracked_files: vec!["a.txt".into()],
        });
        fx.app.rebuild_file_list();
        assert_eq!(fx.app.selected_file().unwrap().path, "a.txt");
        fx.app.on_key(KeyCode::Char('d'));
        let err = fx.app.error().expect("expected nothing-to-discard error");
        assert!(err.contains("nothing to discard"), "got: {err}");
    }

    #[test]
    fn d_on_collapsed_dir_discards_everything_under_it() {
        let mut fx = harness(&["src/a.rs", "src/nested/b.rs", "z.txt"]);
        // Collapse src/; the cursor stays on hidden src/a.rs, so the
        // header takes the highlight and `d` acts on the whole dir.
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.selected_file().unwrap().path, "src/a.rs");
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Enter);
        let st = wait_for(&mut fx.app, |st| {
            st.files.iter().all(|e| !e.path.starts_with("src/"))
        });
        let outside = st.files.iter().find(|e| e.path == "z.txt").unwrap();
        assert_eq!(
            outside.state,
            FileState::Unstaged,
            "files outside the dir must be left alone"
        );
    }

    #[test]
    fn d_in_fullscreen_discards_the_loaded_file() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Enter);
        wait_for(&mut fx.app, |st| st.files.iter().all(|e| e.path != "a.txt"));
    }

    #[test]
    fn enter_with_no_files_does_not_open_fullscreen() {
        let mut fx = harness(&[]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn space_on_conflicted_asks_the_worker_to_resolve_it() {
        let mut fx = harness(&[]);
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![StatusEntry {
                path: "a.txt".into(),
                state: FileState::Conflicted,
            }],
            tracked_files: vec!["a.txt".into()],
        });
        fx.app.rebuild_file_list();
        // Not refused up front any more: staging is how a conflict is
        // marked resolved. (`d` on it is still refused.)
        fx.app.on_key(KeyCode::Char(' '));
        assert!(fx.app.error().is_none());
        assert!(fx.app.mutation_in_flight());
        fx.app.on_key(KeyCode::Char('d'));
        assert!(fx.app.error().unwrap().contains("conflicted"));
    }

    #[test]
    fn c_opens_modal_esc_cancels() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.mode(), Mode::Normal);
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.mode(), Mode::Committing);
        fx.app.on_key(KeyCode::Char('x'));
        assert_eq!(fx.app.draft(), "x");
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.draft(), "");
    }

    #[test]
    fn enter_in_modal_commits_and_returns_to_normal() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char('c'));
        for c in "my commit".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        let st = wait_for(&mut fx.app, |st| st.files.is_empty());
        assert_eq!(st.head_summary, "my commit");
    }

    #[test]
    fn enter_with_empty_message_does_not_submit() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Committing, "stays in modal");
    }

    #[test]
    fn commit_message_edits_mid_text_with_cursor() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        for c in "hello".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.draft_cursor(), 5);
        fx.app.on_key(KeyCode::Left);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.draft_cursor(), 3);
        // Backspace deletes before the cursor: "hello" -> "helo".
        fx.app.on_key(KeyCode::Backspace);
        assert_eq!(fx.app.draft(), "helo");
        assert_eq!(fx.app.draft_cursor(), 2);
        // Typing inserts at the cursor: "helo" -> "heXlo".
        fx.app.on_key(KeyCode::Char('X'));
        assert_eq!(fx.app.draft(), "heXlo");
        assert_eq!(fx.app.draft_cursor(), 3);
        // Delete removes under the cursor: "heXlo" -> "heXo".
        fx.app.on_key(KeyCode::Delete);
        assert_eq!(fx.app.draft(), "heXo");
        // Home/End jump; typing at the front and back works.
        fx.app.on_key(KeyCode::Home);
        fx.app.on_key(KeyCode::Char('!'));
        assert_eq!(fx.app.draft(), "!heXo");
        fx.app.on_key(KeyCode::End);
        fx.app.on_key(KeyCode::Char('?'));
        assert_eq!(fx.app.draft(), "!heXo?");
        assert_eq!(fx.app.draft_cursor(), 6);
    }

    #[test]
    fn commit_cursor_clamps_at_both_ends() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_key(KeyCode::Char('a'));
        fx.app.on_key(KeyCode::Left);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.draft_cursor(), 0, "clamps at start");
        fx.app.on_key(KeyCode::Backspace);
        assert_eq!(fx.app.draft(), "a", "nothing to delete at start");
        fx.app.on_key(KeyCode::Right);
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.draft_cursor(), 1, "clamps at end");
        fx.app.on_key(KeyCode::Delete);
        assert_eq!(fx.app.draft(), "a", "nothing to delete at end");
    }

    #[test]
    fn commit_box_up_down_moves_between_visual_rows() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        for c in "subject".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.push_draft_char('\n');
        for c in "body".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.draft(), "subject\nbody");
        assert_eq!(fx.app.draft_cursor(), 12);
        // Up keeps the display column (end of "body" → col 4 in "subject").
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.draft_cursor(), 4);
        // Up past the first row clamps to the document start.
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.draft_cursor(), 0);
        // Down preserves the column into the next row.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 8);
        // Down past the last row jumps to the very end.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 12);
        // Enter still commits a multi-line message.
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn commit_box_up_down_follows_soft_wrapped_rows() {
        let mut fx = harness(&["a.txt"]);
        // Narrow wrap so a single hard line becomes several visual rows.
        fx.app.set_draft_wrap_width(5);
        fx.app.on_key(KeyCode::Char('c'));
        for c in "hello world".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.draft_cursor(), 11);
        // Rows are "hello" / " worl" / "d": end-of-doc col is 1 ("d"), so
        // up lands at col 1 of the previous row (index 6, after the space).
        fx.app.on_key(KeyCode::Up);
        assert_eq!(
            fx.app.draft_cursor(),
            6,
            "same display col on prev visual row"
        );
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 11, "back to end on the last row");
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 11, "clamps at document end");
        // From the document start, down walks visual rows without jumping
        // to the end of the hard line.
        fx.app.on_key(KeyCode::Home);
        assert_eq!(fx.app.draft_cursor(), 0);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.draft_cursor(), 5, "start of second visual row");
    }

    #[test]
    fn empty_commit_shows_notice_and_stays_open() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Committing, "box stays open");
        let notice = fx.app.notice().unwrap_or("");
        assert!(
            notice.contains("type a commit message"),
            "expected empty-draft notice, got: {notice:?}"
        );
        assert!(fx.app.error().is_none());
    }

    #[test]
    fn enter_while_generating_is_ignored_with_notice() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.generating = true;
        for c in "wip".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(
            fx.app.mode(),
            Mode::Committing,
            "must not commit mid-generation"
        );
        assert_eq!(fx.app.draft(), "wip");
        let notice = fx.app.notice().unwrap_or("");
        assert!(notice.contains("generating"), "got: {notice:?}");
    }

    #[test]
    fn failing_job_surfaces_error_display() {
        let mut fx = harness(&["a.txt"]);
        // Commit with nothing staged -> EmptyCommit error via worker.
        fx.app.on_key(KeyCode::Char('c'));
        for c in "nothing".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("nothing to commit"), "got: {err}");
    }

    #[test]
    fn empty_commit_error_tells_user_to_stage_first_and_clears_on_next_action() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        for c in "oops".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("nothing to commit"), "got: {err}");
        assert!(err.contains("space"), "should hint at staging, got: {err}");
        // A new action dismisses the stale error.
        fx.app.on_key(KeyCode::Char('j'));
        assert!(fx.app.error().is_none());
    }

    #[test]
    fn right_arrow_no_longer_leaves_the_file_list_enter_opens_fullscreen() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Vertical-only navigation: Right does nothing now.
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.focus(), Focus::Status);
        assert_eq!(fx.app.mode(), Mode::Normal);
        // Enter opens the file fullscreen; Esc closes back.
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        // Left still jumps back to the file list.
        fx.app.on_key(KeyCode::Char('2'));
        assert_eq!(fx.app.focus(), Focus::Branches);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.focus(), Focus::Status);
    }

    #[test]
    fn up_down_arrows_scroll_diff_line_by_line_without_moving_hunk() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert_eq!(fx.app.diff_scroll(), 0);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.diff_scroll(), 1);
        assert_eq!(
            fx.app.hunk(),
            0,
            "arrow scroll must not move hunk selection"
        );
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.diff_scroll(), 2);
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.diff_scroll(), 1);
        // J/K jump by hunk (and snap scroll to the hunk top).
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1);
        assert!(fx.app.diff_scroll() > 1);
    }

    #[test]
    fn preview_jk_jump_by_hunk_and_cursor_follows() {
        let mut fx = two_hunk_fixture();
        // Focus the right-side preview without going fullscreen.
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        assert_eq!(fx.app.hunk(), 0);
        let first_cursor = fx.app.cursor_row();
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1, "preview J must jump to next hunk");
        assert!(
            fx.app.cursor_row() > first_cursor,
            "cursor must jump with the hunk"
        );
        fx.app.on_key(KeyCode::Char('K'));
        assert_eq!(fx.app.hunk(), 0, "preview K must jump back");
    }

    /// Fabricated screen geometry for mouse tests (matches a 100x32
    /// terminal with the two-line footer).
    fn mouse_layout(app: &mut App) -> crate::ui::ScreenLayout {
        let layout = crate::ui::compute_layout(
            ratatui::layout::Rect::new(0, 0, 100, 32),
            2,
            app.layout_overrides(),
        );
        app.set_last_layout(layout);
        layout
    }

    /// Zero-based cell offset of the footer button labeled `want`.
    fn button_x(app: &App, want: &str) -> u16 {
        let mut off = 0usize;
        for (label, _) in crate::ui::footer_buttons(app) {
            if label == want {
                return off as u16;
            }
            off += label.len() + 3;
        }
        panic!("no footer button {want:?}");
    }

    #[test]
    fn rail_divider_drag_resizes_preview() {
        let mut fx = harness(&["a.txt"]);
        let layout = mouse_layout(&mut fx.app);
        let dx = crate::ui::rail_divider_x(&layout);
        assert_eq!(dx, 30);
        let mid = layout.files.y + 2;
        fx.app.on_mouse_down(dx, mid);
        assert_eq!(fx.app.drag, Some(DragTarget::RailV));
        fx.app.on_mouse_drag(dx + 10, mid);
        fx.app.on_mouse_up();
        assert_eq!(fx.app.drag, None);
        let layout = mouse_layout(&mut fx.app);
        assert_eq!(layout.diff.x, 40, "dragged divider must move the preview");
    }

    #[test]
    fn panel_divider_drag_resizes_files() {
        let mut fx = harness(&["a.txt"]);
        let layout = mouse_layout(&mut fx.app);
        let dys = crate::ui::panel_divider_ys(&layout);
        let before = layout.files.height;
        fx.app.on_mouse_down(2, dys[1]);
        assert_eq!(fx.app.drag, Some(DragTarget::Panel(1)));
        fx.app.on_mouse_drag(2, dys[1] + 2);
        fx.app.on_mouse_up();
        let layout = mouse_layout(&mut fx.app);
        assert_eq!(layout.files.height, before + 2);
    }

    #[test]
    fn footer_buttons_fire_like_keys() {
        let mut fx = harness(&["a.txt"]);
        let layout = mouse_layout(&mut fx.app);
        let y = layout.footer.y;
        fx.app.on_mouse_down(button_x(&fx.app, "Find") + 1, y);
        assert_eq!(fx.app.mode(), Mode::FindFile);
        fx.app.on_key(KeyCode::Esc);
        fx.app.on_mouse_down(button_x(&fx.app, "Commit") + 1, y);
        assert_eq!(fx.app.mode(), Mode::Committing);
    }

    #[test]
    fn file_click_selects_and_second_click_opens_fullscreen() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        let layout = mouse_layout(&mut fx.app);
        // Tree rows: a.txt on the first inner line, b.txt on the next.
        let y0 = layout.files.y + 1;
        fx.app.on_mouse_down(2, y0 + 1);
        assert_eq!(fx.app.selected_file().unwrap().path, "b.txt");
        assert_eq!(fx.app.mode(), Mode::Normal);
        // Clicking the already-selected file opens it fullscreen.
        fx.app.on_mouse_down(2, y0 + 1);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
    }

    #[test]
    fn wheel_over_files_steps_the_tree() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        let layout = mouse_layout(&mut fx.app);
        let (x, y) = (layout.files.x + 2, layout.files.y + 2);
        fx.app.on_wheel(x, y, false);
        assert_eq!(fx.app.selected(), 1);
        fx.app.on_wheel(x, y, true);
        assert_eq!(fx.app.selected(), 0);
    }

    #[test]
    fn preview_click_moves_the_cursor() {
        let mut fx = two_hunk_fixture();
        let layout = mouse_layout(&mut fx.app);
        let y = layout.diff.y + 2;
        fx.app.set_hit(HitMap::Preview, vec![(y, 2)]);
        fx.app.on_mouse_down(layout.diff.x + 4, y);
        assert_eq!(fx.app.focus(), Focus::Diff);
        assert_eq!(fx.app.cursor_row(), 2);
    }

    #[test]
    fn fullscreen_click_moves_cursor_and_header_jumps_hunk() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        let h1 = fx.app.hunk_start_row(1) as usize;
        fx.app
            .set_last_full_rect(ratatui::layout::Rect::new(0, 0, 100, 32), u16::MAX);
        fx.app.set_hit(HitMap::Full, vec![(10, h1 + 1), (20, h1)]);
        fx.app.on_mouse_down(5, 10);
        assert_eq!(fx.app.cursor_row(), h1 + 1);
        assert_eq!(fx.app.hunk(), 1);
        // Header click jumps to that hunk (the viewport only follows
        // enough to keep the cursor visible, no top snap).
        fx.app.on_mouse_down(5, 20);
        assert_eq!(fx.app.hunk(), 1);
        assert_eq!(fx.app.cursor_row(), h1);
        assert_eq!(fx.app.diff_scroll(), h1 as u16);
    }

    #[test]
    fn fullscreen_button_bar_fires_stage_and_close() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        let rect = ratatui::layout::Rect::new(0, 0, 100, 32);
        fx.app.set_last_full_rect(rect, 30);
        // First button is `[Stage hunk]`: fires the staging job path
        // (fails silently here without assertion — mode must not change).
        fx.app.on_mouse_down(rect.x + 2, 30);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        // `[Close]` leaves fullscreen.
        let x = button_x(&fx.app, "Close");
        fx.app.on_mouse_down(rect.x + 1 + x, 30);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    /// Repo with `a.txt` dirtied and `b.txt` left clean.
    fn clean_fixture() -> (tempfile::TempDir, App) {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "a\n", "init");
        commit_file(&repo, "b.txt", "b\n", "init");
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(repo.workdir().unwrap().join("a.txt"))
                .unwrap();
            f.write_all(b"dirty\n").unwrap();
        }
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let app = App::new(JobQueue::spawn(path).unwrap());
        (dir, app)
    }

    /// Settle until the browsable tree holds `n` entries.
    fn wait_for_files(app: &mut App, n: usize) -> Vec<StatusEntry> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if app.file_list().len() == n {
                return app.file_list().to_vec();
            }
            assert!(
                Instant::now() < deadline,
                "timed out; last: {:?}",
                app.file_list()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn clean_tracked_files_join_the_tree_after_changed_ones() {
        let (_dir, mut app) = clean_fixture();
        let list = wait_for_files(&mut app, 2);
        assert_eq!(list[0].path, "a.txt");
        assert_eq!(list[0].state, FileState::Unstaged);
        assert_eq!(list[1].path, "b.txt");
        assert_eq!(list[1].state, FileState::Clean);
    }

    #[test]
    fn binary_file_gets_notice_instead_of_whole_file_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("img.png"), b"\x89PNG\r\n\x1a\n\0\0\x1b[2J").unwrap();
        let mut app = App::new(JobQueue::spawn(dir.path()).unwrap());
        wait_for(&mut app, |st| st.files.iter().any(|e| e.path == "img.png"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !app.diff().is_some_and(|d| d.path == "img.png") {
            assert!(Instant::now() < deadline, "timed out waiting for diff");
            app.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        // Let any (wrong) whole-file fallback job run and land.
        for _ in 0..30 {
            app.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        let d = app.diff().expect("diff stays loaded");
        assert!(d.binary, "binary flag lost: {d:?}");
        assert!(d.hunks.is_empty(), "binary bytes leaked into hunks");
        assert!(
            !app.diff_whole_file(),
            "binary must not fall back to whole-file view"
        );
    }

    #[test]
    fn enter_on_clean_file_loads_whole_file() {
        use git_tui_core::diff::LineKind;
        let (_dir, mut app) = clean_fixture();
        wait_for_files(&mut app, 2);
        fx_select(&mut app);
        let d = wait_for_diff(&mut app, "b.txt");
        assert!(app.diff_whole_file());
        let texts: Vec<&str> = d
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(texts, vec!["b"]);
        assert!(d
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .all(|l| l.kind == LineKind::Context));
    }

    /// Move selection to the clean `b.txt` (index 1 in the clean fixture).
    fn fx_select(app: &mut App) {
        app.on_key(KeyCode::Char('j'));
        assert_eq!(app.selected_file().unwrap().path, "b.txt");
    }

    #[test]
    fn space_on_clean_file_reports_error() {
        let (_dir, mut app) = clean_fixture();
        wait_for_files(&mut app, 2);
        fx_select(&mut app);
        app.on_key(KeyCode::Char(' '));
        let err = app.error().unwrap_or("").to_string();
        assert!(err.contains("unchanged"), "got: {err}");
    }

    #[test]
    fn space_on_whole_file_hunk_errors_instead_of_staging() {
        let (_dir, mut app) = clean_fixture();
        wait_for_files(&mut app, 2);
        fx_select(&mut app);
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        wait_for_diff(&mut app, "b.txt");
        app.on_key(KeyCode::Char(' '));
        let err = app.error().unwrap_or("").to_string();
        assert!(err.contains("nothing to stage"), "got: {err}");
    }

    #[test]
    fn enter_on_changed_file_shows_diff_automatically() {
        let mut fx = harness(&["a.txt"]);
        // Changed file: the loaded view is the diff, not the whole file.
        let d = wait_for_diff(&mut fx.app, "a.txt");
        assert!(!fx.app.diff_whole_file());
        assert!(!d.hunks.is_empty());
        // Fullscreen shows that same diff.
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert!(!fx.app.diff_whole_file());
        assert_eq!(fx.app.diff().unwrap().path, "a.txt");
    }

    #[test]
    fn enter_on_clean_file_shows_whole_file_automatically() {
        use git_tui_core::diff::LineKind;
        let (_dir, mut app) = clean_fixture();
        wait_for_files(&mut app, 2);
        fx_select(&mut app);
        // No changes: whole workdir file loads on its own.
        let d = wait_for_diff(&mut app, "b.txt");
        assert!(app.diff_whole_file());
        assert!(d
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .all(|l| { l.kind == LineKind::Context }));
        // Fullscreen shows that same whole-file view.
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        assert!(app.diff_whole_file());
        assert_eq!(app.diff().unwrap().path, "b.txt");
    }

    // Needs a mode-only change, which only exists where the filesystem
    // has an executable bit.
    #[cfg(unix)]
    #[test]
    fn empty_diff_falls_back_to_whole_file_automatically() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "hello\nworld\n", "init");
        // Mode-only change: status lists the file, but the content diff
        // has zero hunks — the viewer must show the whole file, not
        // "(no changes)".
        std::fs::set_permissions(
            repo.workdir().unwrap().join("a.txt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(path).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            if fx.app.diff_whole_file() {
                if let Some(d) = fx.app.diff() {
                    if d.path == "a.txt" && !d.hunks.is_empty() {
                        break;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "whole-file fallback never loaded; whole={} diff={:?}",
                fx.app.diff_whole_file(),
                fx.app.diff()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let texts: Vec<&str> = fx
            .app
            .diff()
            .unwrap()
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(texts, vec!["hello", "world"]);
    }

    #[test]
    fn q_in_modal_types_literal_and_quits_after_esc() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_key(KeyCode::Char('q'));
        assert!(!fx.app.should_quit());
        assert_eq!(fx.app.draft(), "q");
        fx.app.on_key(KeyCode::Char('Q'));
        assert!(!fx.app.should_quit(), "Q in modal must type, not quit");
        assert_eq!(fx.app.draft(), "qQ");
        fx.app.on_key(KeyCode::Esc);
        // Lowercase `q` no longer quits the app (workspace closes the
        // project instead); uppercase `Q` quits everything.
        fx.app.on_key(KeyCode::Char('q'));
        assert!(!fx.app.should_quit());
        fx.app.on_key(KeyCode::Char('Q'));
        assert!(fx.app.should_quit());
    }

    /// Repo with an `origin` remote (bare, on disk) that nothing has been
    /// pushed to yet: pressing `P` must offer to set the upstream.
    fn sync_harness() -> (tempfile::TempDir, tempfile::TempDir, App) {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let bare = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&bare).unwrap();
        repo.remote("origin", bare.to_str().unwrap()).unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let app = App::new(JobQueue::spawn(path).unwrap());
        (dir, origin_dir, app)
    }

    /// Settle until upstream tracking state arrives (preloaded at startup).
    fn wait_for_sync(app: &mut App) -> git_tui_core::sync::SyncStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(st) = app.sync() {
                return st.clone();
            }
            assert!(Instant::now() < deadline, "timed out waiting for sync");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn p_pull_failure_surfaces_error_and_clears_syncing() {
        // Harness repos have no remote (and dirty files, so `git pull`
        // fails one way or another depending on the user's pull config).
        // What matters: the failure lands in the error line and the
        // "pulling…" indicator clears instead of sticking forever.
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('p'));
        assert_eq!(fx.app.syncing(), Some("pulling…"));
        let err = wait_for_error(&mut fx.app);
        assert!(!err.is_empty(), "expected a pull failure message");
        assert!(fx.app.syncing().is_none());
    }

    #[test]
    fn push_without_remotes_opens_publish_modal_and_enter_publishes() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let bare = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&bare).unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut app = App::new(JobQueue::spawn(path).unwrap());
        let st = wait_for_sync(&mut app);
        assert!(st.remotes.is_empty());
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::SetRemote);
        for c in bare.to_str().unwrap().chars() {
            app.on_key(KeyCode::Char(c));
        }
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::Normal);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if app.sync().is_some_and(|s| s.upstream.is_some()) {
                break;
            }
            assert!(Instant::now() < deadline, "publish never recorded upstream");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.sync().unwrap().upstream.as_deref(), Some("origin/main"));
        let remote = git2::Repository::open_bare(&bare).unwrap();
        assert!(remote.find_reference("refs/heads/main").is_ok());
        drop(dir);
    }

    #[test]
    fn push_without_upstream_opens_set_upstream_modal() {
        let (_dir, _origin, mut app) = sync_harness();
        wait_for_sync(&mut app);
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::SetUpstream);
        assert_eq!(app.draft(), "origin");
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::Normal);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if app.sync().is_some_and(|s| s.upstream.is_some()) {
                break;
            }
            assert!(Instant::now() < deadline, "push -u never recorded upstream");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.sync().unwrap().upstream.as_deref(), Some("origin/main"));
    }

    #[test]
    fn push_with_upstream_asks_for_confirmation_first() {
        let (_dir, _origin, mut app) = sync_harness();
        wait_for_sync(&mut app);
        // Establish the upstream first.
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::SetUpstream);
        app.on_key(KeyCode::Enter);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if app.sync().is_some_and(|s| s.upstream.is_some()) {
                break;
            }
            assert!(Instant::now() < deadline, "push -u never recorded upstream");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.syncing().is_none());
        // A fresh `P` now opens the confirm box instead of pushing.
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::ConfirmPush);
        assert_eq!(
            app.pending_push(),
            Some(("origin".to_string(), "main".to_string()))
        );
        assert!(app.syncing().is_none(), "nothing pushed yet");
        // Enter pushes.
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.syncing(), Some("pushing…"));
        assert!(app.pending_push().is_none());
    }

    #[test]
    fn esc_in_push_confirm_cancels_without_pushing() {
        let (_dir, _origin, mut app) = sync_harness();
        wait_for_sync(&mut app);
        app.on_key(KeyCode::Char('P'));
        app.on_key(KeyCode::Enter);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if app.sync().is_some_and(|s| s.upstream.is_some()) {
                break;
            }
            assert!(Instant::now() < deadline, "push -u never recorded upstream");
            std::thread::sleep(Duration::from_millis(10));
        }
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::ConfirmPush);
        app.on_key(KeyCode::Esc);
        assert_eq!(app.mode(), Mode::Normal);
        assert!(app.syncing().is_none(), "esc must not push");
        assert!(app.pending_push().is_none());
        // Other keys are swallowed by the confirm box (no typing leaks).
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::ConfirmPush);
        app.on_key(KeyCode::Char('q'));
        assert_eq!(app.mode(), Mode::ConfirmPush);
        assert!(!app.should_quit());
        app.on_key(KeyCode::Esc);
    }

    #[test]
    fn push_confirm_buttons_are_clickable() {
        let mut fx = harness(&["a.txt"]);
        fx.app.pending_push = Some(("origin".to_string(), "main".to_string()));
        fx.app.mode = Mode::ConfirmPush;
        fx.app.set_confirm_btn(Some((10, 20)));
        // Inside `[Push]`: pushes.
        fx.app.on_mouse_down(11, 20);
        assert_eq!(fx.app.syncing(), Some("pushing…"));
        assert_eq!(fx.app.mode(), Mode::Normal);
        // Reset: inside `[Cancel]` (starts 8 cells past `[Push]`).
        fx.app.syncing = None;
        fx.app.pending_push = Some(("origin".to_string(), "main".to_string()));
        fx.app.mode = Mode::ConfirmPush;
        fx.app.on_mouse_down(10 + 8 + 1, 20);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert!(fx.app.syncing().is_none(), "cancel must not push");
        assert!(fx.app.pending_push().is_none());
        // Clicks elsewhere in the box do nothing.
        fx.app.pending_push = Some(("origin".to_string(), "main".to_string()));
        fx.app.mode = Mode::ConfirmPush;
        fx.app.on_mouse_down(60, 20);
        assert_eq!(fx.app.mode(), Mode::ConfirmPush);
    }

    #[test]
    fn esc_in_sync_modal_returns_without_syncing() {
        let (_dir, _origin, mut app) = sync_harness();
        wait_for_sync(&mut app);
        app.on_key(KeyCode::Char('P'));
        assert_eq!(app.mode(), Mode::SetUpstream);
        app.on_key(KeyCode::Esc);
        assert_eq!(app.mode(), Mode::Normal);
        assert!(app.syncing().is_none());
        assert!(app.sync().unwrap().upstream.is_none());
    }

    #[test]
    fn sync_modal_from_fullscreen_returns_to_fullscreen() {
        let mut fx = harness(&["a.txt"]);
        wait_for_sync(&mut fx.app);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        // Harness repos have no remotes: `P` opens the publish modal.
        fx.app.on_key(KeyCode::Char('P'));
        assert_eq!(fx.app.mode(), Mode::SetRemote);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
    }

    /// Two-hunk fixture: 40 lines, changes at line 5 and line 35.
    fn two_hunk_fixture() -> Fixture {
        let (dir, repo) = init_repo();
        let base = (1..=40).map(|i| format!("line {i}\n")).collect::<String>();
        commit_file(&repo, "a.txt", &base, "init");
        let dirty = base.replacen("line 5\n", "line 5 CHANGED\n", 1).replacen(
            "line 35\n",
            "line 35 CHANGED\n",
            1,
        );
        std::fs::write(repo.workdir().unwrap().join("a.txt"), &dirty).unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(path).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        wait_for_diff(&mut fx.app, "a.txt");
        fx
    }

    fn wait_for_diff(app: &mut App, path: &str) -> git_tui_core::diff::FileDiff {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(d) = app.diff() {
                if d.path == path && !d.hunks.is_empty() {
                    return d.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for diff of {path}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn force_reload_recomputes_target_from_fresh_status() {
        // Stash push cleans the tree (whole-file view); pop dirties it
        // again (hunks). The [5] preview must follow the fresh status,
        // not re-request the stale `diff_for`.
        use git_tui_core::status::{RepoStatus, StatusEntry};
        let mut fx = harness(&["a.txt"]);
        wait_for_diff(&mut fx.app, "a.txt");
        // Fresh status: tree clean -> whole-file view for the same path.
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![],
            tracked_files: vec!["a.txt".into()],
        });
        fx.app.rebuild_file_list();
        fx.app.force_reload_diff_for_fresh_status();
        assert_eq!(
            fx.app.diff_for,
            Some(("a.txt".to_string(), false)),
            "target must be recomputed, got {:?}",
            fx.app.diff_for
        );
        assert!(
            fx.app.diff_whole_file(),
            "clean tree must show the whole file, not hunks"
        );
        // Fresh status: dirty again -> real diff with hunks.
        fx.app.status = Some(RepoStatus {
            branch: "main".into(),
            head_summary: "x".into(),
            files: vec![StatusEntry {
                path: "a.txt".into(),
                state: FileState::Unstaged,
            }],
            tracked_files: vec!["a.txt".into()],
        });
        fx.app.rebuild_file_list();
        fx.app.force_reload_diff_for_fresh_status();
        assert!(
            !fx.app.diff_whole_file(),
            "dirty tree must show the diff, not the whole file"
        );
    }

    #[test]
    fn enter_esc_then_walk_files_in_both_directions() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        for expected in ["b.txt", "c.txt"] {
            fx.app.on_key(KeyCode::Char('j'));
            assert_eq!(
                fx.app.selected_file().unwrap().path,
                expected,
                "down-walk broke after Esc"
            );
        }
        for expected in ["b.txt", "a.txt"] {
            fx.app.on_key(KeyCode::Char('k'));
            assert_eq!(
                fx.app.selected_file().unwrap().path,
                expected,
                "up-walk broke after Esc"
            );
        }
        // The preview must follow the cursor again too.
        let d = wait_for_diff(&mut fx.app, "a.txt");
        assert_eq!(d.path, "a.txt");
    }

    #[test]
    fn slash_opens_finder_and_esc_cancels() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('/'));
        assert_eq!(fx.app.mode(), Mode::FindFile);
        assert_eq!(fx.app.draft(), "");
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn finder_typing_filters_and_enter_jumps_to_match() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        fx.app.on_key(KeyCode::Char('/'));
        for c in "b".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.finder_matches(), vec![1]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.focus(), Focus::Status);
        assert_eq!(fx.app.selected(), 1);
        assert_eq!(fx.app.selected_file().unwrap().path, "b.txt");
    }

    #[test]
    fn finder_arrows_move_and_clamp() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        fx.app.on_key(KeyCode::Char('/'));
        assert_eq!(fx.app.finder_matches().len(), 3);
        fx.app.on_key(KeyCode::Down);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.finder_cursor(), 2);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.finder_cursor(), 2, "clamps at end");
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.finder_cursor(), 1);
    }

    #[test]
    fn finder_enter_with_no_matches_stays_open() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('/'));
        for c in "zzz".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert!(fx.app.finder_matches().is_empty());
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FindFile);
    }

    #[test]
    fn slash_in_fullscreen_opens_finder_and_esc_returns_to_fullscreen() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('/'));
        assert_eq!(fx.app.mode(), Mode::FindFile);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
    }

    #[test]
    fn finder_enter_in_fullscreen_jumps_to_match_and_stays_fullscreen() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('/'));
        for c in "b".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert_eq!(fx.app.finder_matches(), vec![1]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert_eq!(fx.app.selected_file().unwrap().path, "b.txt");
        let d = wait_for_diff(&mut fx.app, "b.txt");
        assert_eq!(d.path, "b.txt");
    }

    #[test]
    fn tab_cycles_focus_through_all_panels() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.focus(), Focus::Status);
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Branches);
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Log);
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Stash);
        // Tab skips the right-side Diff preview: Stash wraps to Status.
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Diff is reached via `5` / Shift+Right, not via Tab.
        fx.app.on_key(KeyCode::Char('4'));
        assert_eq!(fx.app.focus(), Focus::Stash);
        fx.app.on_key(KeyCode::Char('3'));
        assert_eq!(fx.app.focus(), Focus::Log);
        fx.app.on_key(KeyCode::Char('2'));
        assert_eq!(fx.app.focus(), Focus::Branches);
        fx.app.on_key(KeyCode::Char('1'));
        assert_eq!(fx.app.focus(), Focus::Status);
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        // Tab from Diff drops back to the file list.
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Status);
    }

    #[test]
    fn shift_right_from_files_focuses_diff_preview() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Plain Right expands folders and must stay in the file list.
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Shift+Right jumps to the right-side diff tab.
        fx.app.on_key_with_modifiers(KeyCode::Right, true);
        assert_eq!(fx.app.focus(), Focus::Diff);
        // Shift+Left (Left already owns focus_status) jumps back.
        fx.app.on_key_with_modifiers(KeyCode::Left, true);
        assert_eq!(fx.app.focus(), Focus::Status);
    }

    #[test]
    fn diff_focus_scrolls_preview_without_moving_selection() {
        let mut fx = two_hunk_fixture();
        assert_eq!(fx.app.focus(), Focus::Status);
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        let sel = fx.app.selected();
        assert_eq!(fx.app.diff_scroll(), 0);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.diff_scroll(), 1);
        assert_eq!(fx.app.selected(), sel, "file cursor must not move");
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.diff_scroll(), 2);
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.diff_scroll(), 1);
        // Left jumps back to the file list (focus_status owns Left).
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.focus(), Focus::Status);
        // Enter from the preview opens fullscreen too.
        fx.app.on_key(KeyCode::Char('5'));
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
    }

    #[test]
    fn diff_preview_scroll_clamps_at_content_end() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        let max = crate::ui::rows_unified_len(fx.app.diff_rows())
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16;
        assert!(max > 2, "fixture must have scrollable content");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('j'));
        }
        assert_eq!(fx.app.diff_scroll(), max);
        for _ in 0..10 {
            fx.app.on_key(KeyCode::PageDown);
        }
        assert_eq!(fx.app.diff_scroll(), max, "PageDown must clamp too");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('k'));
        }
        assert_eq!(fx.app.diff_scroll(), 0);
        for _ in 0..10 {
            fx.app.on_key(KeyCode::PageUp);
        }
        assert_eq!(fx.app.diff_scroll(), 0, "PageUp must not underflow");
    }

    #[test]
    fn fullscreen_scroll_clamps_at_content_end() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        let max = fx
            .app
            .diff_rows()
            .len()
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16;
        assert!(max > 2, "fixture must have scrollable content");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Down);
        }
        assert_eq!(fx.app.diff_scroll(), max);
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Up);
        }
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn diff_rows_are_paired_once_on_arrival() {
        use git_tui_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        let mut fx = harness(&["a.txt"]);
        assert!(fx.app.diff_rows().is_empty(), "no diff, no rows");
        fx.app.set_diff_for_test(
            FileDiff {
                path: "a.txt".into(),
                binary: false,
                hunks: vec![Hunk {
                    header: "@@ -1,1 +1,1 @@".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        DiffLine {
                            kind: LineKind::Del,
                            text: "old".into(),
                        },
                        DiffLine {
                            kind: LineKind::Add,
                            text: "new".into(),
                        },
                    ],
                }],
            },
            false,
        );
        // Header + one paired del/add row, matching a fresh pairing.
        assert_eq!(fx.app.diff_rows().len(), 2);
        let fresh = crate::ui::diff_rows(fx.app.diff().unwrap());
        assert_eq!(fx.app.diff_rows(), fresh.as_slice());
    }

    #[test]
    fn moving_file_selection_loads_its_diff() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        // Select b.txt; its diff should load automatically.
        fx.app.on_key(KeyCode::Char('j'));
        let d = wait_for_diff(&mut fx.app, "b.txt");
        assert_eq!(d.path, "b.txt");
    }

    #[test]
    fn fully_staged_file_loads_staged_diff() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            if fx.app.diff_viewing_staged() == Some(true) {
                if let Some(d) = fx.app.diff() {
                    if !d.hunks.is_empty() {
                        return;
                    }
                }
            }
            assert!(Instant::now() < deadline, "staged diff never loaded");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn hunk_nav_clamps_and_snaps_scroll() {
        let mut fx = two_hunk_fixture();
        assert!(fx.app.diff().unwrap().hunks.len() >= 2);
        assert_eq!(fx.app.hunk(), 0);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1);
        // Snaps the selected hunk to the top of the view.
        assert!(fx.app.diff_scroll() > 0);
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1, "clamps at last hunk");
        fx.app.on_key(KeyCode::Char('K'));
        assert_eq!(fx.app.hunk(), 0);
    }

    #[test]
    fn jk_moves_line_cursor_and_syncs_hunk() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert_eq!(fx.app.cursor_row(), 0);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.diff_scroll(), 1, "view follows the cursor");
        assert_eq!(fx.app.hunk(), 0, "still inside the first hunk");
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.cursor_row(), 0);
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn shift_jk_jumps_between_hunks() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1);
        let start = crate::ui::rows_hunk_start(fx.app.diff_rows(), 1) as usize;
        assert_eq!(fx.app.cursor_row(), start);
        assert_eq!(fx.app.diff_scroll(), start as u16);
        // Clamps at the last hunk.
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1);
        fx.app.on_key(KeyCode::Char('K'));
        assert_eq!(fx.app.hunk(), 0);
        assert_eq!(fx.app.cursor_row(), 0);
    }

    #[test]
    fn shift_down_up_mirror_jk_in_fullscreen() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key_with_modifiers(KeyCode::Down, true);
        assert_eq!(fx.app.hunk(), 1, "Shift+Down must jump like J");
        fx.app.on_key_with_modifiers(KeyCode::Down, true);
        assert_eq!(fx.app.hunk(), 1, "clamps at last hunk like J");
        fx.app.on_key_with_modifiers(KeyCode::Up, true);
        assert_eq!(fx.app.hunk(), 0, "Shift+Up must jump like K");
        // Plain arrows still move line-by-line.
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.hunk(), 0, "plain Down must not jump hunks");
    }

    #[test]
    fn shift_down_up_mirror_jk_in_preview() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        fx.app.on_key_with_modifiers(KeyCode::Down, true);
        assert_eq!(fx.app.hunk(), 1, "preview Shift+Down must jump like J");
        fx.app.on_key_with_modifiers(KeyCode::Up, true);
        assert_eq!(fx.app.hunk(), 0, "preview Shift+Up must jump like K");
    }

    #[test]
    fn shift_down_up_noop_where_j_noops() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        // File list: J has no binding, so Shift+Down must also do nothing.
        let before = fx.app.selected();
        fx.app.on_key_with_modifiers(KeyCode::Down, true);
        assert_eq!(fx.app.selected(), before);
        fx.app.on_key_with_modifiers(KeyCode::Up, true);
        assert_eq!(fx.app.selected(), before);
    }

    #[test]
    fn shift_arrows_keep_arrow_behavior_in_finder() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        fx.app.on_key(KeyCode::Char('/'));
        assert_eq!(fx.app.mode(), Mode::FindFile);
        fx.app.on_key_with_modifiers(KeyCode::Down, true);
        assert_eq!(fx.app.finder_cursor(), 1);
        assert_eq!(fx.app.draft(), "", "must not type J into the filter");
        fx.app.on_key_with_modifiers(KeyCode::Up, true);
        assert_eq!(fx.app.finder_cursor(), 0);
    }

    #[test]
    fn line_cursor_clamps_at_ends() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        let last = fx.app.diff_rows().len() - 1;
        assert!(last > 2, "fixture must have several rows");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('j'));
        }
        assert_eq!(fx.app.cursor_row(), last);
        assert_eq!(fx.app.diff_scroll(), last as u16);
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('k'));
        }
        assert_eq!(fx.app.cursor_row(), 0);
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn cursor_moves_within_view_before_scrolling_down() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.set_view_h_for_test(4, 4);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 3);
        assert_eq!(fx.app.diff_scroll(), 0, "view must not scroll yet");
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 4);
        assert_eq!(fx.app.diff_scroll(), 1, "scrolls only at the edge");
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 5);
        assert_eq!(fx.app.diff_scroll(), 2);
    }

    #[test]
    fn scrolling_up_follows_the_top_edge() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.set_view_h_for_test(4, 4);
        for _ in 0..5 {
            fx.app.on_key(KeyCode::Char('j'));
        }
        assert_eq!((fx.app.cursor_row(), fx.app.diff_scroll()), (5, 2));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.diff_scroll(), 2, "still visible, no scroll");
        fx.app.on_key(KeyCode::Char('k'));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.diff_scroll(), 2);
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!((fx.app.cursor_row(), fx.app.diff_scroll()), (1, 1));
    }

    #[test]
    fn preview_follows_unified_lines_at_edges() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        fx.app.set_view_h_for_test(4, 4);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 3);
        assert_eq!(fx.app.diff_scroll(), 0, "three context lines fit");
        // Row 4 is a del/add pair (two unified lines): the whole row
        // must become visible.
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 4);
        assert_eq!(fx.app.diff_scroll(), 2);
        fx.app.on_key(KeyCode::Char('k'));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.diff_scroll(), 2, "still visible, no scroll");
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!((fx.app.cursor_row(), fx.app.diff_scroll()), (1, 1));
    }

    #[test]
    fn page_keys_move_cursor_with_follow() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        // A page is what is on screen (minus one line of overlap), not a
        // fixed ten rows.
        fx.app.set_view_h_for_test(5, 5);
        fx.app.on_key(KeyCode::PageDown);
        assert_eq!(fx.app.cursor_row(), 4);
        fx.app.on_key(KeyCode::PageDown);
        fx.app.on_key(KeyCode::PageDown);
        assert_eq!(fx.app.cursor_row(), 12);
        assert_eq!(fx.app.hunk(), 1, "crossed into the second hunk");
        assert_eq!(fx.app.diff_scroll(), 8);
        for _ in 0..3 {
            fx.app.on_key(KeyCode::PageUp);
        }
        assert_eq!((fx.app.cursor_row(), fx.app.diff_scroll()), (0, 0));
        // Ctrl+D / Ctrl+U move half a page.
        fx.app.on_chord(Chord::Ctrl('d'));
        assert_eq!(fx.app.cursor_row(), 2);
        fx.app.on_chord(Chord::Ctrl('u'));
        assert_eq!(fx.app.cursor_row(), 0);
    }

    #[test]
    fn arrows_move_line_cursor_in_fullscreen() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.diff_scroll(), 1);
        assert_eq!(fx.app.hunk(), 0, "arrow move must not jump hunks");
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.cursor_row(), 0);
    }

    #[test]
    fn preview_jk_moves_line_cursor() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        assert_eq!(fx.app.cursor_row(), 0);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.diff_scroll(), 1);
        assert_eq!(fx.app.hunk(), 0);
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.cursor_row(), 0);
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn hl_moves_column_and_clamps_to_line_end() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        // Row 1 is a short context line; walk right past its end.
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.cursor_col(), 0);
        fx.app.on_key(KeyCode::Char('l'));
        assert_eq!(fx.app.cursor_col(), 1);
        let len = crate::ui::cursor_line_text(fx.app.diff().unwrap(), &fx.app.diff_rows()[1])
            .chars()
            .count();
        assert!(len > 2, "fixture line must have some width");
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('l'));
        }
        assert_eq!(fx.app.cursor_col(), len - 1, "clamps on last char like $");
        fx.app.on_key(KeyCode::Char('h'));
        assert_eq!(fx.app.cursor_col(), len - 2);
        for _ in 0..500 {
            fx.app.on_key(KeyCode::Char('h'));
        }
        assert_eq!(fx.app.cursor_col(), 0);
    }

    #[test]
    fn column_is_sticky_across_rows_with_home_end() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('l'));
        fx.app.on_key(KeyCode::Char('l'));
        fx.app.on_key(KeyCode::Char('l'));
        assert_eq!(fx.app.cursor_col(), 3);
        // Moving rows keeps the desired column (nvim sticky column).
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 2);
        assert_eq!(fx.app.cursor_col(), 3);
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.cursor_row(), 1);
        assert_eq!(fx.app.cursor_col(), 3);
        fx.app.on_key(KeyCode::End);
        let len = crate::ui::cursor_line_text(fx.app.diff().unwrap(), &fx.app.diff_rows()[1])
            .chars()
            .count();
        assert_eq!(fx.app.cursor_col(), len - 1);
        fx.app.on_key(KeyCode::Home);
        assert_eq!(fx.app.cursor_col(), 0);
        fx.app.on_key(KeyCode::Char('0'));
        assert_eq!(fx.app.cursor_col(), 0);
    }

    #[test]
    fn arrows_move_column_in_fullscreen() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.cursor_col(), 1);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.cursor_col(), 0);
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.cursor_col(), 0, "clamps at line start");
    }

    #[test]
    fn preview_hl_moves_column_and_left_still_leaves() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        assert_eq!(fx.app.focus(), Focus::Diff);
        fx.app.on_key(KeyCode::Char('l'));
        assert_eq!(fx.app.cursor_col(), 1);
        fx.app.on_key(KeyCode::Char('h'));
        assert_eq!(fx.app.cursor_col(), 0);
        // Plain Left keeps its back-to-files job in the preview.
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.focus(), Focus::Status);
    }

    #[test]
    fn visual_linewise_yank_joins_whole_rows() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        // Anchor on the header, extend one row down, yank whole lines.
        fx.app.on_key(KeyCode::Char('V'));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(
            fx.app.yank_text().as_deref(),
            Some("@@ -1,2 +1,2 @@\nsame\n")
        );
    }

    #[test]
    fn visual_charwise_yank_slices_endpoints() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('l'));
        fx.app.on_key(KeyCode::Char('v'));
        fx.app.on_key(KeyCode::Char('j'));
        // Anchor (1,1) on `same`, cursor (2,1) on the `new` side.
        assert_eq!(fx.app.yank_text().as_deref(), Some("ame\nne"));
    }

    #[test]
    fn visual_charwise_normalizes_reversed_selection() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('l'));
        // Anchor below, move up: same text as extending downward.
        fx.app.on_key(KeyCode::Char('v'));
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.yank_text().as_deref(), Some("ame\nne"));
    }

    #[test]
    fn yank_without_visual_takes_cursor_line() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.yank_text().as_deref(), Some("same\n"));
        fx.app.on_key(KeyCode::Char('y'));
        assert!(fx.app.visual_selection().is_none());
        let notice = fx.app.notice().unwrap_or("").to_string();
        assert!(notice.contains("yanked"), "got: {notice:?}");
    }

    #[test]
    fn esc_leaves_visual_before_closing_fullscreen() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('v'));
        assert!(fx.app.visual_selection().is_some());
        fx.app.on_key(KeyCode::Esc);
        assert!(fx.app.visual_selection().is_none());
        assert_eq!(fx.app.mode(), Mode::FullDiff, "still fullscreen");
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn visual_toggle_and_mode_switch() {
        let mut fx = visual_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('v'));
        assert!(fx.app.visual_selection().is_some());
        // Same key again leaves visual.
        fx.app.on_key(KeyCode::Char('v'));
        assert!(fx.app.visual_selection().is_none());
        // V enters linewise; v switches it back to charwise.
        fx.app.on_key(KeyCode::Char('V'));
        assert_eq!(fx.app.visual_selection().map(|s| s.2), Some(true));
        fx.app.on_key(KeyCode::Char('v'));
        assert_eq!(fx.app.visual_selection().map(|s| s.2), Some(false));
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(super::base64_encode(b""), "");
        assert_eq!(super::base64_encode(b"f"), "Zg==");
        assert_eq!(super::base64_encode(b"fo"), "Zm8=");
        assert_eq!(super::base64_encode(b"foo"), "Zm9v");
        assert_eq!(super::base64_encode(b"hello"), "aGVsbG8=");
    }

    /// One-hunk diff with known text for visual tests: header, a `same`
    /// context line, and an old/new pair.
    fn visual_fixture() -> Fixture {
        use git_tui_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        let mut fx = harness(&["a.txt"]);
        fx.app.set_diff_for_test(
            FileDiff {
                path: "a.txt".into(),
                binary: false,
                hunks: vec![Hunk {
                    header: "@@ -1,2 +1,2 @@".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        DiffLine {
                            kind: LineKind::Context,
                            text: "same".into(),
                        },
                        DiffLine {
                            kind: LineKind::Del,
                            text: "old".into(),
                        },
                        DiffLine {
                            kind: LineKind::Add,
                            text: "new".into(),
                        },
                    ],
                }],
            },
            false,
        );
        fx
    }

    #[test]
    fn cursor_resets_when_diff_reloads() {
        use git_tui_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        let mut fx = harness(&["a.txt"]);
        let diff = FileDiff {
            path: "a.txt".into(),
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,2 @@".into(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    DiffLine {
                        kind: LineKind::Context,
                        text: "same".into(),
                    },
                    DiffLine {
                        kind: LineKind::Del,
                        text: "old".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        text: "new".into(),
                    },
                ],
            }],
        };
        fx.app.set_diff_for_test(diff.clone(), false);
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.cursor_row(), 1);
        fx.app.set_diff_for_test(diff, false);
        assert_eq!(fx.app.cursor_row(), 0, "fresh diff resets the cursor");
        assert_eq!(fx.app.hunk(), 0);
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn space_in_diff_stages_selected_hunk() {
        let mut fx = two_hunk_fixture();
        let before = fx.app.diff().unwrap().hunks.len();
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char(' '));
        // Partial staging: status shows Both, diff reloads with one less hunk.
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::BothStagedAndUnstaged)
        });
        let _ = st;
        let d = wait_for_diff(&mut fx.app, "a.txt");
        assert_eq!(d.hunks.len(), before - 1);
    }

    #[test]
    fn space_in_staged_diff_view_errors() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            if fx.app.diff_viewing_staged() == Some(true) {
                break;
            }
            assert!(Instant::now() < deadline, "staged diff never loaded");
            std::thread::sleep(Duration::from_millis(10));
        }
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char(' '));
        assert!(fx.app.error().is_some(), "expected staged-hunk error");
    }

    fn wait_for_branches(app: &mut App, min_count: usize) -> Vec<BranchInfo> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(b) = app.branches() {
                if b.len() >= min_count {
                    return b.to_vec();
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for branches");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn focus_branches(fx: &mut Fixture) {
        fx.app.on_key(KeyCode::Char('2'));
        assert_eq!(fx.app.focus(), Focus::Branches);
    }

    /// Move branch selection to the named branch (sort-order independent).
    fn select_branch(fx: &mut Fixture, name: &str) {
        let branches = wait_for_branches(&mut fx.app, 1);
        let pos = branches
            .iter()
            .position(|b| b.name == name)
            .unwrap_or_else(|| panic!("branch {name} missing in {branches:?}"));
        while fx.app.branch_selected() < pos {
            fx.app.on_key(KeyCode::Char('j'));
        }
        while fx.app.branch_selected() > pos {
            fx.app.on_key(KeyCode::Char('k'));
        }
    }

    #[test]
    fn panels_load_eagerly_at_startup_without_focusing() {
        let mut fx = harness(&["a.txt"]);
        // No focus keys pressed: branches, log, and stash must arrive on
        // their own so every pane shows data on launch.
        let branches = wait_for_branches(&mut fx.app, 1);
        assert!(branches.iter().any(|b| b.name == "main" && b.is_head));
        let entries = wait_for_log(&mut fx.app, 1);
        assert_eq!(entries[0].summary, "init");
        let stash = wait_for_stash(&mut fx.app, 0);
        assert!(stash.is_empty());
    }

    #[test]
    fn branches_load_when_panel_focused() {
        let mut fx = harness(&["a.txt"]);
        // Eager at startup; focusing must keep working.
        focus_branches(&mut fx);
        let branches = wait_for_branches(&mut fx.app, 1);
        assert!(branches.iter().any(|b| b.name == "main" && b.is_head));
    }

    #[test]
    fn create_branch_via_modal() {
        let mut fx = harness(&["a.txt"]);
        focus_branches(&mut fx);
        wait_for_branches(&mut fx.app, 1);
        fx.app.on_key(KeyCode::Char('a'));
        assert_eq!(fx.app.mode(), Mode::NewBranch);
        for c in "feat".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        let branches = wait_for_branches(&mut fx.app, 2);
        assert!(branches.iter().any(|b| b.name == "feat"));
    }

    #[test]
    fn checkout_branch_via_enter() {
        let mut fx = harness(&["a.txt"]);
        focus_branches(&mut fx);
        wait_for_branches(&mut fx.app, 1);
        // Create feat, then move selection to it and check out.
        fx.app.on_key(KeyCode::Char('a'));
        for c in "feat".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        wait_for_branches(&mut fx.app, 2);
        select_branch(&mut fx, "feat");
        fx.app.on_key(KeyCode::Enter);
        let st = wait_for(&mut fx.app, |st| st.branch == "feat");
        assert_eq!(st.branch, "feat");
    }

    #[test]
    fn delete_branch_via_key() {
        let mut fx = harness(&["a.txt"]);
        focus_branches(&mut fx);
        wait_for_branches(&mut fx.app, 1);
        fx.app.on_key(KeyCode::Char('a'));
        for c in "gone".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        wait_for_branches(&mut fx.app, 2);
        // Select "gone" and delete it.
        select_branch(&mut fx, "gone");
        fx.app.on_key(KeyCode::Char('D'));
        fx.app.on_key(KeyCode::Enter);
        let branches = wait_for_branches(&mut fx.app, 1);
        // Wait until "gone" disappears (list reloads after MutationDone).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            if let Some(b) = fx.app.branches() {
                if !b.iter().any(|x| x.name == "gone") {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "gone never deleted: {branches:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn delete_checked_out_branch_surfaces_error() {
        let mut fx = harness(&["a.txt"]);
        focus_branches(&mut fx);
        wait_for_branches(&mut fx.app, 1);
        // "main" is checked out; deleting it must fail loudly.
        fx.app.on_key(KeyCode::Char('D'));
        fx.app.on_key(KeyCode::Enter);
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("checked out"), "got: {err}");
    }

    fn wait_for_log(app: &mut App, min_count: usize) -> Vec<CommitInfo> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(entries) = app.log() {
                if entries.len() >= min_count {
                    return entries.to_vec();
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for log");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn log_loads_newest_first_when_focused() {
        let mut fx = harness(&["a.txt"]);
        // Eager at startup; focusing must keep working.
        fx.app.on_key(KeyCode::Char('3'));
        let entries = wait_for_log(&mut fx.app, 1);
        assert_eq!(entries[0].summary, "init");
    }

    #[test]
    fn new_commit_appears_at_top_of_log() {
        let mut fx = harness(&["a.txt"]);
        // Stage + commit, then open the log.
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char('c'));
        for c in "second".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('3'));
        let entries = wait_for_log(&mut fx.app, 2);
        assert_eq!(entries[0].summary, "second");
        assert_eq!(entries[1].summary, "init");
    }

    #[test]
    fn j_k_move_the_log_selection_and_clamp_at_ends() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char('c'));
        for c in "second".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('3'));
        wait_for_log(&mut fx.app, 2);
        assert_eq!(fx.app.log_selected(), 0);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.log_selected(), 1, "j must move to the next commit");
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.log_selected(), 1, "must clamp at the oldest commit");
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.log_selected(), 0, "k must move back up");
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.log_selected(), 0, "must clamp at the newest commit");
    }

    fn wait_for_commit_overview(app: &mut App, oid: &str) -> git_tui_core::log::CommitOverview {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(o) = app.commit_overview() {
                if o.oid == oid {
                    return o.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for commit overview of {oid}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn selecting_a_commit_loads_its_overview() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('3'));
        let entries = wait_for_log(&mut fx.app, 1);
        let overview = wait_for_commit_overview(&mut fx.app, &entries[0].oid);
        assert_eq!(overview.summary, "init");
        assert_eq!(overview.author, "Test User");
        assert_eq!(overview.files.len(), 1);
        assert_eq!(overview.files[0].path, "a.txt");
    }

    #[test]
    fn moving_the_log_selection_switches_the_overview() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char('c'));
        for c in "second".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('3'));
        let entries = wait_for_log(&mut fx.app, 2);
        let top = wait_for_commit_overview(&mut fx.app, &entries[0].oid);
        assert_eq!(top.summary, "second");
        fx.app.on_key(KeyCode::Char('j'));
        let bottom = wait_for_commit_overview(&mut fx.app, &entries[1].oid);
        assert_eq!(bottom.summary, "init");
    }

    fn wait_for_stash(app: &mut App, count: usize) -> Vec<StashEntry> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.poll();
            if let Some(entries) = app.stash() {
                if entries.len() == count {
                    return entries.to_vec();
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for stash");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn focus_stash(fx: &mut Fixture) {
        fx.app.on_key(KeyCode::Char('4'));
        assert_eq!(fx.app.focus(), Focus::Stash);
    }

    #[test]
    fn stash_push_cleans_status_and_lists_entry() {
        let mut fx = harness(&["a.txt"]);
        focus_stash(&mut fx);
        wait_for_stash(&mut fx.app, 0);
        fx.app.on_key(KeyCode::Char('a'));
        assert_eq!(fx.app.mode(), Mode::StashPush);
        for c in "wip".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        let entries = wait_for_stash(&mut fx.app, 1);
        assert!(
            entries[0].message.contains("wip"),
            "got {:?}",
            entries[0].message
        );
        // Status is clean after the push.
        let st = wait_for(&mut fx.app, |st| st.files.is_empty());
        assert!(st.files.is_empty());
    }

    #[test]
    fn stash_pop_restores_changes() {
        let mut fx = harness(&["a.txt"]);
        focus_stash(&mut fx);
        wait_for_stash(&mut fx.app, 0);
        fx.app.on_key(KeyCode::Char('a'));
        for c in "wip".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        wait_for_stash(&mut fx.app, 1);
        // Pop it back.
        fx.app.on_key(KeyCode::Enter);
        wait_for_stash(&mut fx.app, 0);
        let st = wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        assert_eq!(st.files[0].state, FileState::Unstaged);
    }

    #[test]
    fn stash_drop_removes_entry() {
        let mut fx = harness(&["a.txt"]);
        focus_stash(&mut fx);
        wait_for_stash(&mut fx.app, 0);
        fx.app.on_key(KeyCode::Char('a'));
        for c in "wip".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        wait_for_stash(&mut fx.app, 1);
        fx.app.on_key(KeyCode::Char('D'));
        fx.app.on_key(KeyCode::Enter);
        wait_for_stash(&mut fx.app, 0);
    }

    #[test]
    fn overridden_bindings_take_effect() {
        use crate::config::Config;
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "base\n", "init");
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(repo.workdir().unwrap().join("a.txt"))
                .unwrap();
            f.write_all(b"dirty\n").unwrap();
        }
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let keys = crate::config::KeyBindings {
            stage: vec![KeyCode::Char('s')],
            ..Default::default()
        };
        let config = Config {
            keys,
            ..Default::default()
        };
        let mut fx = Fixture {
            _dir: dir,
            app: App::new_with_config(JobQueue::spawn(path).unwrap(), config),
        };
        wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        // Space is no longer bound: nothing happens.
        fx.app.on_key(KeyCode::Char(' '));
        std::thread::sleep(Duration::from_millis(200));
        fx.app.poll();
        assert!(fx.app.error().is_none());
        let st = fx.app.status().unwrap();
        assert_eq!(st.files[0].state, FileState::Unstaged);
        // "s" stages.
        fx.app.on_key(KeyCode::Char('s'));
        let st = wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.path == "a.txt" && e.state == FileState::Staged)
        });
        assert_eq!(st.files[0].state, FileState::Staged);
    }

    #[test]
    fn ctrl_g_in_commit_box_starts_generation_and_no_staged_errors() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.mode(), Mode::Committing);
        fx.app.on_chord(Chord::Ctrl('g'));
        assert!(fx.app.is_generating());
        let err = wait_for_error(&mut fx.app);
        assert!(
            err.contains("nothing staged") || err.contains("API key"),
            "got: {err}"
        );
        assert!(!fx.app.is_generating());
        assert_eq!(fx.app.mode(), Mode::Committing, "stay in the box to edit");
    }

    #[test]
    fn capital_a_types_in_the_commit_box_with_or_without_shift() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        // "Add ..." has to be typable: Shift+A is a letter, not a command.
        fx.app.on_key_with_modifiers(KeyCode::Char('A'), true);
        fx.app.on_key_with_modifiers(KeyCode::Char('A'), false);
        fx.app.on_key_with_modifiers(KeyCode::Char('a'), false);
        assert!(!fx.app.is_generating());
        assert_eq!(fx.app.draft(), "AAa");
        // Generation does not type into the draft.
        fx.app.on_chord(Chord::Ctrl('g'));
        assert!(fx.app.is_generating());
        assert_eq!(fx.app.draft(), "AAa");
    }

    #[test]
    fn generated_message_fills_draft_and_moves_cursor_to_end() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.mode(), Mode::Committing);
        fx.app.generating = true;
        fx.app.gen_draft = Some(String::new());
        fx.app
            .apply(git_tui_core::jobqueue::AsyncResult::GeneratedMessage(
                "feat: add thing".into(),
            ));
        assert_eq!(fx.app.draft(), "feat: add thing");
        assert_eq!(fx.app.draft_cursor(), fx.app.draft().chars().count());
        assert!(!fx.app.is_generating());
        assert!(fx.app.error().is_none());
    }

    #[test]
    fn generated_message_arriving_after_esc_is_ignored() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.generating = true;
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        fx.app.generating = true;
        fx.app
            .apply(git_tui_core::jobqueue::AsyncResult::GeneratedMessage(
                "feat: late".into(),
            ));
        assert!(!fx.app.is_generating());
        assert_eq!(fx.app.draft(), "", "closed box must not be filled");
        // It is not thrown away either: it waits in the box.
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.draft(), "feat: late");
    }

    #[test]
    fn generated_message_does_not_overwrite_text_typed_meanwhile() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.generating = true;
        fx.app.gen_draft = Some(String::new());
        fx.app.on_key(KeyCode::Char('x'));
        fx.app
            .apply(git_tui_core::jobqueue::AsyncResult::GeneratedMessage(
                "feat: generated".into(),
            ));
        assert_eq!(fx.app.draft(), "x");
        assert!(fx.app.notice().unwrap_or("").contains("dropped"));
    }

    #[test]
    fn shift_a_in_file_list_opens_llm_settings_prefilled() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.mode(), Mode::Normal);
        fx.app.on_key(KeyCode::Char('A'));
        assert_eq!(fx.app.mode(), Mode::LlmSettings);
        assert_eq!(fx.app.llm_selected(), 0);
        // Prefilled from the session config (defaults here).
        assert_eq!(fx.app.llm_field_value(0), "openai");
        assert_eq!(fx.app.llm_field_value(1), "gpt-4o-mini");
    }

    #[test]
    fn llm_settings_tab_switches_fields_and_typing_edits() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.llm_selected(), 1);
        for c in "x-model".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        assert!(fx.app.llm_field_value(1).contains("x-model"));
        fx.app.on_key(KeyCode::Up);
        assert_eq!(fx.app.llm_selected(), 0);
        // Row 0 kept its prefilled value (stash/unstash round-trip).
        assert_eq!(fx.app.llm_field_value(0), "openai");
    }

    #[test]
    fn llm_settings_esc_cancels_without_applying() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        fx.app.on_key(KeyCode::Tab);
        fx.app.on_key(KeyCode::Tab);
        for c in "junk".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.llm.provider, "openai", "cancel must not apply");
        assert!(fx.app.notice().is_none());
    }

    #[test]
    fn llm_settings_enter_with_bad_provider_stays_open() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        fx.app.clear_draft();
        for c in "skynet".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::LlmSettings, "bad input stays open");
        assert!(fx.app.error().unwrap_or("").contains("unknown provider"));
        assert_eq!(fx.app.llm.provider, "openai", "bad input not applied");
    }

    #[test]
    fn llm_settings_arrows_cycle_provider_and_follow_with_model() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        // openai → openrouter on the first press, wrapping at the list end.
        assert_eq!(fx.app.llm_field_value(0), "openai");
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.llm_field_value(0), "custom", "← wraps to the end");
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.llm_field_value(0), "openai", "→ wraps to the start");
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.llm_field_value(0), "openrouter");
        // Models come from the provider, not a compiled-in list: with
        // none fetched the row is plain text and shows why.
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.llm_selected(), 1);
        assert!(fx.app.llm_row_options(1).is_empty(), "nothing fetched yet");
        assert!(
            fx.app.llm_models_error().unwrap_or("").contains("API key"),
            "no key means no listing: {:?}",
            fx.app.llm_models_error()
        );
        // Save applies the pair as typed.
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.llm.provider, "openrouter");
        assert_eq!(fx.app.llm.model, "gpt-4o-mini", "model kept as prefilled");
    }

    #[test]
    fn fetched_models_fill_and_cycle_the_model_row() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        // Pretend the provider answered with its real list.
        let id = fx.app.llm_models_request;
        fx.app.apply(git_tui_core::jobqueue::AsyncResult::Models {
            id,
            models: vec![
                "anthropic/claude-3.5-haiku".into(),
                "openai/gpt-4o-mini".into(),
            ],
        });
        assert!(!fx.app.llm_models_loading());
        assert!(fx.app.llm_models_error().is_none());
        // The row takes the first id the provider reported.
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.llm_selected(), 1);
        assert_eq!(fx.app.llm_field_value(1), "anthropic/claude-3.5-haiku");
        assert_eq!(fx.app.llm_row_options(1).len(), 2);
        // `←/→` now cycle the fetched ids instead of editing text.
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.llm_field_value(1), "openai/gpt-4o-mini");
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.llm_field_value(1), "anthropic/claude-3.5-haiku");
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.llm.model, "anthropic/claude-3.5-haiku");
    }

    #[test]
    fn stale_model_list_for_another_provider_is_dropped() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        let first = fx.app.llm_models_request;
        // ...user switches provider before it lands.
        fx.app.on_key(KeyCode::Right);
        assert_eq!(fx.app.llm_field_value(0), "openrouter");
        assert!(fx.app.llm_models_request > first, "a new request went out");
        fx.app.apply(git_tui_core::jobqueue::AsyncResult::Models {
            id: first,
            models: vec!["gpt-4o-mini".into()],
        });
        assert!(
            fx.app.llm_row_options(1).is_empty(),
            "openai's list must not fill the row for openrouter"
        );
    }

    #[test]
    fn model_fetch_error_leaves_the_row_usable() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        let id = fx.app.llm_models_request;
        fx.app
            .apply(git_tui_core::jobqueue::AsyncResult::ModelsError {
                id,
                error: "provider HTTP 401: bad key".into(),
            });
        assert!(!fx.app.llm_models_loading());
        assert!(fx.app.llm_models_error().unwrap().contains("401"));
        // Typing still works and the form still saves.
        fx.app.on_key(KeyCode::Tab);
        fx.app.clear_draft();
        for c in "gpt-4o".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.llm.model, "gpt-4o");
    }

    #[test]
    fn saving_without_a_model_is_refused() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        fx.app.on_key(KeyCode::Tab);
        for _ in 0..20 {
            fx.app.on_key(KeyCode::Delete);
        }
        assert_eq!(fx.app.llm_field_value(1), "");
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::LlmSettings, "form stays open");
        assert!(fx.app.error().unwrap().contains("no model"));
    }

    #[test]
    fn llm_settings_custom_provider_keeps_typed_model() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('A'));
        fx.app.clear_draft();
        for c in "custom".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        // A typed value outside the list starts from its near end.
        fx.app.on_key(KeyCode::Left);
        assert_eq!(fx.app.llm_field_value(0), "gemini");
        fx.app.clear_draft();
        for c in "custom".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        // Nothing has been fetched for `custom`, so on the model row the
        // arrows are plain text editing again.
        fx.app.on_key(KeyCode::Tab);
        assert!(fx.app.llm_row_options(1).is_empty());
        fx.app.clear_draft();
        for c in "my-local-model".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        for _ in 0..4 {
            fx.app.on_key(KeyCode::Left);
        }
        fx.app.on_key(KeyCode::Char('X'));
        assert_eq!(fx.app.llm_field_value(1), "my-local-mXodel");
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.llm.provider, "custom");
        assert_eq!(fx.app.llm.model, "my-local-mXodel");
    }

    #[test]
    fn llm_settings_enter_saves_and_persists_to_file() {
        let mut fx = harness(&["a.txt"]);
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_path = dir.path().join("config.toml");
        fx.app.set_config_path(Some(cfg_path.clone()));
        fx.app.on_key(KeyCode::Char('A'));
        // Provider row: replace "openai" with "ollama".
        fx.app.clear_draft();
        for c in "ollama".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.llm.provider, "ollama");
        assert!(fx.app.notice().unwrap_or("").contains("saved"));
        // Round-trips through the real config file.
        let cfg = Config::load_from_path(&cfg_path).unwrap();
        assert_eq!(cfg.llm.provider, "ollama");
    }

    #[test]
    fn theme_picker_previews_live_and_esc_restores() {
        let mut fx = harness(&["a.txt"]);
        let before = fx.app.theme();
        fx.app.on_key(KeyCode::Char('T'));
        assert_eq!(fx.app.mode(), Mode::ThemePicker);
        // Opens on the active theme (`default`, row 0).
        assert_eq!(fx.app.theme_selected(), 0);
        fx.app.on_key(KeyCode::Down);
        assert_eq!(fx.app.theme(), Theme::by_name("tokyo-night").unwrap());
        // Wraps upward from the first row to the last theme.
        fx.app.on_key(KeyCode::Up);
        fx.app.on_key(KeyCode::Up);
        let last = Theme::names().last().unwrap();
        assert_eq!(fx.app.theme(), Theme::by_name(last).unwrap());
        // Keys that act elsewhere are inert inside the picker.
        fx.app.on_key(KeyCode::Char('d'));
        assert_eq!(fx.app.mode(), Mode::ThemePicker);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.theme(), before);
    }

    #[test]
    fn theme_picker_enter_keeps_theme_and_saves_config() {
        use crate::config::Config;
        let mut fx = harness(&["a.txt"]);
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_path = dir.path().join("config.toml");
        fx.app.set_config_path(Some(cfg_path.clone()));
        fx.app.on_key(KeyCode::Char('T'));
        let gruvbox = Theme::names().position(|n| n == "gruvbox").unwrap();
        for _ in 0..gruvbox {
            fx.app.on_key(KeyCode::Char('j'));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.theme(), Theme::by_name("gruvbox").unwrap());
        assert!(fx.app.notice().unwrap_or("").contains("gruvbox"));
        let cfg = Config::load_from_path(&cfg_path).unwrap();
        assert_eq!(cfg.theme, Theme::by_name("gruvbox").unwrap());
        // Reopening starts on the theme that is now active.
        fx.app.on_key(KeyCode::Char('T'));
        assert_eq!(fx.app.theme_selected(), gruvbox);
    }

    #[test]
    fn theme_picker_without_config_path_applies_for_the_session() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('T'));
        fx.app.on_key(KeyCode::End);
        fx.app.on_key(KeyCode::Enter);
        let last = Theme::names().last().unwrap();
        assert_eq!(fx.app.theme(), Theme::by_name(last).unwrap());
        assert!(fx.app.notice().unwrap_or("").contains("this session"));
    }

    #[test]
    fn theme_picker_click_previews_then_second_click_keeps() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('T'));
        fx.app
            .set_hit(HitMap::ThemePicker, vec![(5, 0), (6, 1), (7, 2)]);
        fx.app.on_mouse_down(10, 6);
        assert_eq!(fx.app.mode(), Mode::ThemePicker);
        assert_eq!(fx.app.theme(), Theme::by_name("tokyo-night").unwrap());
        fx.app.on_mouse_down(10, 6);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.theme(), Theme::by_name("tokyo-night").unwrap());
    }

    fn type_into_menu(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(KeyCode::Char(c));
        }
    }

    #[test]
    fn action_menu_filters_and_runs_the_selected_action() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('?'));
        assert_eq!(fx.app.mode(), Mode::ActionMenu);
        let all = fx.app.menu_matches().len();
        type_into_menu(&mut fx.app, "commit");
        let matches = fx.app.menu_matches();
        assert!(matches.len() < all);
        assert_eq!(matches[0].label, "Commit");
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Committing);
        // The filter text must not leak into the commit message.
        assert_eq!(fx.app.draft(), "");
    }

    #[test]
    fn action_menu_typed_keys_never_fire_their_bindings() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('?'));
        // `d` discards and `Q` quits in the file list; here they are text.
        type_into_menu(&mut fx.app, "dQ");
        assert_eq!(fx.app.mode(), Mode::ActionMenu);
        assert_eq!(fx.app.draft(), "dQ");
        assert!(!fx.app.should_quit());
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.draft(), "");
    }

    #[test]
    fn action_menu_destructive_entry_goes_through_the_confirm_box() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('?'));
        type_into_menu(&mut fx.app, "discard");
        assert_eq!(fx.app.menu_matches()[0].label, "Discard changes");
        fx.app.on_key(KeyCode::Enter);
        // Not run: the confirm box names the file it would discard.
        assert_eq!(fx.app.mode(), Mode::Confirm);
        assert_eq!(
            fx.app.confirm_prompt(),
            Some("Discard all changes to a.txt?")
        );
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        fx.app.poll();
        let status = fx.app.status().unwrap();
        assert!(status.files.iter().any(|e| e.path == "a.txt"));
    }

    #[test]
    fn action_menu_new_branch_moves_to_the_branches_panel() {
        let mut fx = harness(&["a.txt"]);
        assert_eq!(fx.app.focus(), Focus::Status);
        fx.app.on_key(KeyCode::Char('?'));
        type_into_menu(&mut fx.app, "new branch");
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::NewBranch);
        assert_eq!(fx.app.focus(), Focus::Branches);
    }

    #[test]
    fn action_menu_project_actions_are_handed_to_the_workspace() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('?'));
        type_into_menu(&mut fx.app, "open project");
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(
            fx.app.take_workspace_request(),
            Some(WorkspaceRequest::Open)
        );
        assert_eq!(fx.app.take_workspace_request(), None);
    }

    #[test]
    fn menu_button_opens_the_menu_and_rows_are_clickable() {
        let mut fx = harness(&["a.txt"]);
        let layout = mouse_layout(&mut fx.app);
        fx.app
            .on_mouse_down(button_x(&fx.app, "Menu") + 1, layout.footer.y);
        assert_eq!(fx.app.mode(), Mode::ActionMenu);
        let commit = fx
            .app
            .menu_matches()
            .iter()
            .position(|e| e.label == "Commit")
            .unwrap();
        fx.app.set_hit(HitMap::ActionMenu, vec![(9, commit)]);
        fx.app.on_mouse_down(20, 9);
        assert_eq!(fx.app.mode(), Mode::ActionMenu);
        assert_eq!(fx.app.menu_selected(), commit);
        fx.app.on_mouse_down(20, 9);
        assert_eq!(fx.app.mode(), Mode::Committing);
    }

    // ---- safety: confirmations, modifiers, paste, commit box ----

    #[test]
    fn d_asks_first_and_esc_keeps_the_file() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('d'));
        assert_eq!(fx.app.mode(), Mode::Confirm);
        assert_eq!(
            fx.app.confirm_prompt(),
            Some("Discard all changes to a.txt?")
        );
        // Keys that act elsewhere are inert in the box.
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Char(' '));
        assert_eq!(fx.app.mode(), Mode::Confirm);
        fx.app.on_key(KeyCode::Char('n'));
        assert_eq!(fx.app.mode(), Mode::Normal);
        fx.app.poll();
        let status = fx.app.status().unwrap();
        assert!(status.files.iter().any(|e| e.path == "a.txt"));
        // `y` confirms as well as Enter.
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Char('y'));
        assert_eq!(fx.app.mode(), Mode::Normal);
        wait_for(&mut fx.app, |st| st.files.is_empty());
    }

    #[test]
    fn confirm_names_the_folder_when_a_collapsed_folder_is_selected() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "src/a.rs", "a\n", "init");
        commit_file(&repo, "src/b.rs", "b\n", "init");
        let root = repo.workdir().unwrap().to_path_buf();
        std::fs::write(root.join("src/a.rs"), "a2\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "b2\n").unwrap();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(root).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.files.len() == 2);
        // Collapse `src/`: the selection is a file hidden inside it.
        fx.app.on_key(KeyCode::Left);
        fx.app.on_key(KeyCode::Char('d'));
        let prompt = fx.app.confirm_prompt().unwrap().to_string();
        assert!(
            prompt.contains("2 files under src/"),
            "the box must name the folder, got: {prompt}"
        );
    }

    #[test]
    fn untracked_file_prompt_says_delete() {
        let mut fx = harness(&["a.txt"]);
        let root = fx._dir.path().to_path_buf();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        fx.app.on_key(KeyCode::Char('r'));
        wait_for(&mut fx.app, |st| {
            st.files.iter().any(|e| e.path == "new.txt")
        });
        while fx.app.selected_file().map(|f| f.path.as_str()) != Some("new.txt") {
            fx.app.on_key(KeyCode::Char('j'));
        }
        fx.app.on_key(KeyCode::Char('d'));
        assert_eq!(
            fx.app.confirm_prompt(),
            Some("Delete untracked file new.txt?")
        );
    }

    #[test]
    fn confirm_can_be_switched_off_in_config() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "base\n", "init");
        let root = repo.workdir().unwrap().to_path_buf();
        std::fs::write(root.join("a.txt"), "dirty\n").unwrap();
        drop(repo);
        let mut config = Config::default();
        config.ui.confirm_destructive = false;
        let mut fx = Fixture {
            _dir: dir,
            app: App::new_with_config(JobQueue::spawn(root).unwrap(), config),
        };
        wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        fx.app.on_key(KeyCode::Char('d'));
        assert_eq!(fx.app.mode(), Mode::Normal, "no box when switched off");
        // A second press before the first lands must not hit the next row.
        fx.app.on_key(KeyCode::Char('d'));
        assert!(fx
            .app
            .notice()
            .unwrap_or("")
            .contains("still being applied"));
        wait_for(&mut fx.app, |st| st.files.is_empty());
    }

    #[test]
    fn chords_never_run_the_bare_key() {
        let mut fx = harness(&["a.txt"]);
        // Ctrl+D pages; it must not discard. Ctrl+X, Ctrl+Q do nothing.
        for chord in [
            Chord::Ctrl('d'),
            Chord::Ctrl('x'),
            Chord::Ctrl('q'),
            Chord::Alt('d'),
        ] {
            fx.app.on_chord(chord);
            assert_eq!(fx.app.mode(), Mode::Normal, "{chord:?}");
            assert!(!fx.app.should_quit());
        }
        fx.app.poll();
        let status = fx.app.status().unwrap();
        assert!(status.files.iter().any(|e| e.path == "a.txt"));
    }

    #[test]
    fn readline_keys_edit_the_commit_box() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_paste("fix parser bug");
        fx.app.on_chord(Chord::Ctrl('w'));
        assert_eq!(fx.app.draft(), "fix parser ");
        fx.app.on_chord(Chord::WordLeft);
        assert_eq!(fx.app.draft_cursor(), 4);
        fx.app.on_chord(Chord::Ctrl('k'));
        assert_eq!(fx.app.draft(), "fix ");
        fx.app.on_chord(Chord::Ctrl('a'));
        assert_eq!(fx.app.draft_cursor(), 0);
        fx.app.on_chord(Chord::Ctrl('e'));
        fx.app.on_chord(Chord::Newline);
        fx.app.on_paste("body");
        assert_eq!(fx.app.draft(), "fix \nbody");
        // Ctrl+U clears the current line only.
        fx.app.on_chord(Chord::Ctrl('u'));
        assert_eq!(fx.app.draft(), "fix \n");
        assert_eq!(fx.app.mode(), Mode::Committing);
    }

    #[test]
    fn pasting_a_multi_line_message_fills_the_box_without_committing() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_paste("fix: thing\r\n\r\ndiscard q P body\n");
        assert_eq!(fx.app.mode(), Mode::Committing, "a paste never submits");
        assert_eq!(fx.app.draft(), "fix: thing\n\ndiscard q P body");
    }

    #[test]
    fn pasting_outside_a_text_box_does_nothing() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_paste("dQq p\n");
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert!(!fx.app.should_quit());
        assert!(fx.app.notice().unwrap_or("").contains("paste ignored"));
        fx.app.poll();
        assert!(fx
            .app
            .status()
            .unwrap()
            .files
            .iter()
            .any(|e| e.path == "a.txt"));
    }

    #[test]
    fn pasting_into_a_one_line_box_stays_on_one_line() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('2'));
        fx.app.on_key(KeyCode::Char('a'));
        assert_eq!(fx.app.mode(), Mode::NewBranch);
        fx.app.on_paste("feat/x\nrm -rf\n");
        assert_eq!(fx.app.mode(), Mode::NewBranch);
        assert_eq!(fx.app.draft(), "feat/x rm -rf");
    }

    #[test]
    fn esc_keeps_the_commit_message_for_next_time() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_paste("wip: half a thought");
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.draft(), "");
        // Another prompt in between does not disturb it.
        fx.app.on_key(KeyCode::Char('/'));
        fx.app.on_key(KeyCode::Esc);
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.draft(), "wip: half a thought");
        assert_eq!(fx.app.draft_cursor(), fx.app.draft().chars().count());
    }

    #[test]
    fn failed_commit_gives_the_message_back() {
        let mut fx = harness(&["a.txt"]);
        // Nothing staged: the commit is refused by the worker.
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_paste("feat: my carefully written message");
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("nothing to commit"), "got: {err}");
        assert_eq!(fx.app.mode(), Mode::Committing, "box reopens");
        assert_eq!(fx.app.draft(), "feat: my carefully written message");
    }

    #[test]
    fn successful_commit_clears_the_kept_message() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files.iter().any(|e| e.state == FileState::Staged)
        });
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_paste("feat: done");
        fx.app.on_key(KeyCode::Enter);
        wait_for(&mut fx.app, |st| st.files.is_empty());
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.draft(), "", "nothing left over after success");
    }

    #[test]
    fn error_from_a_failed_job_survives_the_refresh_that_follows() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('c'));
        fx.app.on_paste("x");
        fx.app.on_key(KeyCode::Enter);
        wait_for_error(&mut fx.app);
        // The failure triggers a status reload; let it land.
        for _ in 0..50 {
            fx.app.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(fx.app.error().is_some(), "error wiped by the refresh");
        // The next key press dismisses it.
        fx.app.on_key(KeyCode::Esc);
        assert!(fx.app.error().is_none());
    }

    #[test]
    fn q_in_the_fullscreen_diff_closes_the_diff_only() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('q'));
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert!(!fx.app.should_quit());
    }

    #[test]
    fn popup_buttons_and_click_outside_work_like_their_keys() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('d'));
        assert_eq!(fx.app.mode(), Mode::Confirm);
        let popup = Rect::new(10, 5, 40, 6);
        fx.app.set_modal_rect(popup);
        fx.app
            .add_click(Rect::new(12, 10, 5, 1), UiClick::Key(KeyCode::Enter));
        fx.app
            .add_click(Rect::new(18, 10, 4, 1), UiClick::Key(KeyCode::Esc));
        // [No]
        fx.app.on_mouse_down(19, 10);
        assert_eq!(fx.app.mode(), Mode::Normal);
        // Click outside the box cancels too.
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.set_modal_rect(popup);
        fx.app.on_mouse_down(0, 0);
        assert_eq!(fx.app.mode(), Mode::Normal);
        fx.app.poll();
        assert!(fx
            .app
            .status()
            .unwrap()
            .files
            .iter()
            .any(|e| e.path == "a.txt"));
        // [Yes]
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.set_modal_rect(popup);
        fx.app
            .add_click(Rect::new(12, 10, 5, 1), UiClick::Key(KeyCode::Enter));
        fx.app.on_mouse_down(13, 10);
        wait_for(&mut fx.app, |st| st.files.is_empty());
    }

    // ---- mouse ----

    /// Make the next press count as a fresh single click, as if the last
    /// one was long ago.
    fn forget_last_click(app: &mut App) {
        app.last_click = None;
    }

    #[test]
    fn wheel_scrolls_under_the_pointer_without_moving_focus() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        let layout = mouse_layout(&mut fx.app);
        fx.app.on_key(KeyCode::Char('2'));
        assert_eq!(fx.app.focus(), Focus::Branches);
        // Over the file list: the selection steps, focus stays.
        fx.app
            .on_wheel(layout.files.x + 2, layout.files.y + 2, false);
        assert_eq!(fx.app.selected(), 1);
        assert_eq!(fx.app.focus(), Focus::Branches);
        // Over the diff: still no focus change.
        fx.app.on_wheel(layout.diff.x + 2, layout.diff.y + 2, false);
        assert_eq!(fx.app.focus(), Focus::Branches);
    }

    #[test]
    fn wheel_in_the_diff_scrolls_the_view_and_the_cursor_follows() {
        let mut fx = two_hunk_fixture();
        let layout = mouse_layout(&mut fx.app);
        fx.app.set_view_h_for_test(10, 10);
        assert_eq!(fx.app.diff_scroll(), 0);
        assert_eq!(fx.app.cursor_row(), 0);
        let (x, y) = (layout.diff.x + 2, layout.diff.y + 2);
        fx.app.on_wheel(x, y, false);
        // The view moved at once (it used to wait for the cursor to reach
        // the bottom edge), and the cursor was pulled inside it.
        assert_eq!(fx.app.diff_scroll(), 3);
        assert!(
            fx.app.cursor_row() >= 1,
            "cursor left behind above the view"
        );
        fx.app.on_wheel(x, y, true);
        assert_eq!(fx.app.diff_scroll(), 0);
    }

    #[test]
    fn wheel_moves_the_selection_inside_popups() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('?'));
        fx.app.on_wheel(0, 0, false);
        fx.app.on_wheel(0, 0, false);
        assert_eq!(fx.app.menu_selected(), 2);
        fx.app.on_wheel(0, 0, true);
        assert_eq!(fx.app.menu_selected(), 1);
        // No wrap at the ends.
        for _ in 0..5 {
            fx.app.on_wheel(0, 0, true);
        }
        assert_eq!(fx.app.menu_selected(), 0);
        fx.app.on_key(KeyCode::Esc);
        fx.app.on_key(KeyCode::Char('T'));
        fx.app.on_wheel(0, 0, false);
        assert_eq!(fx.app.theme(), Theme::by_name("tokyo-night").unwrap());
    }

    #[test]
    fn clicking_a_panel_title_does_not_select_a_row() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        let layout = mouse_layout(&mut fx.app);
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.selected(), 1);
        // The title border row of the files panel.
        fx.app.on_mouse_down(layout.files.x + 3, layout.files.y);
        assert_eq!(fx.app.selected(), 1, "title click must not jump to row 0");
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn one_click_on_the_selected_file_does_not_open_it() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        let layout = mouse_layout(&mut fx.app);
        let y0 = layout.files.y + 1;
        // a.txt is selected already; coming back to the list with one
        // click must only focus it.
        fx.app.on_key(KeyCode::Char('5'));
        fx.app.on_mouse_down(2, y0);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert_eq!(fx.app.focus(), Focus::Status);
        // A slow second click is still just a click.
        forget_last_click(&mut fx.app);
        fx.app.on_mouse_down(2, y0);
        assert_eq!(fx.app.mode(), Mode::Normal);
        // Two quick ones open it.
        fx.app.on_mouse_down(2, y0);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
    }

    #[test]
    fn right_click_selects_then_opens_the_menu_for_that_panel() {
        let mut fx = harness(&["a.txt", "b.txt"]);
        let layout = mouse_layout(&mut fx.app);
        fx.app.on_right_click(2, layout.files.y + 2);
        assert_eq!(fx.app.mode(), Mode::ActionMenu);
        assert_eq!(fx.app.selected_file().unwrap().path, "b.txt");
        assert!(fx
            .app
            .menu_matches()
            .iter()
            .any(|e| e.label == "Discard changes"));
        fx.app.on_key(KeyCode::Esc);
        // Over the branches panel the menu is the branches menu.
        fx.app.on_right_click(2, layout.branches.y + 1);
        assert_eq!(fx.app.focus(), Focus::Branches);
        assert!(fx
            .app
            .menu_matches()
            .iter()
            .all(|e| e.label != "Discard changes"));
    }

    #[test]
    fn clicking_a_folder_arrow_folds_and_unfolds_it() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "src/a.rs", "a\n", "init");
        commit_file(&repo, "top.txt", "t\n", "init");
        let root = repo.workdir().unwrap().to_path_buf();
        std::fs::write(root.join("src/a.rs"), "a2\n").unwrap();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(root).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.files.len() == 1);
        let layout = mouse_layout(&mut fx.app);
        let header_y = layout.files.y + 1;
        assert!(!fx.app.is_collapsed("src"));
        // The arrow sits right after the border and selection marker.
        fx.app.on_mouse_down(layout.files.x + 3, header_y);
        assert!(fx.app.is_collapsed("src"), "arrow click folds");
        forget_last_click(&mut fx.app);
        fx.app.on_mouse_down(layout.files.x + 3, header_y);
        assert!(!fx.app.is_collapsed("src"), "arrow click unfolds");
        // A click on the name only selects.
        forget_last_click(&mut fx.app);
        fx.app.on_mouse_down(layout.files.x + 12, header_y);
        assert!(!fx.app.is_collapsed("src"));
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn dragging_over_the_diff_selects_lines_for_copy() {
        let mut fx = two_hunk_fixture();
        let layout = mouse_layout(&mut fx.app);
        let y = layout.diff.y + 2;
        fx.app.set_hit(
            HitMap::Preview,
            vec![(y, 1), (y + 1, 2), (y + 2, 3), (y + 3, 4)],
        );
        let x = layout.diff.x + 4;
        fx.app.on_mouse_down(x, y);
        // A press without moving selects nothing.
        assert!(fx.app.visual().is_none());
        fx.app.on_mouse_drag(x, y + 2);
        let visual = fx.app.visual().expect("drag starts a selection");
        assert_eq!(visual.mode, VisualMode::Linewise);
        assert_eq!(visual.anchor_row, 1);
        assert_eq!(fx.app.cursor_row(), 3);
        fx.app.on_mouse_up();
        // The selection survives the release and yanks three lines.
        let text = fx.app.yank_text().unwrap();
        assert_eq!(text.lines().count(), 3, "got {text:?}");
        // Dragging without a press in the diff does nothing.
        fx.app.on_mouse_drag(x, y + 3);
        assert_eq!(fx.app.cursor_row(), 3);
    }

    #[test]
    fn pressing_a_divider_without_moving_does_not_resize() {
        let mut fx = harness(&["a.txt"]);
        let layout = mouse_layout(&mut fx.app);
        let before = fx.app.layout_overrides();
        let dx = crate::ui::rail_divider_x(&layout);
        fx.app.on_mouse_down(dx, layout.files.y + 1);
        fx.app.on_mouse_up();
        assert_eq!(fx.app.layout_overrides(), before);
    }

    #[test]
    fn menu_opens_from_the_fullscreen_diff_and_returns_to_it() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char('?'));
        assert_eq!(fx.app.mode(), Mode::ActionMenu);
        let labels: Vec<&str> = fx.app.menu_matches().iter().map(|e| e.label).collect();
        assert!(labels.contains(&"Restore hunk"), "{labels:?}");
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::FullDiff, "esc returns to the diff");
        fx.app.on_key(KeyCode::Char('?'));
        type_into_menu(&mut fx.app, "close");
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Normal);
    }

    #[test]
    fn commits_panel_offers_no_stage_button() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('3'));
        let labels: Vec<&str> = crate::ui::footer_buttons(&fx.app)
            .iter()
            .map(|(label, _)| *label)
            .collect();
        assert!(!labels.contains(&"Stage"), "{labels:?}");
        assert!(labels.contains(&"Menu"));
    }

    // ---- keyboard navigation ----

    #[test]
    fn g_and_shift_g_jump_to_the_ends_of_lists_and_diffs() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        fx.app.on_key(KeyCode::Char('G'));
        assert_eq!(fx.app.selected_file().unwrap().path, "c.txt");
        fx.app.on_key(KeyCode::Char('g'));
        assert_eq!(fx.app.selected_file().unwrap().path, "a.txt");
        fx.app.on_key(KeyCode::End);
        assert_eq!(fx.app.selected_file().unwrap().path, "c.txt");
        fx.app.on_key(KeyCode::Home);
        assert_eq!(fx.app.selected_file().unwrap().path, "a.txt");

        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.set_view_h_for_test(5, 5);
        fx.app.on_key(KeyCode::Char('G'));
        assert_eq!(fx.app.cursor_row(), fx.app.diff_rows.len() - 1);
        assert_eq!(fx.app.hunk(), 1);
        fx.app.on_key(KeyCode::Char('g'));
        assert_eq!((fx.app.cursor_row(), fx.app.diff_scroll()), (0, 0));
    }

    #[test]
    fn shift_tab_cycles_panels_backwards() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::BackTab);
        assert_eq!(fx.app.focus(), Focus::Stash);
        fx.app.on_key(KeyCode::BackTab);
        assert_eq!(fx.app.focus(), Focus::Log);
        fx.app.on_key(KeyCode::Tab);
        assert_eq!(fx.app.focus(), Focus::Stash);
        // Shift+Right reaches the diff from any rail panel.
        fx.app.on_key_with_modifiers(KeyCode::Right, true);
        assert_eq!(fx.app.focus(), Focus::Diff);
    }

    #[test]
    fn search_in_diff_jumps_between_matches() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.set_view_h_for_test(8, 8);
        fx.app.on_chord(Chord::Ctrl('f'));
        assert_eq!(fx.app.mode(), Mode::SearchDiff);
        for c in "CHANGED".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff, "back in the diff");
        let first = fx.app.cursor_row();
        let text =
            |app: &App| cursor_line_text(app.diff().unwrap(), &app.diff_rows[app.cursor_row()]);
        assert!(text(&fx.app).contains("CHANGED"), "cursor not on a match");
        assert!(fx.app.notice().unwrap().starts_with("match 1 of "));
        // `n` goes on, wrapping at the end; `N` comes back.
        fx.app.on_key(KeyCode::Char('n'));
        let second = fx.app.cursor_row();
        assert!(second > first);
        assert!(text(&fx.app).contains("CHANGED"));
        fx.app.on_key(KeyCode::Char('N'));
        assert_eq!(fx.app.cursor_row(), first);
        fx.app.on_key(KeyCode::Char('N'));
        assert!(fx.app.cursor_row() > first, "N wraps to the last match");
        // Search is case-insensitive and reports a miss.
        fx.app.on_chord(Chord::Ctrl('f'));
        fx.app.on_chord(Chord::Ctrl('u'));
        for c in "nothing-like-this".chars() {
            fx.app.on_key(KeyCode::Char(c));
        }
        fx.app.on_key(KeyCode::Enter);
        assert!(fx.app.error().unwrap().contains("no match"));
    }

    #[test]
    fn brackets_step_through_files_inside_the_fullscreen_diff() {
        let mut fx = harness(&["a.txt", "b.txt", "c.txt"]);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        fx.app.on_key(KeyCode::Char(']'));
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert_eq!(fx.app.selected_file().unwrap().path, "b.txt");
        wait_for_diff(&mut fx.app, "b.txt");
        fx.app.on_key(KeyCode::Char(']'));
        fx.app.on_key(KeyCode::Char(']'));
        assert_eq!(fx.app.selected_file().unwrap().path, "c.txt");
        assert_eq!(fx.app.notice(), Some("last file"));
        fx.app.on_key(KeyCode::Char('['));
        assert_eq!(fx.app.selected_file().unwrap().path, "b.txt");
    }

    #[test]
    fn staging_a_hunk_keeps_your_place_in_the_diff() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Enter);
        fx.app.set_view_h_for_test(8, 8);
        // Stage the first of two hunks: the remaining one is where the
        // cursor should be, and with more hunks you would stay at the
        // same index instead of being thrown back to the top.
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.state == FileState::BothStagedAndUnstaged)
        });
        wait_for_diff(&mut fx.app, "a.txt");
        assert_eq!(fx.app.diff().unwrap().hunks.len(), 1);
        assert_eq!(fx.app.hunk(), 0);

        // With three hunks, staging the second leaves you on the hunk
        // that takes its place.
        let (dir, repo) = init_repo();
        let base = (1..=60).map(|i| format!("line {i}\n")).collect::<String>();
        commit_file(&repo, "a.txt", &base, "init");
        let dirty = base
            .replacen("line 5\n", "line 5 X\n", 1)
            .replacen("line 30\n", "line 30 X\n", 1)
            .replacen("line 55\n", "line 55 X\n", 1);
        let root = repo.workdir().unwrap().to_path_buf();
        std::fs::write(root.join("a.txt"), dirty).unwrap();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(root).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        wait_for_diff(&mut fx.app, "a.txt");
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('J'));
        assert_eq!(fx.app.hunk(), 1);
        fx.app.on_key(KeyCode::Char(' '));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            if fx.app.diff().is_some_and(|d| d.hunks.len() == 2) {
                break;
            }
            assert!(Instant::now() < deadline, "diff never reloaded");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fx.app.hunk(), 1, "thrown back to the first hunk");
        assert!(fx.app.cursor_row() > 0);
    }

    #[test]
    fn help_screen_opens_scrolls_and_closes() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::F(1));
        assert_eq!(fx.app.mode(), Mode::Help);
        // Keys that act elsewhere only scroll or do nothing here.
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Char('j'));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(fx.app.clamp_help_scroll(100), 2);
        fx.app.on_key(KeyCode::Char('G'));
        assert_eq!(fx.app.clamp_help_scroll(40), 40, "pulled back to the end");
        fx.app.on_key(KeyCode::Char('k'));
        assert_eq!(fx.app.clamp_help_scroll(40), 39);
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        // From the fullscreen diff it returns there.
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::F(1));
        fx.app.on_key(KeyCode::Char('q'));
        assert_eq!(fx.app.mode(), Mode::FullDiff);
    }

    #[test]
    fn selection_does_not_outlive_leaving_the_diff_panel() {
        let mut fx = two_hunk_fixture();
        fx.app.on_key(KeyCode::Char('5'));
        fx.app.on_key(KeyCode::Char('V'));
        assert!(fx.app.visual().is_some());
        fx.app.on_key(KeyCode::Char('1'));
        assert!(
            fx.app.visual().is_none(),
            "selection lingered after leaving"
        );
    }

    // ---- staged side, history, push guard ----

    #[test]
    fn shift_s_shows_the_staged_half_of_a_partly_staged_file() {
        let mut fx = two_hunk_fixture();
        // Not partly staged yet: nothing to flip.
        fx.app.on_key(KeyCode::Char('S'));
        assert!(fx.app.notice().unwrap().contains("partly staged"));
        // Stage one of the two hunks.
        fx.app.on_key(KeyCode::Char('5'));
        fx.app.on_key(KeyCode::Char(' '));
        wait_for(&mut fx.app, |st| {
            st.files
                .iter()
                .any(|e| e.state == FileState::BothStagedAndUnstaged)
        });
        wait_for_diff(&mut fx.app, "a.txt");
        assert!(!fx.app.diff_for.as_ref().unwrap().1, "unstaged by default");
        let unstaged = fx.app.diff().unwrap().clone();
        fx.app.on_key(KeyCode::Char('S'));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fx.app.poll();
            let staged_view = fx.app.diff_for.as_ref().is_some_and(|(_, staged)| *staged);
            if staged_view && fx.app.diff().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "staged side never loaded");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(*fx.app.diff().unwrap(), unstaged, "same diff shown twice");
        assert_eq!(fx.app.diff().unwrap().hunks.len(), 1);
        // `x` on the staged side unstages the hunk: nothing is lost, so it
        // does not ask.
        fx.app.on_key(KeyCode::Char('x'));
        assert_eq!(fx.app.mode(), Mode::Normal);
        wait_for(&mut fx.app, |st| {
            st.files.iter().any(|e| e.state == FileState::Unstaged)
        });
    }

    #[test]
    fn staged_side_is_per_file() {
        let (dir, repo) = init_repo();
        let base = (1..=40).map(|i| format!("line {i}\n")).collect::<String>();
        commit_file(&repo, "a.txt", &base, "init");
        commit_file(&repo, "b.txt", &base, "init");
        let root = repo.workdir().unwrap().to_path_buf();
        let dirty =
            base.replacen("line 5\n", "line 5 X\n", 1)
                .replacen("line 35\n", "line 35 X\n", 1);
        std::fs::write(root.join("a.txt"), &dirty).unwrap();
        std::fs::write(root.join("b.txt"), &dirty).unwrap();
        // Partly stage both from the outside.
        git_tui_core::stage::stage_hunk(&repo, "a.txt", 0).unwrap();
        git_tui_core::stage::stage_hunk(&repo, "b.txt", 0).unwrap();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(root).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.files.len() == 2);
        fx.app.on_key(KeyCode::Char('S'));
        assert_eq!(fx.app.diff_target(), Some(("a.txt".into(), true)));
        fx.app.on_key(KeyCode::Char('j'));
        assert_eq!(
            fx.app.diff_target(),
            Some(("b.txt".into(), false)),
            "the choice must not follow the cursor to another file"
        );
    }

    #[test]
    fn shift_l_asks_for_more_history_only_when_there_may_be_more() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('3'));
        let deadline = Instant::now() + Duration::from_secs(10);
        while fx.app.log().is_none() {
            fx.app.poll();
            assert!(Instant::now() < deadline, "log never loaded");
            std::thread::sleep(Duration::from_millis(10));
        }
        // One commit in the fixture: the limit was not reached.
        fx.app.on_key(KeyCode::Char('L'));
        assert!(fx.app.notice().unwrap().contains("that is all of it"));
        assert_eq!(fx.app.log_limit, App::LOG_LIMIT);
    }

    #[test]
    fn pushing_from_a_detached_head_is_refused_with_a_reason() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "one\n", "first");
        let first = repo.head().unwrap().target().unwrap();
        commit_file(&repo, "a.txt", "two\n", "second");
        repo.set_head_detached(first).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let root = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(root).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.branch.starts_with('('));
        fx.app.on_key(KeyCode::Char('P'));
        assert_eq!(fx.app.mode(), Mode::Normal, "no upstream prompt");
        assert!(fx.app.error().unwrap().contains("not on a branch"));
    }

    // ---- undo, fetch, amend, commit view, operations ----

    fn wait_until(app: &mut App, what: &str, mut done: impl FnMut(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            app.poll();
            if done(app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn u_puts_back_the_last_discard() {
        let mut fx = harness(&["a.txt"]);
        let file = fx._dir.path().join("a.txt");
        let dirty = std::fs::read_to_string(&file).unwrap();
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Enter);
        wait_for(&mut fx.app, |st| st.files.is_empty());
        assert_ne!(std::fs::read_to_string(&file).unwrap(), dirty);
        assert!(
            fx.app.notice().unwrap_or("").contains("u puts it back"),
            "the way back is announced: {:?}",
            fx.app.notice()
        );
        fx.app.on_key(KeyCode::Char('u'));
        wait_for(&mut fx.app, |st| st.files.iter().any(|e| e.path == "a.txt"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), dirty);
        // Spent: a second undo says so instead of doing something else.
        fx.app.on_key(KeyCode::Char('u'));
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("nothing to undo"), "got: {err}");
    }

    #[test]
    fn discarding_a_folder_is_one_undo() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "src/a.rs", "a\n", "init");
        commit_file(&repo, "src/b.rs", "b\n", "init");
        let root = repo.workdir().unwrap().to_path_buf();
        std::fs::write(root.join("src/a.rs"), "a2\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "b2\n").unwrap();
        std::fs::write(root.join("src/new.rs"), "new\n").unwrap();
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(&root).unwrap()),
        };
        wait_for(&mut fx.app, |st| st.files.len() == 3);
        // The cursor starts on the open `src/` header.
        fx.app.on_key(KeyCode::Char('g'));
        fx.app.on_key(KeyCode::Char('d'));
        let prompt = fx.app.confirm_prompt().unwrap().to_string();
        assert!(prompt.contains("3 files under src/"), "{prompt}");
        assert!(prompt.contains("1 untracked will be deleted"), "{prompt}");
        fx.app.on_key(KeyCode::Enter);
        wait_for(&mut fx.app, |st| st.files.is_empty());
        assert!(!root.join("src/new.rs").exists());
        fx.app.on_key(KeyCode::Char('u'));
        wait_for(&mut fx.app, |st| st.files.len() == 3);
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "a2\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/new.rs")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn f_fetches_and_reports_when_there_is_no_remote() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('f'));
        assert_eq!(fx.app.syncing(), Some("fetching…"));
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("no remote"), "got: {err}");
        assert!(
            fx.app.syncing().is_none(),
            "the label must clear on failure"
        );
    }

    #[test]
    fn amend_opens_the_box_with_the_last_message_and_rewrites_it() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('?'));
        type_into_menu(&mut fx.app, "amend");
        fx.app.on_key(KeyCode::Enter);
        wait_until(&mut fx.app, "the amend box", |app| {
            app.mode() == Mode::Committing
        });
        assert!(fx.app.is_amending());
        assert_eq!(fx.app.draft(), "init");
        fx.app.on_chord(Chord::Ctrl('u'));
        fx.app.on_paste("init, reworded");
        fx.app.on_key(KeyCode::Enter);
        fx.app.on_key(KeyCode::Char('3'));
        wait_until(&mut fx.app, "the reworded commit", |app| {
            app.log()
                .is_some_and(|log| log.first().is_some_and(|c| c.summary == "init, reworded"))
        });
        assert_eq!(fx.app.log().unwrap().len(), 1, "amended, not added");
        // The next `c` is an ordinary, empty commit box again.
        fx.app.on_key(KeyCode::Char('1'));
        fx.app.on_key(KeyCode::Char('c'));
        assert!(!fx.app.is_amending());
        assert_eq!(fx.app.draft(), "");
    }

    #[test]
    fn esc_in_the_amend_box_does_not_leak_the_old_message_into_the_next_commit() {
        let mut fx = harness(&["a.txt"]);
        fx.app.on_key(KeyCode::Char('?'));
        type_into_menu(&mut fx.app, "amend");
        fx.app.on_key(KeyCode::Enter);
        wait_until(&mut fx.app, "the amend box", |app| {
            app.mode() == Mode::Committing
        });
        fx.app.on_key(KeyCode::Esc);
        fx.app.on_key(KeyCode::Char('c'));
        assert_eq!(fx.app.draft(), "");
        assert!(!fx.app.is_amending());
    }

    #[test]
    fn enter_on_a_commit_opens_its_files_read_only() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "one\n", "first");
        // One commit touching two files.
        let root = repo.workdir().unwrap().to_path_buf();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("b.txt"), "bee\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        index.add_path(std::path::Path::new("b.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();
        drop(tree);
        drop(parent);
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(&root).unwrap()),
        };
        fx.app.on_key(KeyCode::Char('3'));
        wait_until(&mut fx.app, "the commit overview", |app| {
            app.commit_overview
                .as_ref()
                .is_some_and(|o| o.files.len() == 2)
        });
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        let view = fx.app.commit_view().unwrap().clone();
        assert_eq!(view.files, vec!["a.txt".to_string(), "b.txt".to_string()]);
        wait_until(&mut fx.app, "the commit's diff", |app| app.diff().is_some());
        assert_eq!(fx.app.diff().unwrap().path, "a.txt");
        // The working tree is clean: this can only be the commit's diff.
        assert_eq!(fx.app.diff().unwrap().hunks.len(), 1);
        // Staging and discarding do nothing here.
        fx.app.on_key(KeyCode::Char(' '));
        fx.app.on_key(KeyCode::Char('d'));
        fx.app.on_key(KeyCode::Char('x'));
        assert_eq!(fx.app.mode(), Mode::FullDiff);
        assert!(fx.app.notice().unwrap().contains("read-only"));
        assert!(!fx.app.mutation_in_flight());
        // `]` goes to the commit's next file.
        fx.app.on_key(KeyCode::Char(']'));
        wait_until(&mut fx.app, "the second file", |app| {
            app.diff().is_some_and(|d| d.path == "b.txt")
        });
        fx.app.on_key(KeyCode::Char(']'));
        assert_eq!(fx.app.notice(), Some("last file"));
        // Esc returns to the commits panel and the working tree's view.
        fx.app.on_key(KeyCode::Esc);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert!(fx.app.commit_view().is_none());
        assert_eq!(fx.app.focus(), Focus::Log);
    }

    #[test]
    fn force_push_needs_an_upstream_and_asks_first() {
        let mut fx = harness(&["a.txt"]);
        wait_for_sync(&mut fx.app);
        fx.app.request_danger(Danger::ForcePush);
        assert_eq!(fx.app.mode(), Mode::Normal);
        assert!(fx.app.error().unwrap().contains("no upstream"));

        let (_dir, _origin, mut app) = sync_harness();
        wait_for_sync(&mut app);
        app.request_danger(Danger::ForcePush);
        // The sync harness has a remote but no upstream yet either.
        assert!(app.error().unwrap().contains("no upstream"));
    }

    #[test]
    fn a_merge_in_progress_is_shown_and_can_be_aborted() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "base\n", "init");
        let root = repo.workdir().unwrap().to_path_buf();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stderr).to_string()
        };
        git(&["checkout", "-q", "-b", "other"]);
        std::fs::write(root.join("a.txt"), "theirs\n").unwrap();
        git(&["commit", "-q", "-am", "theirs"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(root.join("a.txt"), "mine\n").unwrap();
        git(&["commit", "-q", "-am", "mine"]);
        git(&["merge", "other"]);
        drop(repo);
        let mut fx = Fixture {
            _dir: dir,
            app: App::new(JobQueue::spawn(&root).unwrap()),
        };
        wait_until(&mut fx.app, "the merge state", |app| {
            app.operation_in_progress().is_some()
        });
        assert_eq!(fx.app.operation_in_progress(), Some("Merge in progress"));
        wait_for(&mut fx.app, |st| {
            st.files.iter().any(|e| e.state == FileState::Conflicted)
        });
        // Space refuses while the markers are still there.
        fx.app.on_key(KeyCode::Char(' '));
        let err = wait_for_error(&mut fx.app);
        assert!(err.contains("conflict markers"), "got: {err}");
        // The menu leads with continue / abort; abort asks first.
        fx.app.on_key(KeyCode::Char('?'));
        assert_eq!(fx.app.menu_matches()[1].action, MenuAction::AbortOperation);
        fx.app.on_key(KeyCode::Down);
        fx.app.on_key(KeyCode::Enter);
        assert_eq!(fx.app.mode(), Mode::Confirm);
        assert!(fx.app.confirm_prompt().unwrap().contains("abort it?"));
        fx.app.on_key(KeyCode::Enter);
        wait_until(&mut fx.app, "the abort", |app| {
            app.operation_in_progress().is_none()
        });
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "mine\n"
        );
    }
}
