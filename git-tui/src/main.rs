mod app;
mod config;
mod fuzzy;
mod input;
mod markdown;
mod menu;
mod session;
mod shortcut;
mod start;
mod syntax;
mod ui;
mod welcome;
mod words;
mod workspace;

use anyhow::{Context, Result};
use config::{Config, Theme};
use crossterm::event::KeyCode;
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use git_tui_core::repo::Repo;
use input::Input;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use session::Session;
use std::ffi::OsString;
use std::io::{stdout, IsTerminal};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use workspace::Workspace;

fn main() -> Result<()> {
    let cli = parse_args(std::env::args_os().skip(1))?;
    if cli.shortcut {
        return shortcut::install();
    }
    let hold = cli.hold;
    let result = start(cli);
    if let (true, Err(e)) = (hold, &result) {
        // Started from a shortcut: the window closes with the process, so
        // the message would be gone before it could be read.
        eprintln!("activegit: {e:#}");
        eprintln!();
        eprintln!("Press Enter to close.");
        let _ = std::io::stdin().read_line(&mut String::new());
        std::process::exit(1);
    }
    result
}

fn start(cli: Cli) -> Result<()> {
    // Stage 5 — raw mode, explained:
    // A terminal normally line-buffers input and echoes it (cooked mode):
    // your program only sees a line after Enter. A TUI needs every keypress
    // immediately and without echo, so it enables raw mode. The terminal is
    // global state: if we exit (or panic) without restoring it, the user's
    // shell is left broken (no echo, no line editing). Hence the panic hook
    // below + the explicit restore after `run` returns.
    install_panic_hook();

    // Missing/invalid config fails fast here (path + value in the error);
    // missing file falls back to defaults inside `Config::load`.
    // Precedence: `--theme` flag > config file > default.
    // Positional paths and `--repo` flags select the projects; empty means
    // restore the last session, falling back to the current directory.
    // Each path is resolved to its enclosing repo.
    let mut config = Config::load().context("cannot load config")?;
    if let Some(name) = cli.theme {
        config.theme = Theme::by_name(&name).with_context(|| format!("unknown theme {name:?}"))?;
    }
    let has_explicit_paths = !cli.paths.is_empty();
    let no_welcome_flag = cli.no_welcome;
    let interactive = std::io::stdin().is_terminal();
    let (mut paths, mut restored_current) = resolve_startup_paths(cli.paths);
    // `--pick` (the shortcut): choose the project first instead of landing
    // in whichever one was open last.
    if cli.pick && !has_explicit_paths && interactive {
        match pick_project(&config, paths.clone())? {
            Some(root) => {
                let index = project_index(&mut paths, root);
                restored_current = Some(index);
            }
            None => return Ok(()),
        }
    }
    let mut workspace = match Workspace::open(paths, config.clone()) {
        Ok(workspace) => workspace,
        // Started somewhere that is not a repository with nothing to
        // restore: ask instead of failing.
        Err(_) if !has_explicit_paths && interactive => match pick_project(&config, Vec::new())? {
            Some(root) => Workspace::open(vec![root], config.clone())?,
            None => return Ok(()),
        },
        Err(e) => return Err(e),
    };
    workspace.set_session_path(Session::default_path());
    // First-run welcome overlay: only when launched bare (no explicit
    // paths), interactive, and never seen before. Wrapper invocations
    // pass an explicit repo, pipes are not TTYs — both skip it.
    workspace.set_show_welcome(welcome::should_show_welcome_runtime(
        has_explicit_paths,
        no_welcome_flag,
    ));
    if let Some(idx) = restored_current {
        workspace.set_current(idx);
    } else {
        // Explicit CLI paths (or fresh cwd): become the new session.
        workspace.save_session();
    }

    let mut terminal = enter_terminal(workspace.mouse_enabled())?;

    let res = run(&mut terminal, &mut workspace);

    restore_terminal(&mut terminal);
    // Persist the tabs that were open so the next launch restores them.
    workspace.save_session();
    res
}

/// Take over the terminal: raw mode (every key at once, no echo), the
/// alternate screen (the shell's contents come back on exit), and the
/// optional extras. Undone by [`restore_terminal`].
fn enter_terminal(mouse: bool) -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    enable_raw_mode().context("cannot enable raw mode")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).context("cannot enter alternate screen")?;
    // `[ui] mouse = false` leaves the mouse to the terminal (native text
    // selection); everything stays reachable from the keyboard.
    if mouse {
        let _ = execute!(out, EnableMouseCapture);
    }
    // Best effort: older Windows consoles support neither. Pastes then
    // arrive as key presses and are caught by `input::paste_text`.
    let _ = execute!(out, EnableBracketedPaste, EnableFocusChange);
    // The window or tab says what is running in it, not the shell's name.
    let _ = execute!(out, SetTitle("ActiveGit"));
    Terminal::new(CrosstermBackend::new(out)).context("cannot create terminal")
}

