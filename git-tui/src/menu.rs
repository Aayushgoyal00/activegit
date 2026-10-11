//! Actions menu (`?`): every action in one searchable list, with its key.
//!
//! The menu is a second way to reach what the keys already do, so each
//! entry carries the binding it mirrors (for the hint column) and whether
//! it destroys work (those ask for a second Enter before running).

use crate::app::Focus;
use crate::config::KeyBindings;
use crate::ui::FooterAction;
use crossterm::event::KeyCode;

/// What a menu entry does. Panel actions reuse [`FooterAction`] so the
/// menu, the button bar, and the keys share one implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuAction {
    Footer(FooterAction),
    Refresh,
    Help,
    MarkdownPreview,
    LlmSettings,
    Focus(Focus),
    Workspace(WorkspaceRequest),
    Quit,
}

/// Actions only the workspace can perform (it owns the project tabs).
/// The app records the request; the workspace picks it up after the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceRequest {
    Open,
    Next,
    Prev,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MenuEntry {
    pub group: &'static str,
    pub label: &'static str,
    pub action: MenuAction,
    /// Throws away work: the menu asks for a second Enter first.
    pub destructive: bool,
}

/// Where the menu was opened from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuContext {
    /// The main view, with this panel focused.
    Panel(Focus),
    /// The fullscreen diff.
    FullDiff,
}

/// Entries for where the menu was opened. Actions on "the selected" file,
/// hunk, branch or stash only appear while their panel is focused, so the
/// thing they act on is the one highlighted behind the menu.
pub(crate) fn entries(context: MenuContext) -> Vec<MenuEntry> {
    use FooterAction as F;
    use MenuAction as M;
    let focus = match context {
        MenuContext::Panel(focus) => focus,
        MenuContext::FullDiff => return fullscreen_entries(),
    };
    let entry = |group, label, action| MenuEntry {
        group,
        label,
        action,
        destructive: false,
    };
    let danger = |group, label, action| MenuEntry {
        group,
        label,
        action,
        destructive: true,
    };
    let mut out = Vec::new();
    match focus {
        Focus::Status => out.extend([
            entry("Files", "Stage / unstage selected", M::Footer(F::Stage)),
            entry("Files", "Open full diff", M::Footer(F::OpenDiff)),
            danger("Files", "Discard changes", M::Footer(F::Discard)),
        ]),
        Focus::Diff => out.extend([
            entry("Diff", "Stage hunk", M::Footer(F::StageHunk)),
            entry("Diff", "Copy selection", M::Footer(F::Copy)),
            entry("Diff", "Open full diff", M::Footer(F::OpenDiff)),
            danger("Diff", "Restore hunk", M::Footer(F::RestoreHunk)),
        ]),
        Focus::Branches | Focus::Log | Focus::Stash => {}
    }
    out.extend([
        entry("Files", "Commit", M::Footer(F::Commit)),
        entry("Files", "Find file", M::Footer(F::Find)),
        entry("Files", "Toggle Markdown preview", M::MarkdownPreview),
        entry("Files", "Refresh", M::Refresh),
    ]);
    if focus == Focus::Branches {
        out.push(entry(
            "Branches",
            "Checkout selected branch",
            M::Footer(F::Checkout),
        ));
    }
    out.push(entry("Branches", "New branch", M::Footer(F::NewBranch)));
    if focus == Focus::Branches {
        out.push(danger(
            "Branches",
            "Delete selected branch",
            M::Footer(F::DeleteBranch),
        ));
    }
    if focus == Focus::Stash {
        out.push(entry("Stash", "Pop selected stash", M::Footer(F::StashPop)));
    }
    out.push(entry("Stash", "Stash changes", M::Footer(F::StashPush)));
    if focus == Focus::Stash {
        out.push(danger(
            "Stash",
            "Drop selected stash",
            M::Footer(F::StashDrop),
        ));
    }
    out.extend([
        entry("Sync", "Pull", M::Footer(F::Pull)),
        entry("Sync", "Push", M::Footer(F::Push)),
        entry("Go to", "Files", M::Focus(Focus::Status)),
        entry("Go to", "Branches", M::Focus(Focus::Branches)),
        entry("Go to", "Commits", M::Focus(Focus::Log)),
        entry("Go to", "Stash", M::Focus(Focus::Stash)),
        entry("Go to", "Diff preview", M::Focus(Focus::Diff)),
        entry(
            "Projects",
            "Open project",
            M::Workspace(WorkspaceRequest::Open),
        ),
        entry(
            "Projects",
            "Next project",
            M::Workspace(WorkspaceRequest::Next),
        ),
        entry(
            "Projects",
            "Previous project",
            M::Workspace(WorkspaceRequest::Prev),
        ),
        entry(
            "Projects",
            "Close project",
            M::Workspace(WorkspaceRequest::Close),
        ),
        entry("Settings", "Theme", M::Footer(F::Theme)),
        entry("Settings", "LLM setup", M::LlmSettings),
        entry("Settings", "Help: all keys", M::Help),
        entry("Settings", "Quit", M::Quit),
    ]);
    out
}

