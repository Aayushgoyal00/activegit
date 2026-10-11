//! First-run welcome screen.
//!
//! Shown once, on the first interactive launch with no explicit repo paths.
//! It introduces activegit's features and keybindings. Dismissing it writes
//! a versioned marker file, so it never nags again (bump [`WELCOME_VERSION`]
//! to re-show it after major changes).
//!
//! Skipped when activegit is opened with an explicit repo (e.g. from a
//! wrapper script), when stdio is not a TTY (piped/automated), when
//! `ACTIVEGIT_NO_WELCOME` is set, or with `--no-welcome`.

use std::path::{Path, PathBuf};

/// Bump to re-show the welcome screen after major changes.
pub const WELCOME_VERSION: u32 = 1;

/// Marker file name under the app config dir.
pub const WELCOME_MARKER: &str = "welcome_seen";

/// Env var that suppresses the welcome screen (wrappers / automation).
pub const NO_WELCOME_ENV: &str = "ACTIVEGIT_NO_WELCOME";

/// Keybindings introduced by the welcome screen: (keys, action).
/// Keep each row short: the modal renders them verbatim.
pub const WELCOME_KEYS: &[(&str, &str)] = &[
    ("j / k", "move in the file tree"),
    ("enter", "fullscreen side-by-side diff"),
    ("space", "stage file, folder, or hunk"),
    ("c", "commit"),
    ("Ctrl+G", "AI commit message (in the commit box)"),
    ("p / P", "pull / push"),
    ("/", "fuzzy-find a file"),
    ("o", "open a project"),
    ("q / Q", "close project / quit app"),
];

/// Features introduced by the welcome screen.
pub const WELCOME_FEATURES: &[&str] = &[
    "Status, inline diff, stage, and commit without leaving the terminal.",
    "Fullscreen side-by-side diffs with word-level change marks.",
    "Stage whole files, folders, or single hunks; discard with d.",
    "AI Conventional Commits from your staged diff (Ctrl+G).",
    "Several repos as tabs in one window; the session is restored.",
];

/// Where the marker file lives. `None` when no config dir is known.
pub fn marker_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join(WELCOME_MARKER))
}

/// Whether the marker at `path` records a seen welcome at the current
/// version. Missing/unparseable files mean "not seen".
pub fn welcome_seen_at(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .is_some_and(|seen| seen >= WELCOME_VERSION)
}

/// Record the welcome as seen at `path`, creating parent dirs.
/// Errors are swallowed: persistence must never break the UI.
pub fn mark_welcome_seen_at(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, WELCOME_VERSION.to_string());
}

/// Whether the welcome was already seen (default path).
pub fn welcome_seen() -> bool {
    marker_path().is_some_and(|p| welcome_seen_at(&p))
}

/// Record the welcome as seen (default path). No-op without a known path.
pub fn mark_welcome_seen() {
    if let Some(path) = marker_path() {
        mark_welcome_seen_at(&path);
    }
}

/// Pure decision logic (testable without touching env or disk).
pub fn should_show_welcome(
    has_explicit_paths: bool,
    already_seen: bool,
    env_suppress: bool,
    no_welcome_flag: bool,
    is_tty: bool,
) -> bool {
    if already_seen || env_suppress || no_welcome_flag || !is_tty {
        return false;
    }
    // Explicit repo paths mean a targeted (possibly scripted) invocation:
    // skip the intro there and go straight to work.
    if has_explicit_paths {
        return false;
    }
    true
}

/// Runtime decision: reads the marker file, env, and TTY state.
pub fn should_show_welcome_runtime(has_explicit_paths: bool, no_welcome_flag: bool) -> bool {
    let env_suppress = std::env::var_os(NO_WELCOME_ENV).is_some_and(|v| !v.is_empty());
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    should_show_welcome(
        has_explicit_paths,
        welcome_seen(),
        env_suppress,
        no_welcome_flag,
        is_tty,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_on_first_interactive_launch() {
        assert!(should_show_welcome(false, false, false, false, true));
    }

    #[test]
    fn hidden_once_seen() {
        assert!(!should_show_welcome(false, true, false, false, true));
    }

    #[test]
    fn hidden_with_explicit_repo_paths() {
        assert!(!should_show_welcome(true, false, false, false, true));
    }

    #[test]
    fn hidden_when_piped_or_suppressed() {
        // Wrapper / piped: stdout is not a TTY.
        assert!(!should_show_welcome(false, false, false, false, false));
        // Env suppression for wrappers and automation.
        assert!(!should_show_welcome(false, false, true, false, true));
        // Explicit opt-out flag.
        assert!(!should_show_welcome(false, false, false, true, true));
    }

    #[test]
    fn marker_roundtrip_at_explicit_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("welcome_seen");
        assert!(!welcome_seen_at(&path));
        mark_welcome_seen_at(&path);
        assert!(welcome_seen_at(&path));
    }

    #[test]
    fn corrupt_marker_means_not_seen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("welcome_seen");
        std::fs::write(&path, "hello").unwrap();
        assert!(!welcome_seen_at(&path));
    }

    #[test]
    fn welcome_content_mentions_activegit() {
        assert!(!WELCOME_KEYS.is_empty());
        assert!(!WELCOME_FEATURES.is_empty());
        // Every key row must fit the modal without wrapping.
        for (keys, action) in WELCOME_KEYS {
            assert!(keys.len() + action.len() + 4 < 70, "{keys} / {action}");
        }
    }
}