/// Show the start screen; `None` means the user quit from it.
fn pick_project(config: &Config, recents: Vec<PathBuf>) -> Result<Option<PathBuf>> {
    let mut terminal = enter_terminal(config.ui.mouse)?;
    let choice = start::choose(&mut terminal, config.theme, recents);
    restore_terminal(&mut terminal);
    Ok(match choice? {
        start::Choice::Open(root) => Some(root),
        start::Choice::Quit => None,
    })
}

/// Where `root` sits among the projects to open, adding it when new.
fn project_index(paths: &mut Vec<PathBuf>, root: PathBuf) -> usize {
    let canon = |p: &PathBuf| dunce::canonicalize(p).unwrap_or_else(|_| p.clone());
    let wanted = canon(&root);
    match paths.iter().position(|p| canon(p) == wanted) {
        Some(index) => index,
        None => {
            paths.push(root);
            paths.len() - 1
        }
    }
}

/// Decide which projects to open at startup.
/// Explicit CLI paths always win (and become the new session). With no CLI
/// paths, the last saved session is restored (dead entries dropped); an
/// empty/missing session falls back to `Workspace::open`'s cwd default
/// (empty vec). Returns the paths plus the session's selected tab, if any.
fn resolve_startup_paths(explicit: Vec<PathBuf>) -> (Vec<PathBuf>, Option<usize>) {
    if !explicit.is_empty() {
        return (explicit, None);
    }
    resolve_from_session(&Session::load())
}

/// Pure core of [`resolve_startup_paths`] (testable without touching the
/// real session file): explicit paths win; otherwise restore the session's
/// still-valid projects; empty/invalid sessions fall back to cwd (empty vec).
fn resolve_from_session(session: &Session) -> (Vec<PathBuf>, Option<usize>) {
    if session.projects.is_empty() {
        return (Vec::new(), None);
    }
    let valid: Vec<PathBuf> = session
        .existing_projects()
        .into_iter()
        .filter(|p| Repo::discover_root(p).is_ok())
        .collect();
    if valid.is_empty() {
        return (Vec::new(), None);
    }
    let current = session.current.min(valid.len() - 1);
    (valid, Some(current))
}

/// Panic-hook pattern (ratatui docs): a panicking TUI must leave cooked mode
/// and the alternate screen *before* the default hook prints the panic,
/// otherwise the panic message itself is unreadable and the shell stays raw.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout(),
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        original(info);
    }));
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableFocusChange,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();
}

/// Map a crossterm mouse event to a workspace action: left-button
/// press/drag/release, the wheel, and right-press (context menu). Hover
/// moves and the middle button are ignored.
fn map_mouse(kind: MouseEventKind, col: u16, row: u16) -> Option<workspace::MouseAction> {
    use workspace::MouseAction;
    match kind {
        MouseEventKind::Down(MouseButton::Left) => Some(MouseAction::Down(col, row)),
        MouseEventKind::Drag(MouseButton::Left) => Some(MouseAction::Drag(col, row)),
        MouseEventKind::Up(MouseButton::Left) => Some(MouseAction::Up),
        MouseEventKind::ScrollUp => Some(MouseAction::ScrollUp(col, row)),
        MouseEventKind::ScrollDown => Some(MouseAction::ScrollDown(col, row)),
        MouseEventKind::Down(MouseButton::Right) => Some(MouseAction::RightDown(col, row)),
        _ => None,
    }
}

struct Cli {
    theme: Option<String>,
    paths: Vec<PathBuf>,
    no_welcome: bool,
    /// `--shortcut`: create Start-menu / desktop shortcuts and exit.
    shortcut: bool,
    /// `--hold`: wait for Enter before closing after a startup error (the
    /// shortcut passes it; its window would vanish with the message).
    hold: bool,
    /// `--pick`: show the start screen and choose a project first.
    pick: bool,
}

const USAGE: &str =
    "usage: activegit [--theme <name>] [--no-welcome] [--repo <path>]... [<path>...] [-- <path>...]
       activegit --pick        choose the project to open on a start screen
       activegit --shortcut    add ActiveGit to the Start menu and desktop (Windows)";