/// What can be done from inside the fullscreen diff.
fn fullscreen_entries() -> Vec<MenuEntry> {
    use FooterAction as F;
    use MenuAction as M;
    let entry = |group, label, action, destructive| MenuEntry {
        group,
        label,
        action,
        destructive,
    };
    vec![
        entry("Diff", "Stage hunk", M::Footer(F::StageHunk), false),
        entry("Diff", "Copy selection", M::Footer(F::Copy), false),
        entry("Diff", "Restore hunk", M::Footer(F::RestoreHunk), true),
        entry("Diff", "Discard file changes", M::Footer(F::Discard), true),
        entry("Diff", "Close diff", M::Footer(F::CloseDiff), false),
        entry("Files", "Find file", M::Footer(F::Find), false),
        entry(
            "Files",
            "Toggle Markdown preview",
            M::MarkdownPreview,
            false,
        ),
        entry("Sync", "Pull", M::Footer(F::Pull), false),
        entry("Sync", "Push", M::Footer(F::Push), false),
        entry("Settings", "Help: all keys", M::Help, false),
        entry("Settings", "Quit", M::Quit, false),
    ]
}

/// The key that does the same thing, for the hint column. Follows the
/// user's bindings; empty when the action has no key.
pub(crate) fn key_hint(action: MenuAction, keys: &KeyBindings) -> String {
    use FooterAction as F;
    use MenuAction as M;
    let bound: &[KeyCode] = match action {
        M::Footer(F::Stage | F::StageHunk) => &keys.stage,
        M::Footer(F::Discard) => &keys.discard,
        M::Footer(F::Commit) => &keys.commit,
        M::Footer(F::Pull) => &keys.sync_pull,
        M::Footer(F::Push) => &keys.sync_push,
        M::Footer(F::Find) => &keys.find_files,
        M::Footer(F::Theme) => &keys.theme_picker,
        M::Footer(F::OpenDiff) => &[KeyCode::Enter],
        M::Footer(F::CloseDiff) => &[KeyCode::Esc],
        M::Footer(F::RestoreHunk) => &[KeyCode::Char('x')],
        M::Footer(F::Checkout) => &keys.checkout,
        M::Footer(F::NewBranch) => &keys.branch_new,
        M::Footer(F::DeleteBranch) => &keys.branch_delete,
        M::Footer(F::StashPop) => &keys.stash_pop,
        M::Footer(F::StashPush) => &keys.stash_push,
        M::Footer(F::StashDrop) => &keys.stash_drop,
        M::Footer(F::Menu) => &keys.action_menu,
        M::Footer(F::Copy) => &[KeyCode::Char('y')],
        M::Refresh => &keys.refresh,
        M::Help => &[KeyCode::F(1)],
        M::MarkdownPreview => &keys.toggle_markdown_preview,
        M::LlmSettings => &keys.llm_settings,
        M::Focus(Focus::Status) => &keys.focus_status,
        M::Focus(Focus::Branches) => &keys.focus_branches,
        M::Focus(Focus::Log) => &keys.focus_log,
        M::Focus(Focus::Stash) => &keys.focus_stash,
        M::Focus(Focus::Diff) => &keys.focus_diff,
        M::Workspace(WorkspaceRequest::Open) => &keys.project_open,
        M::Workspace(WorkspaceRequest::Next) => &keys.project_next,
        M::Workspace(WorkspaceRequest::Prev) => &keys.project_prev,
        M::Workspace(WorkspaceRequest::Close) => &keys.project_close,
        M::Quit => &keys.quit,
    };
    bound.first().map(|k| key_label(*k)).unwrap_or_default()
}