fn parse_args(args: impl IntoIterator<Item = impl Into<OsString>>) -> Result<Cli> {
    let mut cli = Cli {
        theme: None,
        paths: Vec::new(),
        no_welcome: false,
        shortcut: false,
        hold: false,
        pick: false,
    };
    let mut args = args.into_iter().map(Into::into);
    while let Some(arg) = args.next() {
        if arg == "--" {
            cli.paths.extend(args.map(PathBuf::from));
            break;
        } else if arg == "-h" || arg == "--help" {
            println!("{USAGE}");
            let themes: Vec<_> = Theme::names().collect();
            println!("themes: {}", themes.join(", "));
            std::process::exit(0);
        } else if arg == "-V" || arg == "--version" {
            println!("activegit {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        } else if arg == "--no-welcome" {
            cli.no_welcome = true;
        } else if arg == "--shortcut" {
            cli.shortcut = true;
        } else if arg == "--hold" {
            cli.hold = true;
        } else if arg == "--pick" {
            cli.pick = true;
        } else if arg == "--theme" {
            let name = args.next().context("--theme needs a value")?;
            cli.theme = Some(
                name.into_string()
                    .map_err(|_| anyhow::anyhow!("theme must be UTF-8"))?,
            );
        } else if let Some(name) = arg.to_str().and_then(|s| s.strip_prefix("--theme=")) {
            anyhow::ensure!(!name.is_empty(), "--theme needs a value");
            cli.theme = Some(name.to_string());
        } else if arg == "--repo" {
            let path = args.next().context("--repo needs a value")?;
            anyhow::ensure!(
                !path.is_empty() && !path.as_encoded_bytes().starts_with(b"-"),
                "--repo needs a value"
            );
            cli.paths.push(path.into());
        } else if arg.as_encoded_bytes().starts_with(b"--repo=") {
            let path: OsString = arg.to_string_lossy()["--repo=".len()..].into();
            anyhow::ensure!(!path.is_empty(), "--repo needs a value");
            cli.paths.push(path.into());
        } else if arg.as_encoded_bytes().starts_with(b"-") {
            anyhow::bail!("{USAGE} (unexpected argument {arg:?})");
        } else {
            cli.paths.push(arg.into());
        }
    }
    Ok(cli)
}

/// How long to wait for input when nothing is happening.
const IDLE_TICK: Duration = Duration::from_millis(100);
/// How long to wait for input while the worker still owes a result, so the
/// result is drawn as soon as it lands instead of up to a full idle tick
/// later (a stage is three chained jobs: mutate, status, diff).
const BUSY_TICK: Duration = Duration::from_millis(8);

/// Event loop, exactly: input → dispatch → poll → render.
/// Polling (not blocking) on input keeps status refreshes and the render
/// loop live; draining the job queue never blocks either.
///
/// Stage 5 — the render-loop pattern: unlike a request/response CLI (run
/// once, print, exit), a TUI loops forever: `poll` input with a timeout
/// (so a frame still renders with no input) → update state → `draw` the
/// whole screen → repeat. `draw` diffs the previous frame and emits only
/// the changed ANSI sequences.
fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    workspace: &mut Workspace,
) -> Result<()> {
    let mut welcome_was_visible = workspace.welcome_visible();
    let mut last_draw: Option<Instant> = None;
    let mut last_paste: Option<Instant> = None;
    loop {
        let tick = if workspace.busy() {
            BUSY_TICK
        } else {
            IDLE_TICK
        };
        let mut redraw = false;
        if event::poll(tick).context("cannot poll input")? {
            // Drain everything already queued before drawing: a held key
            // then costs one frame per batch, not one frame per repeat.
            let mut batch = Vec::new();
            loop {
                batch.push(event::read().context("cannot read input")?);
                if !event::poll(Duration::ZERO).context("cannot poll input")? {
                    break;
                }
            }
            redraw |= handle_batch(batch, workspace, &mut last_paste);
        }
        redraw |= workspace.poll();
        // Persist the welcome dismissal so the intro shows exactly once.
        if welcome_was_visible && !workspace.welcome_visible() {
            welcome::mark_welcome_seen();
        }
        welcome_was_visible = workspace.welcome_visible();
        // Busy ticks are short; only repaint when something changed or the
        // idle heartbeat is due.
        if redraw || last_draw.is_none_or(|t| t.elapsed() >= IDLE_TICK) {
            terminal
                .draw(|f| ui::render_workspace(f, workspace))
                .context("cannot render")?;
            last_draw = Some(Instant::now());
        }
        if workspace.should_quit() {
            return Ok(());
        }
    }
}

/// A paste that arrives as key presses can span several reads; keys that
/// follow a detected burst this closely are still part of it.
const PASTE_TAIL: Duration = Duration::from_millis(120);

/// Apply everything read in one go; returns whether the screen needs a
/// repaint. A burst of plain keys is a paste (see [`input::paste_text`]) and
/// goes to the focused text box as text instead of being run key by key.
fn handle_batch(
    batch: Vec<Event>,
    workspace: &mut Workspace,
    last_paste: &mut Option<Instant>,
) -> bool {
    let inputs: Vec<Input> = batch
        .iter()
        .filter_map(|event| match event {
            Event::Key(key) => Some(input::classify(key)),
            _ => None,
        })
        .collect();
    let in_tail = last_paste.is_some_and(|t| t.elapsed() < PASTE_TAIL);
    let pasted = match input::paste_text(&inputs) {
        Some(text) => Some(text),
        // The tail of a paste: whatever plain keys it holds are text too.
        None if in_tail => {
            let text: String = inputs
                .iter()
                .filter_map(|input| match input {
                    Input::Key(KeyCode::Char(c), _) => Some(*c),
                    Input::Key(KeyCode::Enter, _) => Some('\n'),
                    _ => None,
                })
                .collect();
            (!text.is_empty()).then_some(text)
        }
        None => None,
    };
    let mut redraw = false;
    if let Some(text) = &pasted {
        *last_paste = Some(Instant::now());
        workspace.on_paste(text);
        redraw = true;
    }
    for event in batch {
        match event {
            // Already delivered as pasted text above.
            Event::Key(_) if pasted.is_some() => {}
            event => redraw |= handle_event(event, workspace),
        }
    }
    redraw
}