/// Every key the app answers to, grouped, for the help screen (`F1`). Bound
/// actions show the user's own binding; the fixed keys are listed as they
/// are.
pub(crate) fn help_sections(
    keys: &KeyBindings,
) -> Vec<(&'static str, Vec<(String, &'static str)>)> {
    let bound = |codes: &[KeyCode]| {
        codes
            .iter()
            .map(|k| key_label(*k))
            .filter(|label| !label.is_empty())
            .collect::<Vec<_>>()
            .join(" / ")
    };
    let fixed = |text: &str| text.to_string();
    vec![
        (
            "Everywhere",
            vec![
                (
                    bound(&keys.action_menu),
                    "actions menu (everything, searchable)",
                ),
                (fixed("F1"), "this help"),
                (
                    format!(
                        "{} {} {} {} {}",
                        bound(&keys.focus_status[..1.min(keys.focus_status.len())]),
                        bound(&keys.focus_branches),
                        bound(&keys.focus_log),
                        bound(&keys.focus_stash),
                        bound(&keys.focus_diff)
                    ),
                    "go to files / branches / commits / stash / diff",
                ),
                (
                    format!("{} / shift+tab", bound(&keys.focus_next)),
                    "next / previous panel",
                ),
                (bound(&keys.find_files), "find a file"),
                (bound(&keys.commit), "commit box"),
                (bound(&keys.sync_pull), "pull"),
                (bound(&keys.sync_push), "push"),
                (bound(&keys.sync_fetch), "fetch"),
                (
                    format!("{} / ctrl+l", bound(&keys.refresh)),
                    "refresh everything",
                ),
                (bound(&keys.undo), "undo the last discard"),
                (bound(&keys.theme_picker), "theme picker"),
                (bound(&keys.llm_settings), "LLM setup"),
                (bound(&keys.project_open), "open another project"),
                (
                    format!(
                        "{} {}",
                        bound(&keys.project_prev),
                        bound(&keys.project_next)
                    ),
                    "previous / next project tab",
                ),
                (
                    bound(&keys.project_close),
                    "close project (in a diff: close the diff)",
                ),
                (format!("{} / ctrl+c", bound(&keys.quit)), "quit"),
            ],
        ),
        (
            "Lists (files, branches, commits, stash)",
            vec![
                (
                    format!("{} {}", bound(&keys.nav_down), bound(&keys.nav_up)),
                    "down / up (ctrl+n / ctrl+p too)",
                ),
                (fixed("g / G"), "first / last"),
            ],
        ),
        (
            "Files",
            vec![
                (bound(&keys.stage), "stage or unstage the file or folder"),
                (bound(&keys.discard), "discard changes (asks first)"),
                (fixed("left / right"), "fold / unfold the folder"),
                (fixed("enter"), "open the diff fullscreen"),
                (
                    bound(&keys.toggle_markdown_preview),
                    "rendered Markdown preview",
                ),
                (
                    format!("{} {}", bound(&keys.scroll_up), bound(&keys.scroll_down)),
                    "page the diff preview",
                ),
            ],
        ),
        (
            "Diff (preview with focus, and fullscreen)",
            vec![
                (fixed("j / k"), "line down / up"),
                (fixed("J / K"), "next / previous hunk (shift+down / up)"),
                (fixed("g / G"), "top / bottom"),
                (fixed("ctrl+d / ctrl+u"), "half a page down / up"),
                (
                    format!("{} {}", bound(&keys.scroll_up), bound(&keys.scroll_down)),
                    "page up / down",
                ),
                (
                    fixed("h / l / 0 / end"),
                    "column left / right / start / end",
                ),
                (fixed("ctrl+f"), "search in this diff"),
                (fixed("n / N"), "next / previous match"),
                (fixed("v / V"), "select characters / lines"),
                (fixed("y"), "copy the selection"),
                (bound(&keys.stage), "stage the hunk under the cursor"),
                (fixed("x"), "restore the hunk (asks first)"),
                (fixed("s"), "staged / unstaged side of a partly staged file"),
            ],
        ),
        (
            "Fullscreen diff only",
            vec![
                (fixed("] / ["), "next / previous file"),
                (
                    bound(&keys.discard),
                    "discard the file's changes (asks first)",
                ),
                (fixed("esc"), "close the diff"),
            ],
        ),
        (
            "Branches",
            vec![
                (bound(&keys.checkout), "check out"),
                (bound(&keys.branch_new), "new branch"),
                (bound(&keys.branch_delete), "delete (asks first)"),
            ],
        ),
        (
            "Commits",
            vec![
                (fixed("enter"), "open the commit's changes"),
                (fixed("L"), "load more history"),
            ],
        ),
        (
            "Stash",
            vec![
                (bound(&keys.stash_pop), "pop"),
                (bound(&keys.stash_push), "stash changes"),
                (bound(&keys.stash_drop), "drop (asks first)"),
            ],
        ),
        (
            "Text boxes",
            vec![
                (
                    fixed("enter / esc"),
                    "accept / close (the commit box keeps its text)",
                ),
                (fixed("ctrl+g"), "commit box: generate a message"),
                (fixed("alt+enter"), "commit box: new line (ctrl+j too)"),
                (fixed("ctrl+w"), "delete the word before the cursor"),
                (
                    fixed("ctrl+u / ctrl+k"),
                    "delete to the start / end of the line",
                ),
                (fixed("ctrl+a / ctrl+e"), "start / end of the line"),
                (
                    fixed("ctrl+left / right"),
                    "word left / right (alt+b / alt+f too)",
                ),
            ],
        ),
        (
            "Mouse",
            vec![
                (fixed("click"), "select; buttons do what they say"),
                (
                    fixed("double-click"),
                    "open the file / check out the branch",
                ),
                (
                    fixed("right-click"),
                    "actions menu for what is under the pointer",
                ),
                (fixed("wheel"), "scroll what is under the pointer"),
                (fixed("drag a border"), "resize panels"),
                (fixed("drag in a diff"), "select lines, then y or [Copy]"),
                (fixed("click outside"), "close a menu or picker"),
            ],
        ),
    ]
}