/// Apply one terminal event; returns whether the screen needs a repaint.
/// Key releases (Windows reports one per keystroke) are ignored without
/// stalling the loop.
fn handle_event(event: Event, workspace: &mut Workspace) -> bool {
    match event {
        Event::Key(key) => match input::classify(&key) {
            Input::Key(code, shift) => workspace.on_key_with_modifiers(code, shift),
            Input::Chord(chord) => workspace.on_chord(chord),
            // Ctrl-C safety hatch: works even if the user rebinds `quit`
            // away from `Q`, and in text modals where `q` is literal.
            Input::Quit => workspace.request_quit(),
            Input::Ignored => return false,
        },
        Event::Mouse(m) => {
            // Plain pointer motion maps to nothing and needs no repaint.
            match map_mouse(m.kind, m.column, m.row) {
                Some(action) => workspace.on_mouse(action),
                None => return false,
            }
        }
        Event::Paste(text) => workspace.on_paste(&text),
        // Coming back to the terminal: files may have changed meanwhile.
        Event::FocusGained => workspace.on_focus_gained(),
        Event::FocusLost => return false,
        Event::Resize(..) => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_means_no_override() {
        let cli = parse_args(args(&[])).unwrap();
        assert!(cli.theme.is_none());
        assert!(cli.paths.is_empty());
        assert!(!cli.no_welcome);
    }

    #[test]
    fn no_welcome_flag_opts_out() {
        assert!(parse_args(args(&["--no-welcome"])).unwrap().no_welcome);
    }

    #[test]
    fn theme_flag_space_and_equals_forms() {
        assert_eq!(
            parse_args(args(&["--theme", "tokyo-night"])).unwrap().theme,
            Some("tokyo-night".into())
        );
        assert_eq!(
            parse_args(args(&["--theme=default"])).unwrap().theme,
            Some("default".into())
        );
    }

    #[test]
    fn theme_flag_needs_a_value() {
        assert!(parse_args(args(&["--theme"])).is_err());
        assert!(parse_args(args(&["--theme="])).is_err());
    }

    #[test]
    fn positional_repo_paths_are_accepted() {
        assert!(parse_args(args(&["some-repo", "other-repo"])).is_ok());
    }

    #[test]
    fn multiple_repo_flags_and_positionals_are_accepted() {
        assert!(parse_args(args(&[
            "first",
            "--repo",
            "second",
            "--theme=tokyo-night",
            "--repo=third",
            "--repo",
            "fourth",
            "fifth",
        ]))
        .is_ok());
    }

    #[test]
    fn repo_flag_needs_a_value() {
        for input in [
            vec!["--repo"],
            vec!["--repo="],
            vec!["--repo", ""],
            vec!["--repo", "--theme=default"],
            vec!["--repo", "--"],
        ] {
            assert!(parse_args(args(&input)).is_err(), "{input:?}");
        }
    }

    #[test]
    fn unknown_flags_are_rejected() {
        for input in [vec!["--bogus"], vec!["-x"], vec!["repo", "--bogus=value"]] {
            assert!(parse_args(args(&input)).is_err(), "{input:?}");
        }
    }

    #[test]
    fn separator_treats_remaining_arguments_as_paths() {
        assert!(parse_args(args(&[
            "first",
            "--",
            "--repo",
            "--theme=default",
            "--help",
            "--",
            "-last",
        ]))
        .is_ok());
    }

    #[test]
    fn cli_theme_overrides_file_theme() {
        // Precedence CLI > file: file says default, CLI says tokyo-night.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[theme]\nname = \"default\"\n").unwrap();
        let mut config = Config::load_from_path(&path).unwrap();
        let cli = parse_args(args(&["--theme", "tokyo-night"])).unwrap();
        if let Some(name) = cli.theme {
            config.theme = Theme::by_name(&name).unwrap();
        }
        assert_eq!(
            config.theme.border_focused,
            ratatui::style::Color::Rgb(122, 162, 247)
        );
    }

    #[test]
    fn mouse_events_map_to_workspace_actions() {
        use crossterm::event::{MouseButton, MouseEventKind};
        use workspace::MouseAction;
        assert_eq!(
            map_mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
            Some(MouseAction::Down(3, 4))
        );
        assert_eq!(
            map_mouse(MouseEventKind::Drag(MouseButton::Left), 3, 4),
            Some(MouseAction::Drag(3, 4))
        );
        assert_eq!(
            map_mouse(MouseEventKind::Up(MouseButton::Left), 3, 4),
            Some(MouseAction::Up)
        );
        assert_eq!(
            map_mouse(MouseEventKind::ScrollUp, 3, 4),
            Some(MouseAction::ScrollUp(3, 4))
        );
        assert_eq!(
            map_mouse(MouseEventKind::ScrollDown, 3, 4),
            Some(MouseAction::ScrollDown(3, 4))
        );
        // Right button and hover moves stay keyboard-only.
        assert_eq!(
            map_mouse(MouseEventKind::Down(MouseButton::Right), 3, 4),
            Some(MouseAction::RightDown(3, 4))
        );
        assert_eq!(
            map_mouse(MouseEventKind::Down(MouseButton::Middle), 3, 4),
            None
        );
        assert_eq!(map_mouse(MouseEventKind::Moved, 3, 4), None);
    }

    fn init_repo(dir: &tempfile::TempDir) {
        let repo = git2::Repository::init(dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
    }

    #[test]
    fn empty_session_falls_back_to_cwd() {
        let (paths, current) = resolve_from_session(&Session::empty());
        assert!(paths.is_empty());
        assert!(current.is_none());
    }

    #[test]
    fn dead_session_entries_fall_back_to_cwd() {
        let s = Session::new(vec![PathBuf::from("/definitely/not/here-git-tui-xyz")], 0);
        let (paths, current) = resolve_from_session(&s);
        assert!(paths.is_empty());
        assert!(current.is_none());
    }

    #[test]
    fn valid_session_projects_are_restored_with_selection() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        init_repo(&a);
        init_repo(&b);
        let s = Session::new(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            5, // out of range clamps to last tab
        );
        let (paths, current) = resolve_from_session(&s);
        assert_eq!(paths.len(), 2);
        assert_eq!(current, Some(1));
    }

    #[test]
    fn plain_dir_in_session_is_dropped() {
        let plain = tempfile::TempDir::new().unwrap();
        let s = Session::new(vec![plain.path().to_path_buf()], 0);
        let (paths, current) = resolve_from_session(&s);
        assert!(paths.is_empty());
        assert!(current.is_none());
    }

    #[test]
    fn shortcut_and_hold_flags_parse() {
        let cli = parse_args(args(&["--shortcut"])).unwrap();
        assert!(cli.shortcut && !cli.hold);
        let cli = parse_args(args(&["--hold"])).unwrap();
        assert!(cli.hold && !cli.shortcut && cli.paths.is_empty());
        assert!(!parse_args(args(&[])).unwrap().hold);
        let cli = parse_args(args(&["--hold", "--pick"])).unwrap();
        assert!(cli.hold && cli.pick);
        assert!(!parse_args(args(&["--hold"])).unwrap().pick);
    }

    #[test]
    fn picked_project_joins_the_session_or_selects_its_tab() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        let mut paths = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        assert_eq!(project_index(&mut paths, b.path().to_path_buf()), 1);
        assert_eq!(paths.len(), 2);
        let c = tempfile::TempDir::new().unwrap();
        assert_eq!(project_index(&mut paths, c.path().to_path_buf()), 2);
        assert_eq!(paths[2], c.path());
    }
}