fn key_label(key: KeyCode) -> String {
    match key {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_actions_only_show_for_the_focused_panel() {
        let has = |focus, label: &str| {
            entries(MenuContext::Panel(focus))
                .iter()
                .any(|e| e.label == label)
        };
        assert!(has(Focus::Status, "Discard changes"));
        assert!(!has(Focus::Branches, "Discard changes"));
        assert!(has(Focus::Branches, "Delete selected branch"));
        assert!(!has(Focus::Status, "Delete selected branch"));
        assert!(has(Focus::Stash, "Drop selected stash"));
        assert!(has(Focus::Diff, "Restore hunk"));
        // Creation and global actions are always there.
        for focus in [
            Focus::Status,
            Focus::Branches,
            Focus::Log,
            Focus::Stash,
            Focus::Diff,
        ] {
            for label in [
                "Commit",
                "New branch",
                "Stash changes",
                "Push",
                "Theme",
                "Quit",
            ] {
                assert!(has(focus, label), "{label} missing with {focus:?} focused");
            }
        }
    }

    #[test]
    fn everything_that_discards_work_is_marked_destructive() {
        use FooterAction as F;
        for focus in [
            Focus::Status,
            Focus::Branches,
            Focus::Log,
            Focus::Stash,
            Focus::Diff,
        ] {
            for e in entries(MenuContext::Panel(focus)) {
                let discards = matches!(
                    e.action,
                    MenuAction::Footer(
                        F::Discard | F::RestoreHunk | F::DeleteBranch | F::StashDrop
                    )
                );
                assert_eq!(e.destructive, discards, "{}", e.label);
            }
        }
    }

    #[test]
    fn key_hints_follow_the_bindings() {
        let mut keys = KeyBindings::default();
        assert_eq!(
            key_hint(MenuAction::Footer(FooterAction::Stage), &keys),
            "space"
        );
        assert_eq!(
            key_hint(MenuAction::Footer(FooterAction::Commit), &keys),
            "c"
        );
        assert_eq!(key_hint(MenuAction::Quit, &keys), "Q");
        keys.commit = vec![KeyCode::Char('C')];
        assert_eq!(
            key_hint(MenuAction::Footer(FooterAction::Commit), &keys),
            "C"
        );
        keys.commit.clear();
        assert_eq!(
            key_hint(MenuAction::Footer(FooterAction::Commit), &keys),
            ""
        );
    }

    #[test]
    fn fullscreen_menu_reaches_the_destructive_actions_too() {
        let labels: Vec<&str> = entries(MenuContext::FullDiff)
            .iter()
            .map(|e| e.label)
            .collect();
        for label in [
            "Stage hunk",
            "Restore hunk",
            "Discard file changes",
            "Close diff",
        ] {
            assert!(labels.contains(&label), "{label} missing");
        }
        // Nothing that needs the main view behind it.
        assert!(!labels.contains(&"Theme"));
    }
}
