//! Phase 4: status + diff panels (ratatui).

use crate::app::{App, Focus, HitMap, Mode, UiClick, VisualSel, LLM_FIELD_LABELS, LLM_PICKER_ROWS};
use crate::config::Theme;
use crate::input::Chord;
use crate::markdown::render_markdown;
use crate::syntax::{highlight_line, HiToken};
use crate::welcome::{WELCOME_FEATURES, WELCOME_KEYS};
use crate::words::{word_diff, WordSeg};
use crate::workspace::Workspace;
use crossterm::event::KeyCode;
use git_tui_core::branch::BranchInfo;
use git_tui_core::diff::{FileDiff, LineKind};
use git_tui_core::log::{compute_lanes, CommitInfo};
use git_tui_core::stash::StashEntry;
use git_tui_core::status::{FileState, StatusEntry};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

fn state_glyph(state: FileState, theme: Theme) -> (&'static str, Color) {
    match state {
        FileState::Staged => ("S", theme.staged),
        FileState::Unstaged => ("M", theme.unstaged),
        FileState::Untracked => ("?", theme.untracked),
        FileState::Conflicted => ("C", theme.conflicted),
        FileState::BothStagedAndUnstaged => ("B", theme.both_staged),
        // Clean files need no marker; the tree position says it all.
        FileState::Clean => (" ", theme.hint),
    }
}

fn focused_border(focused: bool, theme: Theme) -> Style {
    if focused {
        Style::default()
            .fg(theme.border_focused)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.border_unfocused)
    }
}

/// LazyVim-style panel frame: rounded corners + title that glows when
/// focused (like Telescope/border highlights in LazyVim).
fn panel_block(focused: bool, theme: Theme, title: String) -> Block<'static> {
    let title_style = if focused {
        Style::default()
            .fg(theme.border_focused)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.hint)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(focused_border(focused, theme))
        // Opaque LazyVim background: no terminal wallpaper bleed-through.
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(title)
        .title_style(title_style)
}

/// Selection wash (Telescope-style): readable fg on a tinted bg.
fn selection_style(theme: Theme) -> Style {
    Style::default()
        .fg(theme.fg)
        .bg(theme.selection_bg)
        .add_modifier(Modifier::BOLD)
}

/// Paint a rendered line with the line-cursor wash: the
/// Telescope-style selection background over the whole row (gutter
/// included) so the cursor line reads as selected. Text colors survive;
/// only the background is swapped.
fn cursor_highlight(line: &mut Line<'static>, theme: Theme) {
    for span in &mut line.spans {
        span.style = span.style.bg(theme.selection_bg);
    }
}

/// Slight highlight for the rest of the hunk under the cursor: bold text
/// only, so the red/green diff washes and syntax colors survive (the
/// cursor row itself keeps the full selection wash). Makes the active
/// hunk read as one block without hiding what changed.
fn hunk_highlight(line: &mut Line<'static>) {
    for span in &mut line.spans {
        span.style = span.style.add_modifier(Modifier::BOLD);
    }
}

/// Wash one rendered line's absolute screen cells `[start, end)` with
/// the selection background, splitting boundary spans. Charwise visual
/// selection edges: only the covered cells change; everything else
/// (syntax fg, the block cursor's reverse) survives.
fn wash_cell_range(line: &mut Line<'static>, start: usize, end: usize, theme: Theme) {
    use unicode_width::UnicodeWidthChar;
    if start >= end {
        return;
    }
    let mut used = 0usize;
    let mut si = 0;
    while si < line.spans.len() {
        let width: usize = line.spans[si]
            .content
            .chars()
            .map(|c| c.width().unwrap_or(0))
            .sum();
        let (s0, s1) = (used, used + width);
        if s1 <= start || s0 >= end {
            used = s1;
            si += 1;
            continue;
        }
        // Overlap: rebuild this span as plain/covered/plain runs.
        let content = line.spans[si].content.clone();
        let style = line.spans[si].style;
        let mut runs: Vec<(String, bool)> = vec![(String::new(), false)];
        let mut cu = s0;
        for ch in content.chars() {
            let w = ch.width().unwrap_or(0);
            let covered = cu + w > start && cu < end;
            if covered != runs.last().map(|r| r.1).unwrap_or(false) {
                runs.push((String::new(), covered));
            }
            runs.last_mut().expect("wash always has a run").0.push(ch);
            cu += w;
        }
        let mut new = Vec::with_capacity(runs.len());
        for (text, covered) in runs {
            if text.is_empty() {
                continue;
            }
            let style = if covered {
                style.bg(theme.selection_bg)
            } else {
                style
            };
            new.push(Span::styled(text, style));
        }
        if new.is_empty() {
            // Only when the span held no chars; drop it, re-examine here.
            line.spans.remove(si);
            continue;
        }
        used = s1;
        line.spans.splice(si..si + 1, new.clone());
        si += new.len();
    }
}

/// Wash plan for one diff row under an active visual selection: whole
/// rows (`Full`) or an exact display-column range (`Partial`, end
/// exclusive) on the row's cursor side. Del/add pairs wash whole — the
/// two sides hold different text, so a char range cannot mean the same
/// thing on both. Partial ranges apply to the first visual row only
/// (soft-wrapped continuations stay plain); headers are single-line, so
/// they are always exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowWash {
    None,
    Full,
    Partial { start: usize, end: usize },
}

/// Wash plan for `rows[idx]` under `sel = ((r1, c1), (r2, c2),
/// linewise)`. `gutter_w` feeds the tab expansion; text comes from the
/// same [`cursor_line_text`] the yank uses, so the wash and the yanked
/// text never disagree.
fn visual_row_wash(
    diff: &FileDiff,
    rows: &[DiffRow],
    idx: usize,
    sel: VisualSel,
    gutter_w: usize,
) -> RowWash {
    let ((r1, c1), (r2, c2), linewise) = sel;
    if idx < r1 || idx > r2 {
        return RowWash::None;
    }
    if linewise || (idx > r1 && idx < r2) {
        return RowWash::Full;
    }
    // Charwise boundary row.
    let row = &rows[idx];
    if let DiffRow::Split { left, right } = row {
        if left.kind == SideKind::Del && right.kind == SideKind::Add {
            return RowWash::Full;
        }
    }
    let text = cursor_line_text(diff, row);
    if text.is_empty() {
        return RowWash::None;
    }
    // Tab stops count from the text's screen origin: after the gutter
    // for code, after the 2-cell marker for headers.
    let origin_gutter = if matches!(row, DiffRow::Header { .. }) {
        1
    } else {
        gutter_w
    };
    let (start, end) = if r1 == r2 {
        (c1, c2.saturating_add(1))
    } else if idx == r1 {
        (c1, usize::MAX)
    } else {
        (0, c2.saturating_add(1))
    };
    let start = expanded_col(&text, start.min(text.chars().count()), origin_gutter);
    let end = if end == usize::MAX {
        usize::MAX
    } else {
        expanded_col(&text, end.min(text.chars().count() + 1), origin_gutter)
    };
    if start >= end {
        return RowWash::None;
    }
    RowWash::Partial { start, end }
}

/// Block-cursor display cell for hunk-header text: char `col` expanded
/// from the 2-cell marker start (`gutter_w = 1` reproduces that origin),
/// clamped onto the last cell like `$`. `None` for empty headers.
fn header_block_cell(header: &str, col: usize) -> Option<usize> {
    let tlen = header.chars().count();
    if tlen == 0 {
        return None;
    }
    let d = expanded_col(header, col.min(tlen - 1), 1);
    let total = expanded_col(header, tlen, 1);
    Some(2 + d.min(total.saturating_sub(1)))
}

/// Nvim block cursor on a hunk-header line: split the span holding
/// display column `want` (header text starts after the 2-cell `> `/`  `
/// marker) so exactly that cell stands alone, then reverse it.
/// Callers clamp `want` inside the line, so a target always exists.
fn header_block_cursor(line: &mut Line<'static>, want: usize) {
    use unicode_width::UnicodeWidthChar;
    let mut used = 0usize;
    for si in 0..line.spans.len() {
        let content = line.spans[si].content.clone();
        let mut byte = 0usize;
        for ch in content.chars() {
            let w = ch.width().unwrap_or(0);
            if want < used + w {
                let blen = ch.len_utf8();
                let before = content[..byte].to_string();
                let target = content[byte..byte + blen].to_string();
                let after = content[byte + blen..].to_string();
                let style = line.spans[si].style;
                let mut new = Vec::with_capacity(3);
                if !before.is_empty() {
                    new.push(Span::styled(before, style));
                }
                new.push(Span::styled(target, style.add_modifier(Modifier::REVERSED)));
                if !after.is_empty() {
                    new.push(Span::styled(after, style));
                }
                line.spans.splice(si..si + 1, new);
                return;
            }
            byte += ch.len_utf8();
            used += w;
        }
    }
}

/// Render the whole screen: a full-width vertical stack (status, files
/// tree, unified diff preview, branches, commits, stash), footer hints,
/// then any modal or the fullscreen diff overlay on top.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    app.begin_frame();
    // Solid LazyVim base: paint every cell once so transparent spans
    // (gutters, padding, unfocused chrome) never show the wallpaper.
    frame.render_widget(
        Block::default().style(Style::default().bg(app.theme().bg)),
        area,
    );
    // Two-line footer: mouse buttons + keyboard hints.
    let layout = compute_layout_focused(area, 2, app.layout_overrides(), app.focus());
    app.set_last_layout(layout);

    render_status_panel(frame, layout.status, app);
    render_files_panel(frame, layout.files, app);
    render_diff_preview_panel(frame, layout.diff, app);
    render_branches_panel(frame, layout.branches, app);
    render_commits_panel(frame, layout.commits, app);
    render_stash_panel(frame, layout.stash, app);
    render_footer(frame, layout.footer, app, false);

    match app.mode() {
        Mode::Committing => render_commit_modal(frame, area, app),
        Mode::NewBranch => render_input_modal(frame, area, app, " New branch name "),
        Mode::StashPush => render_input_modal(frame, area, app, " Stash message "),
        Mode::SetUpstream => render_input_modal(frame, area, app, " Push - set upstream (remote) "),
        Mode::SetRemote => {
            render_input_modal(frame, area, app, " Remote URL for origin (publish) ")
        }
        Mode::OpenProject => render_open_browser_modal(frame, area, app, &[]),
        Mode::ConfirmInit => render_confirm_init_modal(frame, area, app),
        Mode::ConfirmPush => render_confirm_push_modal(frame, area, app),
        Mode::LlmSettings => render_llm_modal(frame, area, app),
        Mode::ThemePicker => render_theme_modal(frame, area, app),
        Mode::ActionMenu => {
            if app.menu_return() == Mode::FullDiff {
                render_fullscreen_diff(frame, area, app);
            }
            render_action_menu(frame, area, app);
        }
        Mode::Confirm => {
            if app.confirm_back() == Mode::FullDiff {
                render_fullscreen_diff(frame, area, app);
            }
            render_confirm_modal(frame, area, app);
        }
        Mode::Help => {
            if app.help_return() == Mode::FullDiff {
                render_fullscreen_diff(frame, area, app);
            }
            render_help_modal(frame, area, app);
        }
        Mode::SearchDiff => {
            if app.search_return() == Mode::FullDiff {
                render_fullscreen_diff(frame, area, app);
            }
            render_input_modal(frame, area, app, " Search in this diff ");
        }
        Mode::FindFile => {
            // Opened fullscreen: keep the diff behind the modal.
            if app.finder_return() == Mode::FullDiff {
                render_fullscreen_diff(frame, area, app);
            }
            render_finder_modal(frame, area, app);
        }
        Mode::FullDiff => render_fullscreen_diff(frame, area, app),
        Mode::Normal => {}
    }
}

/// Render a workspace of one or more projects. A single project renders
/// exactly like [`render`]; multiple projects gain a project bar on top
/// that stays visible even with a fullscreen diff open.
pub fn render_workspace(frame: &mut Frame, ws: &Workspace) {
    let area = frame.area();
    if ws.len() <= 1 {
        render(frame, ws.current());
        if ws.welcome_visible() {
            render_welcome_modal(frame, area, ws.theme());
        }
        return;
    }
    let theme = ws.theme();
    ws.current().begin_frame();
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);
    let bar_h = 3u16.min(area.height);
    let bar = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: bar_h,
    };
    let body = Rect {
        x: area.x,
        y: area.y + bar_h,
        width: area.width,
        height: area.height.saturating_sub(bar_h),
    };
    render_project_bar(frame, bar, ws);
    let app = ws.current();
    // Two-line footer: mouse buttons + keyboard hints.
    let layout = compute_layout_focused(body, 2, app.layout_overrides(), app.focus());
    app.set_last_layout(layout);

    render_status_panel(frame, layout.status, app);
    render_files_panel(frame, layout.files, app);
    render_diff_preview_panel(frame, layout.diff, app);
    render_branches_panel(frame, layout.branches, app);
    render_commits_panel(frame, layout.commits, app);
    render_stash_panel(frame, layout.stash, app);
    render_footer(frame, layout.footer, app, true);

    match app.mode() {
        // Center under the project bar in multi-project mode (`body`).
        Mode::Committing => render_commit_modal(frame, body, app),
        Mode::NewBranch => render_input_modal(frame, body, app, " New branch name "),
        Mode::StashPush => render_input_modal(frame, body, app, " Stash message "),
        Mode::SetUpstream => render_input_modal(frame, body, app, " Push - set upstream (remote) "),
        Mode::SetRemote => {
            render_input_modal(frame, body, app, " Remote URL for origin (publish) ")
        }
        Mode::OpenProject => render_open_browser_modal(frame, body, app, ws.project_roots()),
        Mode::ConfirmInit => render_confirm_init_modal(frame, body, app),
        Mode::ConfirmPush => render_confirm_push_modal(frame, body, app),
        Mode::LlmSettings => render_llm_modal(frame, body, app),
        Mode::ThemePicker => render_theme_modal(frame, body, app),
        Mode::ActionMenu => {
            if app.menu_return() == Mode::FullDiff {
                render_fullscreen_diff(frame, body, app);
            }
            render_action_menu(frame, body, app);
        }
        Mode::Confirm => {
            if app.confirm_back() == Mode::FullDiff {
                render_fullscreen_diff(frame, body, app);
            }
            render_confirm_modal(frame, body, app);
        }
        Mode::Help => {
            if app.help_return() == Mode::FullDiff {
                render_fullscreen_diff(frame, body, app);
            }
            render_help_modal(frame, body, app);
        }
        Mode::SearchDiff => {
            if app.search_return() == Mode::FullDiff {
                render_fullscreen_diff(frame, body, app);
            }
            render_input_modal(frame, body, app, " Search in this diff ");
        }
        Mode::FindFile => {
            if app.finder_return() == Mode::FullDiff {
                render_fullscreen_diff(frame, body, app);
            }
            render_finder_modal(frame, body, app);
        }
        // The project bar stays on screen; only the body goes fullscreen.
        Mode::FullDiff => render_fullscreen_diff(frame, body, app),
        Mode::Normal => {}
    }
    if ws.welcome_visible() {
        render_welcome_modal(frame, area, ws.theme());
    }
}

/// One tab per project: `1:name (dirty)`, highlighted when active.
/// Tab screen columns are recorded on the workspace for mouse clicks.
fn render_project_bar(frame: &mut Frame, area: Rect, ws: &Workspace) {
    if area.is_empty() {
        return;
    }
    let theme = ws.theme();
    let mut spans = Vec::new();
    let mut hit: Vec<(u16, u16, usize)> = Vec::new();
    let mut x = area.x.saturating_add(1);
    for i in 0..ws.len() {
        let dirty = ws
            .project_dirty_count(i)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "…".to_string());
        let label = format!(" {}:{} ({}) ", i + 1, ws.project_name(i), dirty);
        let w = label.width() as u16;
        hit.push((x, x.saturating_add(w), i));
        x = x.saturating_add(w);
        if i == ws.index() {
            spans.push(Span::styled(label, selection_style(theme)));
        } else {
            spans.push(Span::styled(label, Style::default().fg(theme.hint)));
        }
    }
    ws.set_bar_hit(hit, area.y.saturating_add(1));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(panel_block(
            false,
            theme,
            " Projects ".to_string(),
        )),
        area,
    );
}

/// Geometry of the screen. Pure function of the area so tests can predict
/// panel corners with the same math the renderer uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScreenLayout {
    pub status: Rect,
    pub files: Rect,
    pub diff: Rect,
    pub branches: Rect,
    pub commits: Rect,
    pub stash: Rect,
    pub footer: Rect,
}

/// Mouse-driven size overrides. `rail_w` replaces the default 30%-capped
/// rail width; `heights[i]` replaces the default height of the i-th rail
/// panel (status, files, branches, commits, stash). `None` means default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LayoutOverrides {
    pub rail_w: Option<u16>,
    pub heights: [Option<u16>; 5],
}

/// Minimum widths/heights so a drag can never collapse a panel away.
pub(crate) const MIN_RAIL_W: u16 = 20;
pub(crate) const MAX_RAIL_W: u16 = 60;
pub(crate) const MIN_PANEL_H: u16 = 3;

fn default_rail_w(area_width: u16) -> u16 {
    (area_width / 10 * 3 + area_width % 10 * 3 / 10)
        .min(44)
        .min(area_width)
}

pub(crate) fn compute_layout(
    area: Rect,
    footer_h: u16,
    overrides: LayoutOverrides,
) -> ScreenLayout {
    let footer_h = footer_h.min(area.height);
    let body_h = area.height - footer_h;
    let footer = Rect {
        x: area.x,
        y: area.y + body_h,
        width: area.width,
        height: footer_h,
    };
    let rail_w = overrides
        .rail_w
        .unwrap_or_else(|| default_rail_w(area.width))
        .clamp(MIN_RAIL_W.min(area.width), area.width)
        .min(
            area.width
                .saturating_sub(20)
                .max(MIN_RAIL_W.min(area.width)),
        );
    let rail = Rect {
        x: area.x,
        y: area.y,
        width: rail_w,
        height: body_h,
    };
    let preview = Rect {
        x: area.x + rail_w,
        y: area.y,
        width: area.width - rail_w,
        height: body_h,
    };
    let defaults = [3, body_h.saturating_sub(16).max(3), 5, 4, 4];
    let mut wants = defaults;
    for (i, o) in overrides.heights.iter().enumerate() {
        if let Some(h) = o {
            wants[i] = (*h).max(MIN_PANEL_H);
        }
    }
    let mut rects = Vec::with_capacity(5);
    let mut y = rail.y;
    let end = rail.y + rail.height;
    for (i, want) in wants.iter().enumerate() {
        let h = if i == 4 {
            // Last panel takes whatever is left so the rail is seamless.
            end.saturating_sub(y)
        } else {
            (y + *want).min(end).saturating_sub(y)
        };
        rects.push(Rect {
            x: rail.x,
            y,
            width: rail.width,
            height: h,
        });
        y += h;
    }
    ScreenLayout {
        status: rects[0],
        files: rects[1],
        branches: rects[2],
        commits: rects[3],
        stash: rects[4],
        diff: preview,
        footer,
    }
}

/// Screen column of the draggable vertical divider between the left rail
/// and the diff preview (the rail's right border column).
pub(crate) fn rail_divider_x(layout: &ScreenLayout) -> u16 {
    layout.diff.x
}

/// Screen rows of the draggable horizontal dividers between rail panels:
/// the bottom border row of every rail panel except the last.
pub(crate) fn panel_divider_ys(layout: &ScreenLayout) -> [u16; 4] {
    let panels = [layout.status, layout.files, layout.branches, layout.commits];
    panels.map(|r| r.y.saturating_add(r.height.saturating_sub(1)))
}

/// Rail panel index that grows while it holds keyboard focus: status
/// focus drives the files panel (`[1]-Files`), branches/commits/stash
/// drive their own panels. `Diff` widens the preview instead, so this
/// returns `None` for it.
fn rail_focus_index(focus: Focus) -> Option<usize> {
    match focus {
        Focus::Status => Some(1),
        Focus::Branches => Some(2),
        Focus::Log => Some(3),
        Focus::Stash => Some(4),
        Focus::Diff => None,
    }
}

/// Focus-aware geometry: the section you are in automatically renders
/// bigger. Rail focus grows that panel (stealing rows from the roomiest
/// siblings, never below [`MIN_PANEL_H`]); diff focus narrows the rail
/// so the preview gains columns. Pure function of area + overrides +
/// focus, so a focus change resizes on the very next frame with no
/// stored state. Explicit divider drags still apply first — focus only
/// redistributes what remains.
pub(crate) fn compute_layout_focused(
    area: Rect,
    footer_h: u16,
    overrides: LayoutOverrides,
    focus: Focus,
) -> ScreenLayout {
    let body_h = area.height.saturating_sub(footer_h.min(area.height));
    let mut wants = [3, body_h.saturating_sub(16).max(3), 5, 4, 4];
    for (i, o) in overrides.heights.iter().enumerate() {
        if let Some(h) = o {
            wants[i] = (*h).max(MIN_PANEL_H);
        }
    }
    let mut rail_w = overrides.rail_w;
    if let Some(idx) = rail_focus_index(focus) {
        // Target ~45% of the rail, at least 10 rows, never shrinking.
        let target = (body_h * 45 / 100).max(10).max(wants[idx]);
        // Leave every other panel its minimum.
        let max_allowed = body_h.saturating_sub(MIN_PANEL_H * 4);
        if target > wants[idx] && max_allowed > wants[idx] {
            let desired = target.min(max_allowed);
            if idx == 4 {
                // Stash takes the remainder: free rows by shrinking 0..4.
                let remainder = body_h.saturating_sub(wants[0] + wants[1] + wants[2] + wants[3]);
                let mut need = desired.saturating_sub(remainder);
                let mut peers = [0, 1, 2, 3];
                peers.sort_by_key(|&p| std::cmp::Reverse(wants[p]));
                for p in peers {
                    if need == 0 {
                        break;
                    }
                    let reducible = wants[p].saturating_sub(MIN_PANEL_H);
                    let take = reducible.min(need);
                    wants[p] -= take;
                    need -= take;
                }
            } else {
                let mut extra = desired - wants[idx];
                let mut peers: Vec<usize> = (0..4).filter(|&p| p != idx).collect();
                peers.sort_by_key(|&p| std::cmp::Reverse(wants[p]));
                for p in peers {
                    if extra == 0 {
                        break;
                    }
                    let reducible = wants[p].saturating_sub(MIN_PANEL_H);
                    let take = reducible.min(extra);
                    wants[p] -= take;
                    extra -= take;
                }
                wants[idx] = desired - extra;
            }
        }
    } else {
        // Diff focus: narrow the rail to 2/3 so the preview grows.
        let base = rail_w.unwrap_or_else(|| default_rail_w(area.width));
        rail_w = Some((base * 2 / 3).max(MIN_RAIL_W.min(area.width)));
    }
    compute_layout(
        area,
        footer_h,
        LayoutOverrides {
            rail_w,
            heights: [
                Some(wants[0]),
                Some(wants[1]),
                Some(wants[2]),
                Some(wants[3]),
                Some(wants[4]),
            ],
        },
    )
}

/// Minimal-scroll follow: keep `selected` visible inside a `visible`-row
/// window, reusing the persisted offset. Pure: the caller stores the result.
fn follow_selection(selected: usize, visible: usize, current: usize) -> usize {
    if visible == 0 {
        return current;
    }
    if selected < current {
        selected
    } else if selected >= current + visible {
        selected + 1 - visible
    } else {
        current
    }
}

fn render_status_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme();
    // Info strip only: the files panel below owns the Status-focus glow
    // (that is where the cursor lives), so this border stays dim.
    let title = match app.operation_in_progress() {
        Some(operation) => format!("[1]-Status · {operation} (? to continue or abort)"),
        None => "[1]-Status".to_string(),
    };
    let mut block = panel_block(false, theme, title);
    if app.operation_in_progress().is_some() {
        block = block.border_style(Style::default().fg(theme.error));
    }
    let sync = sync_suffix(app);
    let line = match app.status() {
        None => Line::raw("loading…"),
        Some(st) if st.files.is_empty() => Line::from(vec![
            Span::styled(
                "✓ ",
                Style::default()
                    .fg(theme.branch_current)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{} → {}{}", app.repo_name(), st.branch, sync)),
        ]),
        // Count first: the rail is narrow and the tail can clip.
        Some(st) => Line::from(vec![
            Span::styled(
                "● ",
                Style::default()
                    .fg(theme.unstaged)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "{} → {} ({}){}",
                app.repo_name(),
                st.branch,
                st.files.len(),
                sync
            )),
        ]),
    };
    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// Upstream tracking suffix for the status line (` → origin/main ↑2`,
/// ` · pushing…`, ` · no upstream (P pushes)`). Empty while sync state
/// is still loading so the line never flickers.
fn sync_suffix(app: &App) -> String {
    if let Some(msg) = app.syncing() {
        return format!(" · {msg}");
    }
    let Some(st) = app.sync() else {
        return String::new();
    };
    match &st.upstream {
        Some(up) => {
            let mut div = String::new();
            if st.ahead > 0 {
                div += &format!(" ↑{}", st.ahead);
            }
            if st.behind > 0 {
                div += &format!(" ↓{}", st.behind);
            }
            format!(" → {up}{div}")
        }
        None if st.remotes.is_empty() => " · no remote (P publishes)".into(),
        None => " · no upstream (P pushes)".into(),
    }
}

/// A row of the files tree: a directory header or a file (indexed into
/// `RepoStatus::files`). Collapsed headers render folded (`▶`) and hide
/// their children; selection stays file-based, so staging keys behave
/// exactly as in the flat list. When the selected file is hidden inside
/// a collapsed dir, its shallowest collapsed ancestor header takes the
/// highlight so the cursor never disappears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileRow<'a> {
    Dir { path: &'a str, depth: usize },
    File { index: usize, depth: usize },
}

/// Cumulative ancestor prefixes: "a/b/c/f" -> ["a", "a/b", "a/b/c"].
fn ancestors(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for (i, b) in path.bytes().enumerate() {
        if b == b'/' {
            out.push(&path[..i]);
        }
    }
    out
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(crate) fn file_rows(files: &[StatusEntry]) -> Vec<FileRow<'_>> {
    use std::collections::HashSet;
    let mut emitted: HashSet<&str> = HashSet::new();
    let mut rows = Vec::new();
    for (index, f) in files.iter().enumerate() {
        let dirs = ancestors(&f.path);
        for dir in &dirs {
            if emitted.insert(*dir) {
                let depth = dir.bytes().filter(|&b| b == b'/').count();
                rows.push(FileRow::Dir { path: dir, depth });
            }
        }
        rows.push(FileRow::File {
            index,
            depth: dirs.len(),
        });
    }
    rows
}

/// The rows the files panel shows: the tree minus everything folded under
/// a collapsed dir (the collapsed header itself stays, drawn as `▶`).
/// `App` navigation walks this same list, so the cursor and the
/// highlight always agree.
pub(crate) fn visible_file_rows(
    files: &[StatusEntry],
    is_collapsed: impl Fn(&str) -> bool,
) -> Vec<FileRow<'_>> {
    file_rows(files)
        .into_iter()
        .filter(|row| {
            let path: &str = match row {
                FileRow::Dir { path, .. } => path,
                FileRow::File { index, .. } => files[*index].path.as_str(),
            };
            !ancestors(path).iter().any(|a| is_collapsed(a))
        })
        .collect()
}

fn render_files_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme();
    let focused = app.focus() == Focus::Status;
    let Some(_st) = app.status() else {
        frame.render_widget(
            Paragraph::new("loading…").block(panel_block(focused, theme, "[1]-Files".to_string())),
            area,
        );
        return;
    };
    // The browsable tree (changed files, then clean tracked ones): the
    // same list the cursor indexes into, so every file can be reached
    // and the highlight never gets stuck behind.
    let files = app.file_list();
    if files.is_empty() {
        frame.render_widget(
            Paragraph::new("(clean working tree)").block(panel_block(
                focused,
                theme,
                "[1]-Files (0)".to_string(),
            )),
            area,
        );
        return;
    }
    let rows = visible_file_rows(files, |d| app.is_collapsed(d));
    let sel = app.selected().min(files.len() - 1);
    // The open folder header the cursor sits on; else the selected file's
    // own row, or — when it is hidden inside a collapsed dir — its
    // shallowest collapsed ancestor header, which is always visible (its
    // own ancestors are all expanded).
    let sel_row = app
        .cursor_dir()
        .and_then(|dir| {
            rows.iter()
                .position(|r| matches!(r, FileRow::Dir { path, .. } if *path == dir))
        })
        .or_else(|| {
            rows.iter()
                .position(|r| matches!(r, FileRow::File { index, .. } if *index == sel))
        })
        .or_else(|| {
            ancestors(files[sel].path.as_str())
                .into_iter()
                .find(|a| app.is_collapsed(a))
                .and_then(|header| {
                    rows.iter()
                        .position(|r| matches!(r, FileRow::Dir { path, .. } if *path == header))
                })
        })
        .unwrap_or(0);
    let visible = area.height.saturating_sub(2) as usize;
    let off = follow_selection(sel_row, visible, app.files_scroll());
    app.set_files_scroll(off);
    let items: Vec<ListItem> = rows
        .iter()
        .skip(off)
        .take(visible)
        .map(|row| match row {
            FileRow::Dir { path, depth } => {
                let indent = "  ".repeat(*depth);
                let glyph = if app.is_collapsed(path) { "▶" } else { "▼" };
                ListItem::new(Line::from(vec![Span::styled(
                    format!("{indent}{glyph} {}/", basename(path)),
                    Style::default().fg(theme.hint).add_modifier(Modifier::BOLD),
                )]))
            }
            FileRow::File { index, depth } => {
                let f = &files[*index];
                let (glyph, color) = state_glyph(f.state, theme);
                let indent = "  ".repeat(*depth);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{indent}{glyph} "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(basename(&f.path).to_string(), Style::default().fg(theme.fg)),
                ]))
            }
        })
        .collect();
    let list = List::new(items)
        .block(panel_block(
            focused,
            theme,
            format!("[1]-Files ({} of {})", sel + 1, files.len()),
        ))
        .highlight_style(selection_style(theme))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select((!rows.is_empty() && visible > 0).then(|| sel_row.saturating_sub(off)));
    frame.render_stateful_widget(list, area, &mut state);
}

/// One side of a side-by-side row: a gutter number plus word segments.
/// `Blank` is the empty counterpart of a one-sided (add/del-only) row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SideKind {
    Context,
    Del,
    Add,
    Blank,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Side {
    pub no: Option<u32>,
    pub segs: Vec<WordSeg>,
    pub kind: SideKind,
}

/// A rendered diff row: a hunk separator or a paired old/new line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiffRow {
    Header { index: usize },
    Split { left: Side, right: Side },
}

/// Pair hunk lines into VS Code-style rows, tracking old/new line numbers
/// from the hunk starts. Del/add runs pair up positionally; leftover dels
/// (or adds) get a blank counterpart; context fills both sides.
pub(crate) fn diff_rows(diff: &FileDiff) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for (index, hunk) in diff.hunks.iter().enumerate() {
        rows.push(DiffRow::Header { index });
        let mut old_no = hunk.old_start;
        let mut new_no = hunk.new_start;
        let mut i = 0;
        while i < hunk.lines.len() {
            let mut dels = Vec::new();
            while i < hunk.lines.len() && hunk.lines[i].kind == LineKind::Del {
                dels.push(&hunk.lines[i]);
                i += 1;
            }
            let mut adds = Vec::new();
            while i < hunk.lines.len() && hunk.lines[i].kind == LineKind::Add {
                adds.push(&hunk.lines[i]);
                i += 1;
            }
            if dels.is_empty() && adds.is_empty() {
                let line = &hunk.lines[i];
                i += 1;
                // The @@ header is already its own row; anything else here
                // is context shown on both sides.
                if line.kind == LineKind::HunkHeader {
                    continue;
                }
                let segs = vec![WordSeg {
                    text: line.text.clone(),
                    changed: false,
                }];
                rows.push(DiffRow::Split {
                    left: Side {
                        no: Some(old_no),
                        segs: segs.clone(),
                        kind: SideKind::Context,
                    },
                    right: Side {
                        no: Some(new_no),
                        segs,
                        kind: SideKind::Context,
                    },
                });
                old_no += 1;
                new_no += 1;
                continue;
            }
            let n = dels.len().max(adds.len());
            for k in 0..n {
                let has_old = dels.get(k).is_some();
                let has_new = adds.get(k).is_some();
                let old_text = dels.get(k).map(|l| l.text.as_str()).unwrap_or("");
                let new_text = adds.get(k).map(|l| l.text.as_str()).unwrap_or("");
                let (old_segs, new_segs) = word_diff(old_text, new_text);
                rows.push(DiffRow::Split {
                    left: Side {
                        no: has_old.then_some(old_no),
                        segs: old_segs,
                        kind: if has_old {
                            SideKind::Del
                        } else {
                            SideKind::Blank
                        },
                    },
                    right: Side {
                        no: has_new.then_some(new_no),
                        segs: new_segs,
                        kind: if has_new {
                            SideKind::Add
                        } else {
                            SideKind::Blank
                        },
                    },
                });
                if has_old {
                    old_no += 1;
                }
                if has_new {
                    new_no += 1;
                }
            }
        }
    }
    rows
}

/// Unified logical-line count over precomputed rows. Used per frame
/// against the rows cached in `App` so no word diffs rerun.
pub(crate) fn rows_unified_len(rows: &[DiffRow]) -> usize {
    rows.iter().map(unified_row_count).sum()
}

/// Hunk header offset over precomputed rows, so hunk navigation can snap
/// the view to the selected hunk. Used against the rows cached in `App`
/// so no word diffs rerun.
pub(crate) fn rows_hunk_start(rows: &[DiffRow], index: usize) -> u16 {
    rows.iter()
        .position(|r| matches!(r, DiffRow::Header { index: i } if *i == index))
        .unwrap_or(0)
        .min(u16::MAX as usize) as u16
}

/// Row range `[start, end)` belonging to hunk `index` (its header row
/// through the row before the next hunk header). `None` when the hunk has
/// no header row (e.g. no diff loaded). Used to wash the hunk under the
/// cursor so the active hunk reads as one block.
pub(crate) fn hunk_row_range(rows: &[DiffRow], index: usize) -> Option<(usize, usize)> {
    let start = rows
        .iter()
        .position(|r| matches!(r, DiffRow::Header { index: i } if *i == index))?;
    let mut end = rows.len();
    for (i, r) in rows.iter().enumerate().skip(start + 1) {
        if matches!(r, DiffRow::Header { .. }) {
            end = i;
            break;
        }
    }
    Some((start, end))
}

/// Logical text under the nvim-style block cursor: the hunk header for
/// header rows, else the row's new side when it has content
/// (added/context lines), else its old side (deleted-only lines). Blank
/// sides carry no text, so the block hides there while the row wash
/// still marks the cursor row.
pub(crate) fn cursor_line_text(diff: &FileDiff, row: &DiffRow) -> String {
    match row {
        DiffRow::Header { index } => diff
            .hunks
            .get(*index)
            .map(|h| h.header.clone())
            .unwrap_or_default(),
        DiffRow::Split { left, right } => {
            let side = if cursor_side_is_right(left, right) {
                right
            } else {
                left
            };
            match side.kind {
                SideKind::Blank => String::new(),
                _ => side.segs.iter().map(|s| s.text.as_str()).collect(),
            }
        }
    }
}

/// Whether the block cursor rides the right side of a split row: the new
/// side whenever it has content, else the old side. Mirrors
/// [`cursor_line_text`] so the wash and the block never disagree.
fn cursor_side_is_right(_left: &Side, right: &Side) -> bool {
    matches!(right.kind, SideKind::Add | SideKind::Context)
}

/// Body for a diff with no hunks. Binary files get an explicit notice
/// (their bytes are never drawn); anything else has nothing to show.
fn empty_diff_text(diff: &FileDiff) -> &'static str {
    if diff.binary {
        "Binary file — content not shown"
    } else {
        "(no changes)"
    }
}

/// Visible stand-in for a control character (C0, DEL, C1). Printed raw,
/// these bytes are terminal commands (cursor moves, clears, colors) that
/// corrupt the screen long after the line scrolls away, so they are drawn
/// as their Unicode "control picture" (`␛`, `␀`, …) one cell wide.
/// Tabs and carriage returns are handled by the callers first.
fn control_picture(ch: char) -> Option<char> {
    match ch as u32 {
        c @ 0x00..=0x1f => char::from_u32(0x2400 + c),
        0x7f => Some('␡'),
        0x80..=0x9f => Some('�'),
        _ => None,
    }
}

/// Map a logical char index into screen cells, expanding tabs exactly
/// like [`side_content_cells`] (stops every 8 past the gutter) and
/// counting wide chars double, so the block cursor lands on the cell the
/// renderer painted for that char.
fn expanded_col(text: &str, col: usize, gutter_w: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    const TAB_STOP: usize = 8;
    let mut used = 0usize;
    let mut cells = gutter_w + 1;
    for (i, ch) in text.chars().enumerate() {
        if i >= col {
            break;
        }
        if ch == '\r' {
            continue;
        }
        if ch == '\t' {
            let spaces = TAB_STOP - (cells % TAB_STOP);
            used += spaces;
            cells += spaces;
        } else {
            let ch = control_picture(ch).unwrap_or(ch);
            let w = ch.width().unwrap_or(0);
            used += w;
            cells += w;
        }
    }
    used
}

/// Gutter width: right-aligned numbers, at least 4 digits wide.
fn gutter_width(rows: &[DiffRow]) -> usize {
    let mut digits = 4;
    for row in rows {
        if let DiffRow::Split { left, right } = row {
            for no in left.no.iter().chain(right.no.iter()) {
                digits = digits.max(no.to_string().len());
            }
        }
    }
    digits
}

/// Max visual rows one code line expands to. Overlong lines soft-wrap onto
/// a continuation row (blank gutter — the line number shows only on the
/// first row) instead of being clipped; anything past the last row is cut
/// with a trailing `…` so no content is silently lost.
const WRAP_MAX_LINES: usize = 2;

/// Empty counterpart of a side for padding short halves when the two
/// sides wrap to different heights: same wash, no number, no text.
fn blank_side(side: &Side) -> Side {
    Side {
        no: None,
        segs: Vec::new(),
        kind: side.kind.clone(),
    }
}

/// Styled content cells of a side: tabs expanded, `\r` dropped, syntax
/// foreground merged with the diff wash per char. No gutter, no padding,
/// no clipping — the caller wraps these into visual lines.
fn side_content_cells(
    side: &Side,
    path: &str,
    gutter_w: usize,
    theme: Theme,
) -> (Vec<(char, Style)>, Option<Color>) {
    use unicode_width::UnicodeWidthChar;
    let (bg, word_bg) = match side.kind {
        // Plain code rows sit on the opaque editor background. Deleted rows
        // carry a red wash (stronger on changed words); added rows carry a
        // single very light green wash so syntax colors stay readable —
        // green is never painted twice on the same cell.
        SideKind::Context => (Some(theme.bg), None),
        SideKind::Del => (Some(theme.diff_del_bg), Some(theme.diff_del_word_bg)),
        SideKind::Add => (Some(theme.diff_add_bg), None),
        SideKind::Blank => (Some(theme.bg), None),
    };
    // Blank counterparts stay empty even if pairing left stray segments.
    let segs: &[WordSeg] = match side.kind {
        SideKind::Blank => &[],
        _ => &side.segs,
    };
    if segs.is_empty() {
        return (Vec::new(), bg);
    }
    // Syntax colors for the whole line, then re-split by word-diff boundaries
    // so changed words keep their stronger wash without losing syntax fg.
    let full_text: String = segs.iter().map(|s| s.text.as_str()).collect();
    let hi = highlight_line(path, &full_text, theme);
    let syntax_chars = expand_tokens(&hi);
    let changed_chars = expand_changed(segs);
    // Both derive from `full_text`, so lengths match; truncate defensively
    // rather than panicking on grapheme edge cases.
    let n = syntax_chars.len().min(changed_chars.len());
    // Expand tabs to spaces and drop carriage returns before styling.
    // Terminals render a raw tab as a jump to the next 8-cell stop while
    // the width math counted it as zero cells, so tab-indented lines were
    // clipped at the wrong column and the side-by-side divider shifted.
    // Columns start after the gutter so stops match terminal behavior.
    const TAB_STOP: usize = 8;
    let mut expanded: Vec<(char, (Color, Modifier), bool)> = Vec::new();
    let mut col = gutter_w + 1;
    for (k, ch) in full_text.chars().enumerate().take(n) {
        let style = syntax_chars[k];
        let changed = changed_chars[k];
        if ch == '\r' {
            continue;
        }
        if ch == '\t' {
            let spaces = TAB_STOP - (col % TAB_STOP);
            for _ in 0..spaces {
                expanded.push((' ', style, changed));
            }
            col += spaces;
            continue;
        }
        let ch = control_picture(ch).unwrap_or(ch);
        expanded.push((ch, style, changed));
        col += ch.width().unwrap_or(0);
    }
    let mut cells = Vec::with_capacity(expanded.len());
    let mut idx = 0;
    while idx < expanded.len() {
        let (fg, modifier) = expanded[idx].1;
        let changed_flag = expanded[idx].2;
        let mut j = idx + 1;
        while j < expanded.len() && expanded[j].1 == (fg, modifier) && expanded[j].2 == changed_flag
        {
            j += 1;
        }
        let mut style = Style::default().fg(fg).add_modifier(modifier);
        // Changed runs take the stronger word wash when the kind has one
        // (deletions); kinds without it (additions) keep the single line
        // wash so green is painted exactly once.
        let wash = if changed_flag { word_bg.or(bg) } else { bg };
        if let Some(wash) = wash {
            style = style.bg(wash);
        }
        // Washed code reads brighter: bold keeps the syntax hue while
        // lifting it off the tinted background (gutter stays dim).
        if matches!(side.kind, SideKind::Del | SideKind::Add) {
            style = style.add_modifier(Modifier::BOLD);
        }
        cells.extend(expanded[idx..j].iter().map(|e| (e.0, style)));
        idx = j;
    }
    (cells, bg)
}

/// Split styled cells into at most `WRAP_MAX_LINES` chunks of at most
/// `content_w` cells each (wide chars never split across rows). Returns the
/// chunks plus whether content remains past the last chunk.
fn wrap_cells(cells: &[(char, Style)], content_w: usize) -> (Vec<Vec<(char, Style)>>, bool) {
    use unicode_width::UnicodeWidthChar;
    let mut chunks: Vec<Vec<(char, Style)>> = vec![Vec::new()];
    let mut used = 0;
    for (ch, style) in cells.iter().copied() {
        let w = ch.width().unwrap_or(0);
        if used + w > content_w {
            if chunks.len() < WRAP_MAX_LINES {
                if w > content_w {
                    // A single char wider than the whole row: no place for it.
                    return (chunks, true);
                }
                chunks.push(Vec::new());
                used = 0;
            } else {
                return (chunks, true);
            }
        }
        chunks
            .last_mut()
            .expect("wrap always has a chunk")
            .push((ch, style));
        used += w;
    }
    (chunks, false)
}

/// Group consecutive same-style chars of one visual row into spans.
fn spans_for_chunk(chunk: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut idx = 0;
    while idx < chunk.len() {
        let style = chunk[idx].1;
        let mut j = idx + 1;
        while j < chunk.len() && chunk[j].1 == style {
            j += 1;
        }
        spans.push(Span::styled(
            chunk[idx..j].iter().map(|c| c.0).collect::<String>(),
            style,
        ));
        idx = j;
    }
    spans
}

/// Render one side (gutter + text) as up to `WRAP_MAX_LINES` visual rows of
/// exactly `width` cells each.
/// LazyVim-style: syntax-highlighted foreground (treesitter-like colors)
/// on a tinted diff wash, with exactly the changed words getting a
/// stronger wash. Whole-file views are all-`Context` with no wash, so they
/// read like a LazyVim buffer: line numbers + full syntax colors.
///
/// `cursor_col` (display column inside the content, post-gutter) paints
/// the nvim-style block cursor: exactly the cell holding that column gets
/// reverse video, clamping onto the last cell past the end (like `$`).
/// An empty line with a cursor shows the block on its first padding cell.
/// `None` renders no block (other rows, blank padding sides, headers).
///
/// The line number shows only on the first row; continuation rows carry a
/// blank gutter so wrapped text never masquerades as new numbered lines.
fn render_side(
    side: &Side,
    path: &str,
    width: usize,
    gutter_w: usize,
    theme: Theme,
    cursor_col: Option<usize>,
) -> Vec<Vec<Span<'static>>> {
    use unicode_width::UnicodeWidthChar;
    let gutter_style = Style::default().fg(theme.line_nr).bg(theme.bg);
    let gutter_text = match side.no {
        Some(no) => format!("{:>gutter_w$} ", no),
        None => " ".repeat(gutter_w + 1),
    };
    // Degenerate pane: the gutter alone doesn't fit — clip it, no content.
    if width <= gutter_w + 1 {
        let mut text = String::new();
        let mut used = 0;
        for ch in gutter_text.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width {
                break;
            }
            text.push(ch);
            used += w;
        }
        text.push_str(&" ".repeat(width.saturating_sub(used)));
        return vec![vec![Span::styled(text, gutter_style)]];
    }
    let (cells, bg) = side_content_cells(side, path, gutter_w, theme);
    let mut cells = cells;
    if let Some(want) = cursor_col {
        // Nvim block cursor: reverse exactly the cell holding display
        // column `want`. Tabs are already expanded above, so columns are
        // screen cells; past the end clamps onto the last cell (like `$`).
        let mut used = 0usize;
        let mut target: Option<usize> = None;
        for (idx, (ch, _)) in cells.iter().enumerate() {
            let w = ch.width().unwrap_or(0);
            if want < used + w {
                target = Some(idx);
                break;
            }
            used += w;
        }
        if let Some(ti) = target.or_else(|| cells.len().checked_sub(1)) {
            cells[ti].1 = cells[ti].1.add_modifier(Modifier::REVERSED);
        }
    }
    let content_w = width - (gutter_w + 1);
    let (mut chunks, truncated) = wrap_cells(&cells, content_w);
    if truncated {
        // Make room for the `…` overflow marker (1 cell) at the end of the
        // last row, preserving wide-char boundaries.
        let mut ellipsis_style = Style::default().fg(theme.hint);
        if let Some(bg) = bg {
            ellipsis_style = ellipsis_style.bg(bg);
        }
        let last = chunks.last_mut().expect("wrap always has a chunk");
        let mut used: usize = last.iter().map(|(ch, _)| ch.width().unwrap_or(0)).sum();
        while used + 1 > content_w {
            let Some((ch, _)) = last.pop() else {
                break;
            };
            used = used.saturating_sub(ch.width().unwrap_or(0));
        }
        last.push(('…', ellipsis_style));
    }
    let blank_gutter = " ".repeat(gutter_w + 1);
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            // First row carries the line number; continuation rows get a
            // blank gutter so wrapped text never looks like new lines.
            let mut spans = vec![Span::styled(
                if i == 0 {
                    gutter_text.clone()
                } else {
                    blank_gutter.clone()
                },
                gutter_style,
            )];
            spans.extend(spans_for_chunk(&chunk));
            // Pad to the full cell width so the wash covers the whole pane.
            // Context/blank rows paint the opaque editor background.
            let used: usize = chunk.iter().map(|(ch, _)| ch.width().unwrap_or(0)).sum();
            let pad = content_w.saturating_sub(used);
            if cells.is_empty() && cursor_col.is_some() {
                // Block cursor on an empty line: reverse the first padding
                // cell (nvim shows the block even with no text); the rest
                // pads normally. Blank padding sides pass `None`, so only
                // a real cursor line ever takes this branch.
                let mut style = Style::default();
                if let Some(bg) = bg {
                    style = style.bg(bg);
                }
                spans.push(Span::styled(" ", style.add_modifier(Modifier::REVERSED)));
                if pad > 1 {
                    spans.push(Span::styled(" ".repeat(pad - 1), style));
                }
            } else if pad > 0 {
                let mut style = Style::default();
                if let Some(bg) = bg {
                    style = style.bg(bg);
                }
                spans.push(Span::styled(" ".repeat(pad), style));
            }
            spans
        })
        .collect()
}

/// Flatten highlight tokens to per-char (fg, modifier) for merging with
/// word-diff changed flags.
fn expand_tokens(tokens: &[HiToken]) -> Vec<(Color, Modifier)> {
    let mut out = Vec::new();
    for t in tokens {
        for _ in t.text.chars() {
            out.push((t.fg, t.modifier));
        }
    }
    out
}

/// Per-char changed flags from word-diff segments.
fn expand_changed(segs: &[WordSeg]) -> Vec<bool> {
    let mut out = Vec::new();
    for s in segs {
        for _ in s.text.chars() {
            out.push(s.changed);
        }
    }
    out
}

/// A unified preview line: single text column with a `-`/`+` marker.
/// Overlong lines wrap onto a continuation row (blank marker + blank
/// gutter) so the whole line stays visible instead of being clipped.
/// `cursor_col` paints the nvim block cursor on this side (see
/// `render_side`); callers pass it only for the cursor row's cursor side.
#[allow(clippy::too_many_arguments)]
fn unified_lines(
    marker: &'static str,
    marker_style: Style,
    side: &Side,
    path: &str,
    gutter_w: usize,
    width: usize,
    theme: Theme,
    cursor_col: Option<usize>,
) -> Vec<Line<'static>> {
    render_side(
        side,
        path,
        width.saturating_sub(2),
        gutter_w,
        theme,
        cursor_col,
    )
    .into_iter()
    .enumerate()
    .map(|(i, spans)| {
        let mut out = vec![Span::styled(
            if i == 0 { marker } else { "  " },
            marker_style,
        )];
        out.extend(spans);
        Line::from(out)
    })
    .collect()
}

/// How many unified lines a row expands to (headers count as one).
/// Needed by the viewport-follow math in `App`, same module family.
pub(crate) fn unified_row_count(row: &DiffRow) -> usize {
    match row {
        DiffRow::Header { .. } => 1,
        DiffRow::Split { left, right } => match (&left.kind, &right.kind) {
            (SideKind::Context, _) => 1,
            (SideKind::Del, SideKind::Add) => 2,
            (SideKind::Del, _) => 1,
            (_, SideKind::Add) => 1,
            _ => 0,
        },
    }
}

/// Render one DiffRow into logical lines, each expanded to 1-2 visual
/// rows (syntax-highlighted). The outer vec is per logical line (headers
/// count as one); the inner vec holds that line's visual rows after
/// soft-wrapping. Scroll offsets count logical lines; the viewport fills
/// with visual rows. `cursor` is `(block display col, block char col)`
/// for the cursor row (`None` elsewhere): the display col lands on the
/// row's cursor side (new side when it has content, else the old side),
/// the char col on hunk-header text (which has no gutter).
fn render_unified_row(
    diff: &FileDiff,
    row: &DiffRow,
    gutter_w: usize,
    width: usize,
    theme: Theme,
    cursor: Option<(Option<usize>, usize)>,
) -> Vec<Vec<Line<'static>>> {
    let path = diff.path.as_str();
    let cursor_col = cursor.and_then(|(d, _)| d);
    match row {
        DiffRow::Header { index } => {
            let mut line = Line::from(vec![Span::styled(
                format!(
                    "  {}",
                    diff.hunks
                        .get(*index)
                        .map(|h| h.header.as_str())
                        .unwrap_or("")
                ),
                Style::default().fg(theme.hint),
            )]);
            if let (Some(h), Some((_, ccol))) = (diff.hunks.get(*index), cursor) {
                if let Some(cell) = header_block_cell(&h.header, ccol) {
                    header_block_cursor(&mut line, cell);
                }
            }
            vec![vec![line]]
        }
        DiffRow::Split { left, right } => match (&left.kind, &right.kind) {
            (SideKind::Context, _) => vec![unified_lines(
                "  ",
                Style::default(),
                left,
                path,
                gutter_w,
                width,
                theme,
                cursor_col,
            )],
            (SideKind::Del, SideKind::Add) => vec![
                unified_lines(
                    "- ",
                    Style::default()
                        .fg(theme.conflicted)
                        .add_modifier(Modifier::BOLD),
                    left,
                    path,
                    gutter_w,
                    width,
                    theme,
                    None,
                ),
                unified_lines(
                    "+ ",
                    Style::default()
                        .fg(theme.staged)
                        .add_modifier(Modifier::BOLD),
                    right,
                    path,
                    gutter_w,
                    width,
                    theme,
                    cursor_col,
                ),
            ],
            (SideKind::Del, _) => vec![unified_lines(
                "- ",
                Style::default()
                    .fg(theme.conflicted)
                    .add_modifier(Modifier::BOLD),
                left,
                path,
                gutter_w,
                width,
                theme,
                cursor_col,
            )],
            (_, SideKind::Add) => vec![unified_lines(
                "+ ",
                Style::default()
                    .fg(theme.staged)
                    .add_modifier(Modifier::BOLD),
                right,
                path,
                gutter_w,
                width,
                theme,
                cursor_col,
            )],
            _ => vec![],
        },
    }
}

/// Flatten side-by-side rows into single-column unified lines for the
/// inline preview: context stays one logical line, del/add pairs become
/// two. `skip` counts logical lines (stable scroll units); the viewport
/// fills with visual rows so wrapped lines stay fully visible.
/// Takes precomputed `rows` (cached in `App`) so no word diffs rerun;
/// only the visible window (`skip`, `take`) is syntax-highlighted so
/// opening a large file stays fast. `cursor` is `(row, col)` for the
/// nvim-style block: the cursor row's logical lines get the selection
/// wash and its cursor side gets the reversed block cell — unless
/// `visual` is active, in which case the selection treatment wins
/// (nvim-like) and the block alone marks the cursor. `visual` is
/// `((r1, c1), (r2, c2), linewise)`; see `visual_row_wash`.
/// `active` is the row range `[start, end)` of the hunk under the cursor:
/// those rows get the same wash so the active hunk reads as one block
/// (the block cursor still marks the exact cursor row).
#[allow(clippy::too_many_arguments)]
fn render_unified_lines(
    diff: &FileDiff,
    rows: &[DiffRow],
    theme: Theme,
    width: usize,
    skip: usize,
    take: usize,
    cursor: Option<(usize, usize)>,
    visual: Option<VisualSel>,
    active: Option<(usize, usize)>,
) -> (Vec<Line<'static>>, usize, Vec<usize>) {
    let gutter_w = gutter_width(rows);
    let total: usize = rows_unified_len(rows);
    let (crow, ccol, c_start, c_end) = cursor
        .filter(|_| !rows.is_empty())
        .map(|(r, c)| {
            let r = r.min(rows.len().saturating_sub(1));
            let start = rows_unified_len(&rows[..r]);
            (r, c, start, start + unified_row_count(&rows[r]))
        })
        .unwrap_or((usize::MAX, 0, usize::MAX, usize::MAX));
    // Block column in screen cells for the cursor row (tabs expanded).
    let dcol = if crow < rows.len() {
        let text = cursor_line_text(diff, &rows[crow]);
        if text.is_empty() {
            None
        } else {
            Some(expanded_col(&text, ccol, gutter_w))
        }
    } else {
        None
    };
    let mut out = Vec::new();
    // `diff_rows` index per emitted visual line, so mouse clicks land on
    // exactly the row the user saw (wrapping included).
    let mut hit: Vec<usize> = Vec::new();
    let mut logical = 0;
    'rows: for (idx, row) in rows.iter().enumerate() {
        // (block display col for this row, if it is the cursor row; the
        // char column rides along for hunk-header rows, whose text has no
        // gutter).
        let ccol = if idx == crow {
            Some((dcol, ccol))
        } else {
            None
        };
        let rwash = visual
            .map(|s| visual_row_wash(diff, rows, idx, s, gutter_w))
            .unwrap_or(RowWash::None);
        let in_active = active.is_some_and(|(s, e)| idx >= s && idx < e) && visual.is_none();
        for visual_group in render_unified_row(diff, row, gutter_w, width, theme, ccol) {
            if logical < skip {
                logical += 1;
                continue;
            }
            let cursor_hl = logical >= c_start && logical < c_end;
            let hl = (cursor_hl && visual.is_none()) || rwash == RowWash::Full;
            // Slight bold for the rest of the active hunk (not the cursor
            // row itself, which already has the full wash).
            let hunk_hl = in_active && !cursor_hl;
            logical += 1;
            // Selection edges wash the first visual row only; wrapped
            // continuations stay plain. Content starts after the 2-cell
            // marker and the gutter — headers have no gutter, so their
            // base is just the marker.
            let base = if matches!(row, DiffRow::Header { .. }) {
                2
            } else {
                2 + gutter_w + 1
            };
            let edge = match rwash {
                RowWash::Partial { start, end } => Some((base + start, end.saturating_add(base))),
                _ => None,
            };
            for (vi, vline) in visual_group.into_iter().enumerate() {
                if out.len() >= take {
                    break 'rows;
                }
                let mut vline = vline;
                if hl {
                    cursor_highlight(&mut vline, theme);
                } else if hunk_hl {
                    hunk_highlight(&mut vline);
                }
                if vi == 0 {
                    if let Some((s, e)) = edge {
                        wash_cell_range(&mut vline, s, e, theme);
                    }
                }
                hit.push(idx);
                out.push(vline);
            }
        }
    }
    (out, total, hit)
}

/// Inline single-column diff preview of the selected file on the right.
/// Focusable (`tab` / `5`): j/k/Up/Down move the line cursor, PgUp/PgDn
/// page it, Enter opens the fullscreen side-by-side view.
fn render_diff_preview_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    // Browsing commits: the [5] panel mirrors the Commits selection (full
    // message, identity, file stat) instead of the status file diff.
    if app.focus() == Focus::Log {
        render_commit_overview_panel(frame, area, app);
        return;
    }
    // Recorded for the cursor-follow math in `App` (see fullscreen).
    app.set_prev_view_h(area.height.saturating_sub(2) as usize);
    let theme = app.theme();
    let focused = app.focus() == Focus::Diff;
    let Some(diff) = app.diff() else {
        let title = if app.has_files() {
            " [5]-Diff (loading…) ".to_string()
        } else {
            " [5]-Diff ".to_string()
        };
        frame.render_widget(
            Paragraph::new("").block(panel_block(focused, theme, title)),
            area,
        );
        return;
    };
    if app.show_markdown_preview() {
        let title = format!(" [5]-Preview: {} (m=raw) ", diff.path);
        let inner_w = area.width.saturating_sub(2) as usize;
        let inner_h = area.height.saturating_sub(2) as usize;
        let Some(text) = app.markdown_text() else {
            frame.render_widget(
                Paragraph::new("rendering markdown…").block(panel_block(focused, theme, title)),
                area,
            );
            return;
        };
        let rendered = render_markdown(text, theme, inner_w);
        let off = (app.diff_scroll() as usize).min(rendered.len());
        let shown: Vec<Line<'static>> = rendered
            .into_iter()
            .skip(off)
            .take(inner_h.max(1))
            .collect();
        frame.render_widget(
            Paragraph::new(shown).block(panel_block(focused, theme, title)),
            area,
        );
        return;
    }
    let title = if app.diff_whole_file() {
        format!(" [5]-File: {} ", diff.path)
    } else if app.diff_viewing_staged() == Some(true) {
        format!(" [5]-Diff: {} (staged) ", diff.path)
    } else {
        format!(" [5]-Diff: {} (unstaged) ", diff.path)
    };
    if diff.hunks.is_empty() {
        frame.render_widget(
            Paragraph::new(empty_diff_text(diff)).block(panel_block(focused, theme, title)),
            area,
        );
        return;
    }
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    // Total over cached rows (cheap, no highlighting) for the "more lines" hint…
    let rows = app.diff_rows();
    let total: usize = rows_unified_len(rows);
    let off = (app.diff_scroll() as usize).min(total);
    // …then highlight only the visible window so large files stay fast.
    let take = if total.saturating_sub(off) > inner_h && inner_h > 0 {
        inner_h.saturating_sub(1)
    } else {
        inner_h
    };
    let (mut shown, _, mut hit) = render_unified_lines(
        diff,
        rows,
        theme,
        inner_w,
        off,
        take.max(1),
        Some((app.cursor_row(), app.cursor_col())),
        app.visual_selection(),
        // Wash the hunk under the cursor so it reads as one block. Whole-
        // file views are a single hunk covering everything: no wash there.
        if app.diff_whole_file() {
            None
        } else {
            hunk_row_range(rows, app.hunk())
        },
    );
    let remaining = total.saturating_sub(off + shown.len());
    if remaining > 0 && !shown.is_empty() {
        shown.push(Line::from(vec![Span::styled(
            format!("… {remaining} more lines — enter for full screen"),
            Style::default().fg(theme.hint),
        )]));
        // Keep exactly `inner_h` lines; drop the oldest visible line, not the
        // newest, so context above stays stable.
        if shown.len() > inner_h {
            shown.remove(0);
            if !hit.is_empty() {
                hit.remove(0);
            }
        }
    }
    // Record screen-row → diff-row hits for mouse clicks (border offset 1).
    app.set_hit(
        HitMap::Preview,
        hit.into_iter()
            .enumerate()
            .map(|(i, r)| (area.y.saturating_add(1).saturating_add(i as u16), r))
            .collect(),
    );
    frame.render_widget(
        Paragraph::new(shown).block(panel_block(focused, theme, title)),
        area,
    );
}

/// Fullscreen side-by-side diff overlay (Enter opens, Esc closes).
fn render_fullscreen_diff(frame: &mut Frame, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    render_diff_pane(frame, area, app);
}

fn render_diff_pane(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme();
    let Some(diff) = app.diff() else {
        let title = if app.has_files() {
            " Full diff (loading…) ".to_string()
        } else {
            " Full diff ".to_string()
        };
        app.set_hit(HitMap::Full, Vec::new());
        app.set_last_full_rect(area, u16::MAX);
        frame.render_widget(
            Paragraph::new("").block(panel_block(true, theme, title)),
            area,
        );
        return;
    };
    if app.show_markdown_preview() {
        let title = format!(" Full preview: {} (m=raw) ", diff.path);
        let inner_w = area.width.saturating_sub(2) as usize;
        let inner_h = area.height.saturating_sub(2) as usize;
        let Some(text) = app.markdown_text() else {
            frame.render_widget(
                Paragraph::new("rendering markdown…").block(panel_block(true, theme, title)),
                area,
            );
            return;
        };
        let rendered = render_markdown(text, theme, inner_w);
        let off = (app.diff_scroll() as usize).min(rendered.len());
        let shown: Vec<Line<'static>> = rendered
            .into_iter()
            .skip(off)
            .take(inner_h.max(1))
            .collect();
        app.set_hit(HitMap::Full, Vec::new());
        app.set_last_full_rect(area, u16::MAX);
        frame.render_widget(
            Paragraph::new(shown).block(panel_block(true, theme, title)),
            area,
        );
        return;
    }
    // LazyVim buffer header: file icon + path + mode, like `LazyVim ● file`.
    let title = if let Some(view) = app.commit_view() {
        format!(
            " Commit {}: {} ({} of {}, ] [ next / previous file) ",
            view.id,
            diff.path,
            view.index + 1,
            view.files.len()
        )
    } else if app.diff_whole_file() {
        format!(" Full file: {} ", diff.path)
    } else if app.diff_viewing_staged() == Some(true) {
        format!(" Full diff: {} (staged) ", diff.path)
    } else {
        format!(" Full diff: {} (unstaged) ", diff.path)
    };
    if diff.hunks.is_empty() {
        app.set_hit(HitMap::Full, Vec::new());
        app.set_last_full_rect(area, u16::MAX);
        frame.render_widget(
            Paragraph::new(empty_diff_text(diff)).block(panel_block(true, theme, title)),
            area,
        );
        return;
    }
    // One visual row per wrapped line: left half + divider + right half,
    // so both panes scroll together under a single scroll offset. Overlong
    // code lines soft-wrap onto a continuation row (blank gutters) instead
    // of being clipped. Only the visible window is syntax-highlighted
    // (large whole-file views stay fast).
    let inner = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    // Last inner line is a clickable button bar (`[Stage hunk] …`), so the
    // mouse works fullscreen too (the main footer sits behind the overlay).
    let diff_h = inner_h.saturating_sub(1).max(1);
    let half = inner.saturating_sub(1) / 2;
    let right_w = inner.saturating_sub(half + 1);
    // Rows are cached in `App` when the diff arrives: scrolling/highlighting
    // per frame must not rerun the word diffs over the whole diff.
    let rows = app.diff_rows();
    let gutter_w = gutter_width(rows);
    let off = (app.diff_scroll() as usize).min(rows.len());
    let path = diff.path.as_str();
    let divider_style = Style::default().fg(theme.line_nr).bg(theme.bg);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner_h);
    // Screen-row → diff-row hits for mouse clicks (one entry per visual
    // line; wrapped continuations map to their row).
    let mut hit: Vec<(u16, usize)> = Vec::with_capacity(inner_h);
    let hit_y = |n: usize| area.y.saturating_add(1).saturating_add(n as u16);
    let cursor = app.cursor_row();
    // No cursor/selection wash over the Markdown preview (plain scrolling
    // there). The renderer also records the viewport height for the
    // cursor-follow math in `App`.
    let show_cursor = !app.show_markdown_preview();
    app.set_full_view_h(diff_h);
    // Active visual selection, if any: ((r1,c1), (r2,c2), linewise).
    let vis = if show_cursor {
        app.visual_selection()
    } else {
        None
    };
    // Rows of the hunk under the cursor: washed so the active hunk reads
    // as one block (the block cursor still marks the exact row). Whole-
    // file views are a single hunk covering everything: no wash there.
    let active = if show_cursor && !app.diff_whole_file() {
        hunk_row_range(rows, app.hunk())
    } else {
        None
    };
    'rows: for (ri, row) in rows.iter().skip(off).enumerate() {
        if lines.len() >= diff_h {
            break;
        }
        let idx = off + ri;
        let at_cursor = show_cursor && idx == cursor;
        // Selection plan for this row. While visual mode is on, the
        // cursor row shows only its selection treatment (nvim-like).
        let row_wash = vis
            .map(|s| visual_row_wash(diff, rows, idx, s, gutter_w))
            .unwrap_or(RowWash::None);
        let full_wash = (at_cursor && vis.is_none()) || row_wash == RowWash::Full;
        // Slight bold for the rest of the active hunk (not the cursor row,
        // which already has the full wash; not while selecting).
        let hunk_wash =
            !at_cursor && vis.is_none() && active.is_some_and(|(s, e)| idx >= s && idx < e);
        match row {
            DiffRow::Header { index } => {
                let selected = *index == app.hunk();
                let header_style = if selected {
                    Style::default()
                        .fg(theme.hunk_header)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.hint)
                };
                let mut line = Line::from(vec![Span::styled(
                    format!(
                        "{} {}",
                        if selected { ">" } else { " " },
                        diff.hunks
                            .get(*index)
                            .map(|h| h.header.as_str())
                            .unwrap_or("")
                    ),
                    header_style,
                )]);
                if full_wash {
                    cursor_highlight(&mut line, theme);
                } else if hunk_wash {
                    hunk_highlight(&mut line);
                }
                // Header text starts after the 2-cell marker.
                if let RowWash::Partial { start, end } = row_wash {
                    let end = end.saturating_add(2);
                    wash_cell_range(&mut line, 2 + start, end, theme);
                }
                if at_cursor {
                    if let Some(h) = diff.hunks.get(*index) {
                        if let Some(cell) = header_block_cell(&h.header, app.cursor_col()) {
                            header_block_cursor(&mut line, cell);
                        }
                    }
                }
                hit.push((hit_y(lines.len()), idx));
                lines.push(line);
            }
            DiffRow::Split { left, right } => {
                // Block cursor rides the new side when it has content,
                // else the old side (mirrors `cursor_line_text`).
                let on_right = cursor_side_is_right(left, right);
                // Context rows mirror the same text on both sides: a
                // partial selection edge washes both panes.
                let both = left.kind == SideKind::Context && right.kind == SideKind::Context;
                let dcol = if at_cursor {
                    let text = cursor_line_text(diff, row);
                    if text.is_empty() {
                        None
                    } else {
                        Some(expanded_col(&text, app.cursor_col(), gutter_w))
                    }
                } else {
                    None
                };
                let dcol_left = if on_right { None } else { dcol };
                let dcol_right = if on_right { dcol } else { None };
                // Selection edge ranges, absolute screen cells. Left
                // content starts after its gutter; right content after
                // the left pane, the divider, and its gutter.
                let edge = |base: usize| match row_wash {
                    RowWash::Partial { start, end } => {
                        Some((base + start, end.saturating_add(base)))
                    }
                    _ => None,
                };
                let edge_left = edge(gutter_w + 1);
                let edge_right = edge(half + 1 + gutter_w + 1);
                // Whole-file opens are one LazyVim buffer: each code line is
                // painted once, full width — never mirrored into both halves.
                if app.diff_whole_file() {
                    if left.kind == SideKind::Context {
                        for (i, spans) in render_side(left, path, inner, gutter_w, theme, dcol)
                            .into_iter()
                            .enumerate()
                        {
                            if lines.len() >= diff_h {
                                break 'rows;
                            }
                            let mut line = Line::from(spans);
                            if full_wash {
                                cursor_highlight(&mut line, theme);
                            } else if hunk_wash {
                                hunk_highlight(&mut line);
                            }
                            // First visual row only; wrapped continuations
                            // stay plain (documented limitation).
                            if i == 0 {
                                if let Some((s, e)) = edge_left {
                                    wash_cell_range(&mut line, s, e, theme);
                                }
                            }
                            hit.push((hit_y(lines.len()), idx));
                            lines.push(line);
                        }
                    } else {
                        // Defensive (production whole-file diffs are all
                        // context): stack old/new full-width so no side is
                        // silently dropped.
                        for side in [left, right] {
                            let (d, e) = if std::ptr::eq(side, left) {
                                (dcol_left, edge_left)
                            } else {
                                (dcol_right, edge_right)
                            };
                            for (i, spans) in render_side(side, path, inner, gutter_w, theme, d)
                                .into_iter()
                                .enumerate()
                            {
                                if lines.len() >= diff_h {
                                    break 'rows;
                                }
                                let mut line = Line::from(spans);
                                if full_wash {
                                    cursor_highlight(&mut line, theme);
                                } else if hunk_wash {
                                    hunk_highlight(&mut line);
                                }
                                if i == 0 {
                                    if let Some((s, e)) = e {
                                        wash_cell_range(&mut line, s, e, theme);
                                    }
                                }
                                hit.push((hit_y(lines.len()), idx));
                                lines.push(line);
                            }
                        }
                    }
                    continue;
                }
                let left_rows = render_side(left, path, half, gutter_w, theme, dcol_left);
                let right_rows = render_side(right, path, right_w, gutter_w, theme, dcol_right);
                // The shorter half is padded with blank washed rows so the
                // divider stays aligned across the wrapped height.
                let height = left_rows.len().max(right_rows.len()).max(1);
                let blank_left = render_side(&blank_side(left), path, half, gutter_w, theme, None)
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let blank_right =
                    render_side(&blank_side(right), path, right_w, gutter_w, theme, None)
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                for i in 0..height {
                    if lines.len() >= diff_h {
                        break 'rows;
                    }
                    let mut spans = left_rows
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| blank_left.clone());
                    spans.push(Span::styled("│", divider_style));
                    spans.extend(
                        right_rows
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| blank_right.clone()),
                    );
                    let mut line = Line::from(spans);
                    if full_wash {
                        cursor_highlight(&mut line, theme);
                    } else if hunk_wash {
                        hunk_highlight(&mut line);
                    }
                    // Selection edges wash the first visual row only;
                    // wrapped continuations stay plain.
                    if i == 0 {
                        if !on_right || both {
                            if let Some((s, e)) = edge_left {
                                wash_cell_range(&mut line, s, e, theme);
                            }
                        }
                        if on_right || both {
                            if let Some((s, e)) = edge_right {
                                wash_cell_range(&mut line, s, e, theme);
                            }
                        }
                    }
                    hit.push((hit_y(lines.len()), idx));
                    lines.push(line);
                }
            }
        }
    }
    // Clickable button bar on the last inner line (the main footer sits
    // behind this overlay). Buttons share [`footer_buttons`] so clicks and
    // keys stay in lockstep.
    let mut btn_spans = Vec::new();
    for (label, _) in footer_buttons(app) {
        btn_spans.push(Span::styled(
            format!("[{label}] "),
            Style::default()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // The main footer (where errors and notices normally show) is behind
    // this overlay, so they take the hint's place here.
    btn_spans.push(match (app.error(), app.notice()) {
        (Some(error), _) => Span::styled(
            error.to_string(),
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        ),
        (None, Some(notice)) => Span::styled(notice.to_string(), Style::default().fg(theme.staged)),
        (None, None) => Span::styled(
            "click a row to move · click its hunk header to jump".to_string(),
            Style::default().fg(theme.hint),
        ),
    });
    let btn_y = hit_y(lines.len());
    lines.push(Line::from(btn_spans));
    app.set_hit(HitMap::Full, hit);
    app.set_last_full_rect(area, btn_y);
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(true, theme, title)),
        area,
    );
}

/// Two-line footer: clickable mouse buttons on top, keyboard hints (or
/// the error/notice line) below. Degenerate heights show buttons first.
fn render_footer(frame: &mut Frame, area: Rect, app: &App, multi: bool) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme();
    if area.height < 2 {
        frame.render_widget(footer_buttons_line(app, theme), area);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    frame.render_widget(footer_buttons_line(app, theme), chunks[0]);
    let theme = app.theme();
    if let Some(err) = app.error() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Error: ",
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(err, Style::default().fg(theme.error)),
            ])),
            chunks[1],
        );
    } else if let Some(note) = app.notice() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                note.to_string(),
                Style::default()
                    .fg(theme.branch_current)
                    .add_modifier(Modifier::BOLD),
            )])),
            chunks[1],
        );
    } else {
        frame.render_widget(footer_hints(app, theme, multi), chunks[1]);
    }
}

/// Clickable button bar (`[Stage] [Discard] …`). Shares [`footer_buttons`]
/// with the hit-testing so clicks and rendering stay in lockstep.
fn footer_buttons_line(app: &App, theme: Theme) -> Paragraph<'static> {
    let mut spans = Vec::new();
    for (label, _) in footer_buttons(app) {
        spans.push(Span::styled(
            format!("[{label}] "),
            Style::default()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(
        "click or press the key".to_string(),
        Style::default().fg(theme.hint),
    ));
    Paragraph::new(Line::from(spans))
}

fn footer_hints(app: &App, theme: Theme, multi: bool) -> Paragraph<'static> {
    let base = match app.mode() {
        Mode::Committing if app.is_generating() => {
            "generating from staged diff… · Esc cancel"
        }
        Mode::Committing => {
            "Enter commit · Ctrl+G generate · Alt+Enter new line · Ctrl+W del word · Esc close (keeps text)"
        }
        Mode::NewBranch => "←/→ move · Home/End jump · Del deletes · Enter create branch · Esc cancel",
        Mode::StashPush => "←/→ move · Home/End jump · Del deletes · Enter stash · Esc cancel",
        Mode::SetUpstream => "←/→ move · Home/End jump · Del deletes · Enter push -u · Esc cancel",
        Mode::SetRemote => "←/→ move · Home/End jump · Del deletes · Enter add origin + push · Esc cancel",
        Mode::FullDiff => {
            "? menu · F1 keys · esc/q close · j/k line · J/K hunk · ]/[ file · space stage hunk · x restore · ctrl+f search · v/V select · y copy · g/G ends · PgUp/PgDn page"
        }
        Mode::Normal if app.focus() == Focus::Branches => {
            "? menu · F1 keys · enter checkout · a new branch · D delete · g/G ends · tab commits · q close · Q quit"
        }
        Mode::Normal if app.focus() == Focus::Log => {
            "? menu · F1 keys · j/k select · enter open commit · L more · g/G ends · tab stash · q close · Q quit"
        }
        Mode::Normal if app.focus() == Focus::Stash => {
            "? menu · F1 keys · enter pop · a stash · D drop · g/G ends · tab files · q close · Q quit"
        }
        Mode::Normal if app.focus() == Focus::Diff => {
            "? menu · F1 keys · j/k line · J/K hunk · space stage hunk · x restore · ctrl+f search · v/V select · y copy · g/G ends · enter full screen · ←/1 files"
        }
        Mode::FindFile => "type to filter · ↑/↓ move · ←/→ edit · enter open · esc cancel",
        Mode::LlmSettings => "tab/↑↓ switch field · ←/→ choose/edit · enter save · esc cancel",
        Mode::ThemePicker => "↑/↓ preview · enter keep + save · esc cancel",
        Mode::ActionMenu => "type to filter · ↑/↓ move · enter run · esc close",
        Mode::Confirm => "enter / y confirm · esc / n cancel",
        Mode::Help => "j/k scroll · PgUp/PgDn page · g/G top/bottom · esc close",
        Mode::SearchDiff => "enter search · then n / N next / previous · esc cancel",
        Mode::OpenProject => {
            "type to filter · ↑/↓ move · enter open/descend · → descend · ← up · tab jump to path · esc clear/close"
        }
        Mode::ConfirmInit => "enter git init here · esc back · any other key picks another folder",
        Mode::ConfirmPush => "enter push to upstream · esc cancel",
        Mode::Normal => {
            "? menu · F1 keys · space stage · d discard · c commit · enter diff · / find · p pull · P push · f fetch · u undo · r refresh · q close · Q quit"
        }
    };
    let switch = if multi && app.mode() == Mode::Normal {
        " · [ ] project"
    } else {
        ""
    };
    // Nvim-style mode readout while selecting.
    let vis = match app.visual() {
        Some(v) if v.mode == crate::app::VisualMode::Linewise => "-- VISUAL LINE -- · ",
        Some(_) => "-- VISUAL -- · ",
        None => "",
    };
    Paragraph::new(Line::styled(
        format!("{vis}{base}{switch}"),
        Style::default().fg(theme.hint),
    ))
}

/// Clickable footer buttons: mouse users get the core actions without
/// memorizing keys. Mirrors the keyboard bindings for the current
/// mode/focus; see [`App::click_footer_button`]. Actions that throw work
/// away (discard, restore hunk, delete branch, drop stash) are not on the
/// bar, where a stray click would fire them: they live in the actions
/// menu (`[Menu]`), which asks for confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FooterAction {
    Stage,
    Discard,
    Commit,
    Pull,
    Push,
    Find,
    OpenDiff,
    CloseDiff,
    StageHunk,
    RestoreHunk,
    Checkout,
    NewBranch,
    DeleteBranch,
    StashPop,
    StashPush,
    StashDrop,
    Theme,
    Menu,
    Copy,
}

pub(crate) fn footer_buttons(app: &App) -> Vec<(&'static str, FooterAction)> {
    use FooterAction::*;
    match app.mode() {
        Mode::FullDiff => vec![
            ("Stage hunk", StageHunk),
            ("Copy", Copy),
            ("Close", CloseDiff),
            ("Find", Find),
            ("Pull", Pull),
            ("Push", Push),
            ("Menu", Menu),
        ],
        Mode::Normal if app.focus() == Focus::Branches => vec![
            ("Checkout", Checkout),
            ("New", NewBranch),
            ("Find", Find),
            ("Menu", Menu),
        ],
        Mode::Normal if app.focus() == Focus::Stash => vec![
            ("Pop", StashPop),
            ("Stash", StashPush),
            ("Find", Find),
            ("Menu", Menu),
        ],
        Mode::Normal if app.focus() == Focus::Diff => vec![
            ("Stage hunk", StageHunk),
            ("Copy", Copy),
            ("Full screen", OpenDiff),
            ("Find", Find),
            ("Menu", Menu),
        ],
        // Nothing in the commits list can be staged: no dead buttons.
        Mode::Normal if app.focus() == Focus::Log => vec![
            ("Commit", Commit),
            ("Pull", Pull),
            ("Push", Push),
            ("Find", Find),
            ("Menu", Menu),
        ],
        Mode::Normal => vec![
            ("Stage", Stage),
            ("Commit", Commit),
            ("Pull", Pull),
            ("Push", Push),
            ("Find", Find),
            ("Diff", OpenDiff),
            ("Theme", Theme),
            ("Menu", Menu),
        ],
        _ => Vec::new(),
    }
}

/// Which footer button (if any) sits at the zero-based cell offset `x`
/// into the footer line. Must stay in lockstep with [`footer_hints`]:
/// each button renders as `[label] ` (label bytes + 3 cells).
pub(crate) fn footer_button_at(buttons: &[(&str, FooterAction)], x: usize) -> Option<FooterAction> {
    let mut off = 0usize;
    for (label, action) in buttons {
        let w = label.len() + 3; // '[' + label + ']' + ' '
        if x >= off && x < off + w {
            return Some(*action);
        }
        off += w;
    }
    None
}

/// The last `width` display cells of `text`, with a leading `…` when it had
/// to be cut. Cuts on character boundaries (paths are not always ASCII).
fn tail_fit(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if text.width() <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut used = 0;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        start = i;
    }
    format!("…{}", &text[start..])
}

/// Draw `[label]` buttons on the popup's bottom border and make them
/// clickable: each one does what its key does (see [`UiClick`]). Also
/// records the popup's area, which is what "click outside" is measured
/// against. Buttons that do not fit are left out.
fn popup_buttons(frame: &mut Frame, app: &App, popup: Rect, buttons: &[(&str, UiClick)]) {
    app.set_modal_rect(popup);
    if popup.height < 3 {
        return;
    }
    let theme = app.theme();
    let y = popup.y + popup.height - 1;
    let right = popup.x + popup.width.saturating_sub(2);
    let mut x = popup.x + 2;
    for (label, click) in buttons {
        let text = format!("[{label}]");
        let w = text.width() as u16;
        if x + w > right {
            break;
        }
        let rect = Rect::new(x, y, w, 1);
        frame.render_widget(
            Paragraph::new(Line::styled(
                text,
                Style::default()
                    .fg(theme.border_focused)
                    .bg(theme.bg)
                    .add_modifier(Modifier::BOLD),
            )),
            rect,
        );
        app.add_click(rect, *click);
        x += w + 1;
    }
}

const OK: UiClick = UiClick::Key(KeyCode::Enter);
const CANCEL: UiClick = UiClick::Key(KeyCode::Esc);

/// Help screen (`F1`): every key, grouped, in the user's own bindings.
/// Scrolls; the offset is clamped here because only the renderer knows
/// how many lines fit.
fn render_help_modal(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let sections = crate::menu::help_sections(app.keys());
    let key_w = sections
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|(key, _)| key.width()))
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (title, rows) in &sections {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(
            (*title).to_string(),
            Style::default()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ));
        for (key, what) in rows {
            let pad = key_w.saturating_sub(key.width());
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {key}{}  ", " ".repeat(pad)),
                    Style::default()
                        .fg(theme.branch_current)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled((*what).to_string(), Style::default().fg(theme.fg)),
            ]));
        }
    }
    let width = (key_w as u16 + 60).min(area.width.saturating_sub(2)).max(3);
    let popup = centered_rect(area, width, lines.len() as u16 + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let inner_h = popup.height.saturating_sub(2) as usize;
    let max = lines.len().saturating_sub(inner_h);
    let off = app.clamp_help_scroll(max);
    let title = if max == 0 {
        " Keys ".to_string()
    } else {
        format!(" Keys ({}-{} of {}) ", off + 1, off + inner_h, lines.len())
    };
    let shown: Vec<Line<'static>> = lines.into_iter().skip(off).take(inner_h).collect();
    frame.render_widget(
        Paragraph::new(shown).block(panel_block(true, theme, title)),
        popup,
    );
    popup_buttons(frame, app, popup, &[("Close", CANCEL)]);
}

/// Yes/no box before an action that throws work away. The question names
/// exactly what will be affected.
fn render_confirm_modal(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let width = 64u16.min(area.width.saturating_sub(2)).max(3);
    let inner_w = width.saturating_sub(2).max(1) as usize;
    let prompt = app.confirm_prompt().unwrap_or_default();
    let rows = wrap_draft(prompt, 0, inner_w).rows;
    // Borders + question + blank + key line.
    let popup = centered_rect(area, width, rows.len() as u16 + 4);
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let mut lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|row| {
            Line::styled(
                row,
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            )
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "enter / y: yes · esc / n: no".to_string(),
        Style::default().fg(theme.hint),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.error))
        .title(" Confirm ")
        .style(Style::default().bg(theme.bg));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    popup_buttons(frame, app, popup, &[("Yes", OK), ("No", CANCEL)]);
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

/// Visible slice of a single-line text input plus the cursor's column
/// inside it. Scrolls horizontally so a long line (e.g. a commit message
/// wider than the modal) stays editable: the cursor is always on screen.
fn input_window(text: &str, cursor: usize, width: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar;
    let chars: Vec<char> = text.chars().collect();
    let widths: Vec<usize> = chars.iter().map(|c| c.width().unwrap_or(0)).collect();
    let cursor = cursor.min(chars.len());
    let cursor_col: usize = widths[..cursor].iter().sum();
    let width = width.max(1);
    let start_col = if cursor_col >= width {
        cursor_col - width + 1
    } else {
        0
    };
    let mut out = String::new();
    let mut col = 0;
    for (i, c) in chars.iter().enumerate() {
        let w = widths[i];
        if col + w <= start_col {
            col += w;
            continue;
        }
        if col >= start_col + width {
            break;
        }
        // A wide char straddling the left edge would overflow the box:
        // show a space so columns stay aligned.
        out.push(if col < start_col { ' ' } else { *c });
        col += w;
    }
    (out, cursor_col - start_col)
}

/// Telescope-style fuzzy file finder (`/`): query line on top, ranked
/// matches below. Enter jumps the file cursor to the chosen match.
fn render_finder_modal(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let popup = centered_rect(area, 72, 14);
    frame.render_widget(Clear, popup);
    // Clear wipes to the terminal default; repaint the opaque base first.
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let matches = app.finder_matches();
    let cursor = app.finder_cursor();
    let block = panel_block(
        true,
        theme,
        format!(
            " Find files ({} match{}) ",
            matches.len(),
            if matches.len() == 1 { "" } else { "es" }
        ),
    );
    let inner_w = popup.width.saturating_sub(2) as usize;
    let inner_h = popup.height.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner_h);
    // Query line (scrolls when longer than the box).
    let (query, query_cursor) =
        input_window(app.draft(), app.draft_cursor(), inner_w.saturating_sub(2));
    lines.push(Line::from(vec![
        Span::styled(
            "> ",
            Style::default()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(query, Style::default().fg(theme.fg)),
    ]));
    // Match window follows the cursor (stateless: cursor only moves ±1 and
    // resets to 0 on every keystroke).
    let rows = inner_h.saturating_sub(1);
    let start = cursor
        .saturating_sub(rows.saturating_sub(1))
        .min(matches.len());
    if matches.is_empty() {
        lines.push(Line::styled(
            "(no matches)",
            Style::default().fg(theme.hint),
        ));
    }
    // Screen-row → match-position hits for mouse clicks.
    let mut hit: Vec<(u16, usize)> = Vec::new();
    for (row, &index) in matches.iter().skip(start).take(rows).enumerate() {
        hit.push((
            popup.y.saturating_add(1).saturating_add(lines.len() as u16),
            start + row,
        ));
        let selected = start + row == cursor;
        let (text, line_style) = match app.file_entry(index) {
            Some(entry) => {
                let (glyph, _) = state_glyph(entry.state, theme);
                (
                    format!("{} {}", glyph, entry.path),
                    if selected {
                        selection_style(theme)
                    } else {
                        Style::default().fg(theme.fg).bg(theme.bg)
                    },
                )
            }
            None => (
                "(gone)".to_string(),
                Style::default().fg(theme.hint).bg(theme.bg),
            ),
        };
        let marker = if selected { "> " } else { "  " };
        let used = marker.width() + text.width();
        let pad = inner_w.saturating_sub(used);
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), line_style),
            Span::styled(text, line_style),
            Span::styled(" ".repeat(pad), line_style),
        ]));
    }
    app.set_hit(HitMap::Finder, hit);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    popup_buttons(frame, app, popup, &[("Open", OK), ("Cancel", CANCEL)]);
    // Cursor inside the (possibly scrolled) query text.
    let cursor_x = popup.x + 1 + 2 + query_cursor as u16;
    let cursor_y = popup.y + 1;
    if cursor_x < popup.x + popup.width.saturating_sub(1) {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Theme picker (`T`): every theme name, with the highlighted one already
/// applied to the whole screen behind the popup as a live preview.
fn render_theme_modal(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let names: Vec<&str> = Theme::names().collect();
    let selected = app.theme_selected();
    // The view the picker was opened from stays visible around the popup.
    let popup = centered_rect(area, 34, names.len() as u16 + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let block = panel_block(true, theme, " Theme ".to_string());
    let inner_w = popup.width.saturating_sub(2) as usize;
    let rows = popup.height.saturating_sub(2) as usize;
    // Window follows the selection when the terminal is too short.
    let start = selected.saturating_sub(rows.saturating_sub(1));
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);
    let mut hit: Vec<(u16, usize)> = Vec::new();
    for (row, name) in names.iter().enumerate().skip(start).take(rows) {
        hit.push((popup.y + 1 + lines.len() as u16, row));
        let style = if row == selected {
            selection_style(theme)
        } else {
            Style::default().fg(theme.fg).bg(theme.bg)
        };
        let marker = if row == selected { "> " } else { "  " };
        let pad = inner_w.saturating_sub(marker.len() + name.len());
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), style),
            Span::styled(name.to_string(), style),
            Span::styled(" ".repeat(pad), style),
        ]));
    }
    app.set_hit(HitMap::ThemePicker, hit);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    popup_buttons(frame, app, popup, &[("Keep", OK), ("Cancel", CANCEL)]);
}

/// Actions menu (`?`): filter line, then every matching action with its
/// key on the right. Group headings separate the unfiltered list. A
/// destructive entry waiting for its second Enter replaces the last line
/// with a confirmation naming what it will act on.
fn render_action_menu(frame: &mut Frame, area: Rect, app: &App) {
    enum Row {
        Heading(&'static str),
        Entry(usize),
    }
    let theme = app.theme();
    let matches = app.menu_matches();
    let selected = app.menu_selected();
    let grouped = app.draft().trim().is_empty();
    let mut rows: Vec<Row> = Vec::new();
    for (i, entry) in matches.iter().enumerate() {
        if grouped && (i == 0 || matches[i - 1].group != entry.group) {
            rows.push(Row::Heading(entry.group));
        }
        rows.push(Row::Entry(i));
    }
    // Borders + filter line + rows.
    let popup = centered_rect(area, 64, rows.len() as u16 + 3);
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let block = panel_block(true, theme, " Actions ".to_string());
    let inner_w = popup.width.saturating_sub(2) as usize;
    let inner_h = popup.height.saturating_sub(2) as usize;
    let plain = Style::default().fg(theme.fg).bg(theme.bg);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner_h);
    let (query, query_cursor) =
        input_window(app.draft(), app.draft_cursor(), inner_w.saturating_sub(2));
    lines.push(Line::from(vec![
        Span::styled(
            "> ",
            Style::default()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(query, plain),
    ]));
    // Window over the rows that keeps the selection (and, when it is the
    // first of its group, its heading) on screen.
    let visible = inner_h.saturating_sub(1);
    let selected_row = rows
        .iter()
        .position(|r| matches!(r, Row::Entry(i) if *i == selected))
        .unwrap_or(0);
    let start = (selected_row + 1).saturating_sub(visible);
    let start = if start == 1 { 0 } else { start };
    if matches.is_empty() {
        lines.push(Line::styled(
            "(no matching action)",
            Style::default().fg(theme.hint).bg(theme.bg),
        ));
    }
    let mut hit: Vec<(u16, usize)> = Vec::new();
    for row in rows.iter().skip(start).take(visible) {
        match row {
            Row::Heading(group) => lines.push(Line::styled(
                group.to_string(),
                Style::default().fg(theme.hint).bg(theme.bg),
            )),
            Row::Entry(i) => {
                hit.push((popup.y + 1 + lines.len() as u16, *i));
                let entry = matches[*i];
                let is_selected = *i == selected;
                let style = if is_selected {
                    selection_style(theme)
                } else {
                    plain
                };
                let marker = if is_selected { "> " } else { "  " };
                // Filtered results lose their headings, so name the group.
                let label = if grouped {
                    entry.label.to_string()
                } else {
                    format!("{}: {}", entry.group, entry.label)
                };
                let key = crate::menu::key_hint(entry.action, app.keys());
                let pad = inner_w.saturating_sub(marker.width() + label.width() + key.width() + 1);
                let label_style = if entry.destructive {
                    style.fg(theme.error)
                } else {
                    style
                };
                lines.push(Line::from(vec![
                    Span::styled(marker.to_string(), style),
                    Span::styled(label, label_style),
                    Span::styled(" ".repeat(pad), style),
                    Span::styled(format!("{key} "), style.fg(theme.hint)),
                ]));
            }
        }
    }
    app.set_hit(HitMap::ActionMenu, hit);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    popup_buttons(frame, app, popup, &[("Run", OK), ("Close", CANCEL)]);
    let cursor_x = popup.x + 1 + 2 + query_cursor as u16;
    if cursor_x < popup.x + popup.width.saturating_sub(1) {
        frame.set_cursor_position((cursor_x, popup.y + 1));
    }
}

/// Commit modal title: shows the generating state while the LLM call is
/// in flight so Ctrl+G has visible feedback. The Ctrl+G hint lives in
/// the footer (and the empty-state placeholder), not the title.
fn commit_title(app: &App) -> &'static str {
    if app.is_generating() {
        " Commit message · generating… "
    } else if app.is_amending() {
        " Amend last commit (staged changes are added to it) "
    } else {
        " Commit message "
    }
}

/// LLM setup form (`A` in the file list): four labeled rows (provider,
/// model, API key, base URL). The provider row is an option picker
/// (`←/→` steps through it, value framed by `‹ ›`); the model row cycles
/// the list the provider reports, so it is framed the same way once that
/// fetch lands and plain text until then. A line under the rows reports
/// the fetch state. Enter saves to the config file.
fn render_llm_modal(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    // 2 borders + 4 rows + status line + blank + keymap.
    let popup = centered_rect(area, 76, 10);
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let block = panel_block(
        true,
        theme,
        " LLM setup — Ctrl+G uses this in the commit box ".to_string(),
    );
    let inner_w = popup.width.saturating_sub(2) as usize;
    let inner_h = popup.height.saturating_sub(2) as usize;
    let sel = app.llm_selected();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner_h);
    let mut cursor_col = 0;
    let mut cursor_row = 0;
    // Cells between the start of the value and the cursor: `> Label: ` plus
    // the `‹ ` on picker rows.
    let mut cursor_prefix = 0usize;
    for (i, label) in LLM_FIELD_LABELS.iter().enumerate() {
        let selected = i == sel;
        let picker = LLM_PICKER_ROWS.contains(&i) && !app.llm_row_options(i).is_empty();
        let open = if picker { "‹ " } else { "" };
        let close = if picker { " ›" } else { "" };
        let value = app.llm_field_value(i).to_string();
        let chrome = open.len() + close.len();
        let width = inner_w.saturating_sub(label.len() + 4 + chrome);
        let (visible, col) = input_window(&value, app.draft_cursor(), width.max(1));
        if selected {
            cursor_col = col;
            cursor_row = lines.len();
            cursor_prefix = 2 + label.len() + 2 + open.len();
        }
        let marker = if selected { "> " } else { "  " };
        let prefix = format!("{marker}{label}: ");
        let style = if selected {
            selection_style(theme)
        } else {
            Style::default().fg(theme.fg).bg(theme.bg)
        };
        let used = prefix.width() + open.width() + visible.width() + close.width();
        let pad = inner_w.saturating_sub(used);
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), style),
            Span::styled(label.to_string(), style),
            Span::styled(": ".to_string(), style),
            Span::styled(open.to_string(), style),
            Span::styled(visible, style),
            Span::styled(close.to_string(), style),
            Span::styled(" ".repeat(pad), style),
        ]));
    }
    // Model-list status: the row above is always editable text, so say
    // here whether the provider's list is on the way, arrived, or failed.
    if app.llm_models_loading() {
        lines.push(Line::from(Span::styled(
            " fetching models from the provider…",
            Style::default().fg(theme.hint).bg(theme.bg),
        )));
    } else if let Some(err) = app.llm_models_error() {
        lines.push(Line::from(Span::styled(
            format!(" could not list models: {err}"),
            Style::default().fg(theme.error).bg(theme.bg),
        )));
    }
    // One-line keymap inside the box, so the pickers are discoverable
    // without leaving the form.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ←/→ choose provider/model · type a custom id · enter save · esc cancel",
        Style::default().fg(theme.hint).bg(theme.bg),
    )));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    popup_buttons(frame, app, popup, &[("Save", OK), ("Cancel", CANCEL)]);
    let cursor_x = popup.x + 1 + cursor_prefix as u16 + cursor_col as u16;
    let cursor_y = popup.y + 1 + cursor_row as u16;
    if cursor_x < popup.x + popup.width.saturating_sub(1) {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Soft-wrap result for the commit draft: display rows, the char range of
/// each row, and the cursor's (row, col) in display cells.
pub(crate) struct DraftWrap {
    pub rows: Vec<String>,
    /// `(start, end)` char-index range for each row (`end` exclusive).
    /// Ranges are contiguous within a hard line; a skipped `\n` gaps them.
    pub bounds: Vec<(usize, usize)>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

/// Wrap a (possibly multi-line) draft into display rows of `width` cells.
/// Hard newlines break rows; long rows soft-wrap on word boundaries when
/// possible (char fallback for overlong words) so the commit box grows
/// vertically instead of scrolling horizontally. All chars are kept in
/// order across rows so the cursor maps exactly.
pub(crate) fn wrap_draft(text: &str, cursor: usize, width: usize) -> DraftWrap {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());

    // Hard lines as char-index ranges (`\n` excluded).
    let mut hard: Vec<(usize, usize)> = Vec::new();
    let mut hs = 0;
    for (i, ch) in chars.iter().enumerate() {
        if *ch == '\n' {
            hard.push((hs, i));
            hs = i + 1;
        }
    }
    hard.push((hs, chars.len()));

    let mut rows: Vec<String> = Vec::new();
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    for (line_start, line_end) in hard {
        if line_start >= line_end {
            rows.push(String::new());
            bounds.push((line_start, line_end));
            continue;
        }
        let mut i = line_start;
        while i < line_end {
            let mut j = i;
            let mut col = 0usize;
            while j < line_end {
                let w = chars[j].width().unwrap_or(0);
                if w > 0 && col + w > width {
                    break;
                }
                col += w;
                j += 1;
            }
            if j == i {
                // Zero-width / overwide single char: force progress.
                j = (i + 1).min(line_end);
            }
            if j >= line_end {
                rows.push(chars[i..line_end].iter().collect());
                bounds.push((i, line_end));
                i = line_end;
                continue;
            }
            // Prefer the last word boundary strictly after `i` inside the
            // fitting span; otherwise hard-break at `j`.
            let mut break_at = j;
            for p in (i + 1..j).rev() {
                if chars[p].is_whitespace() {
                    break_at = p + 1;
                    break;
                }
            }
            rows.push(chars[i..break_at].iter().collect());
            bounds.push((i, break_at));
            i = break_at;
        }
    }

    // Cursor: earliest row whose end covers it (end-of-row wins over the
    // next row's start, matching the pre-word-wrap convention).
    let mut cursor_row = 0;
    let mut cursor_col = 0;
    for (idx, &(rs, re)) in bounds.iter().enumerate() {
        if cursor <= re {
            cursor_row = idx;
            cursor_col = chars[rs..cursor]
                .iter()
                .map(|c| c.width().unwrap_or(0))
                .sum();
            break;
        }
    }

    DraftWrap {
        rows,
        bounds,
        cursor_row,
        cursor_col,
    }
}

/// Thin wrapper over [`wrap_draft`] for renderers and tests that only need
/// the display rows and cursor cell.
fn wrap_draft_lines(text: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let w = wrap_draft(text, cursor, width);
    (w.rows, w.cursor_row, w.cursor_col)
}

/// Commit message box: wraps and grows vertically with the message.
/// Single-line subjects stay one row; long lines soft-wrap on word
/// boundaries and generated multi-line messages keep their hard breaks.
/// Enter commits, ↑/↓ move by visual row, Esc cancels.
fn render_commit_modal(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    // Width is fixed; height follows the wrapped content with a floor so
    // the empty box still reads as a text area (centered_rect clamps both
    // to the screen).
    let popup_w = 60u16.min(area.width.saturating_sub(2)).max(3);
    let inner_w = popup_w.saturating_sub(2).max(1) as usize;
    // Record the content width so ↑/↓ navigate the same soft-wrapped rows.
    app.set_draft_wrap_width(inner_w);

    let draft_empty = app.draft().is_empty();
    let (rows, crow, ccol) = if draft_empty {
        // Placeholders are display-only: they never feed wrap/cursor math.
        (Vec::new(), 0usize, 0usize)
    } else {
        wrap_draft_lines(app.draft(), app.draft_cursor(), inner_w)
    };
    let content_rows = if draft_empty { 1 } else { rows.len() };
    let popup = centered_rect(
        area,
        popup_w,
        (content_rows as u16).saturating_add(2).max(5),
    );
    frame.render_widget(Clear, popup);
    // Clear wipes to the terminal default; repaint the opaque base first.
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let inner_h = popup.height.saturating_sub(2) as usize;
    // Scroll vertically only when the message outgrows the screen: keep
    // the cursor row visible.
    let start = if draft_empty || inner_h == 0 {
        0
    } else {
        crow.saturating_sub(inner_h.saturating_sub(1))
            .min(rows.len().saturating_sub(inner_h))
    };
    let lines: Vec<Line<'static>> = if draft_empty {
        let placeholder = if app.is_generating() {
            "generating…"
        } else {
            "Type a message… · Ctrl+G for AI"
        };
        vec![Line::styled(
            placeholder.to_string(),
            Style::default().fg(theme.hint),
        )]
    } else {
        rows.iter()
            .skip(start)
            .take(inner_h.max(1))
            .map(|r| Line::raw(r.clone()))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(true, theme, commit_title(app).to_string())),
        popup,
    );
    popup_buttons(
        frame,
        app,
        popup,
        &[
            ("Commit", OK),
            ("Generate", UiClick::Chord(Chord::Ctrl('g'))),
            ("Close", CANCEL),
        ],
    );
    // Cursor tracks the true edit position (start of the box when showing
    // a placeholder), even when wrapped/scrolled.
    let cursor_x = popup.x + 1 + ccol as u16;
    let cursor_y = popup.y + 1 + crow.saturating_sub(start) as u16;
    if cursor_x < popup.x + popup.width.saturating_sub(1)
        && cursor_y < popup.y + popup.height.saturating_sub(1)
    {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_input_modal(frame: &mut Frame, area: Rect, app: &App, title: &str) {
    let popup = centered_rect(area, 60, 3);
    frame.render_widget(Clear, popup);
    let inner_w = popup.width.saturating_sub(2) as usize;
    // While the LLM call is in flight and the draft is still empty, show a
    // placeholder so the modal doesn't look stuck on a blank line.
    let text = if app.is_generating() && app.draft().is_empty() {
        "generating…"
    } else {
        app.draft()
    };
    let (visible, cursor_col) = input_window(text, app.draft_cursor(), inner_w);
    let input = Paragraph::new(visible).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title),
    );
    frame.render_widget(input, popup);
    popup_buttons(frame, app, popup, &[("OK", OK), ("Cancel", CANCEL)]);
    // Cursor tracks the true edit position, even when the text is scrolled.
    let cursor_x = popup.x + 1 + cursor_col as u16;
    let cursor_y = popup.y + 1;
    if cursor_x < popup.x + popup.width.saturating_sub(1) {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Project browser (`o`): pick a folder to open. `.` opens the shown
/// folder itself, `..` goes up, subfolders open as projects (plain ones
/// offer `git init`). Repo roots carry a `[repo]` badge, folders already
/// open as tabs carry `[open]`.
/// What the project browser shows inside its frame: the folder heading,
/// the path / filter / error lines, then the rows that fit. `top` is the
/// screen row of the first line; the second value maps screen rows to list
/// indexes for mouse clicks.
pub(crate) fn browser_lines(
    browser: &crate::app::OpenBrowser,
    theme: Theme,
    (inner_w, inner_h): (usize, usize),
    top: u16,
    open_roots: &[std::path::PathBuf],
    path_visible: &str,
) -> (Vec<Line<'static>>, Vec<(u16, usize)>) {
    use crate::app::BrowserRow;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner_h);

    // Current folder on top (left-truncated when too long).
    let cwd = if browser.at_places() {
        PLACES_TITLE.to_string()
    } else {
        tail_fit(
            &browser.cwd.display().to_string(),
            inner_w.saturating_sub(2),
        )
    };
    lines.push(Line::from(vec![
        Span::styled(
            "> ",
            Style::default()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            cwd,
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ]));
    if browser.editing_path {
        lines.push(Line::from(vec![
            Span::styled("path: ", Style::default().fg(theme.hint)),
            Span::styled(path_visible.to_string(), Style::default().fg(theme.fg)),
        ]));
    }
    if !browser.filter.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("filter: ", Style::default().fg(theme.hint)),
            Span::styled(browser.filter.clone(), Style::default().fg(theme.fg)),
        ]));
    }
    if let Some(err) = browser.error.as_deref() {
        let mut text = err.to_string();
        if text.len() > inner_w {
            // Back off to a char boundary without `floor_char_boundary`
            // (stable since 1.91; MSRV is 1.88).
            let mut max = inner_w.saturating_sub(1).min(text.len());
            while !text.is_char_boundary(max) {
                max -= 1;
            }
            text.truncate(max);
        }
        lines.push(Line::from(vec![Span::styled(
            text,
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        )]));
    }

    // Scrollable rows follow the highlight (stateless, like the finder).
    let total = browser.row_count();
    let cursor = browser.selected.min(total.saturating_sub(1));
    let rows = inner_h.saturating_sub(lines.len());
    let start = cursor.saturating_sub(rows.saturating_sub(1)).min(total);
    // Screen-row → browser-list-index hits for mouse clicks.
    let mut hit: Vec<(u16, usize)> = Vec::new();
    for (row, index) in (start..total).take(rows).enumerate() {
        hit.push((top.saturating_add(lines.len() as u16), start + row));
        let selected = start + row == cursor;
        let style = if selected {
            selection_style(theme)
        } else {
            Style::default().fg(theme.fg).bg(theme.bg)
        };
        let mut spans = vec![Span::styled(
            if selected { "> " } else { "  " }.to_string(),
            style,
        )];
        match browser.row(index) {
            BrowserRow::Current => {
                spans.push(Span::styled(".".to_string(), style));
                spans.push(Span::styled(
                    "  (open this folder)".to_string(),
                    Style::default()
                        .fg(theme.hint)
                        .bg(style.bg.unwrap_or(theme.bg)),
                ));
            }
            BrowserRow::Parent => {
                spans.push(Span::styled("../".to_string(), style));
            }
            BrowserRow::Dir(i) => {
                let (name, note, is_repo) = browser
                    .view
                    .get(i)
                    .map(|e| (e.name.clone(), e.note.clone(), e.is_repo_root))
                    .unwrap_or_default();
                // Places are full paths (`C:\`), not folder names.
                let label = if browser.at_places() {
                    name.clone()
                } else {
                    format!("{name}/")
                };
                spans.push(Span::styled(label, style));
                if !note.is_empty() {
                    spans.push(Span::styled(
                        format!("  {note}"),
                        Style::default()
                            .fg(theme.hint)
                            .bg(style.bg.unwrap_or(theme.bg)),
                    ));
                }
                if is_repo {
                    spans.push(Span::styled(
                        " [repo]".to_string(),
                        Style::default()
                            .fg(theme.branch_current)
                            .bg(style.bg.unwrap_or(theme.bg))
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                let abs = browser.cwd.join(&name);
                let canon = dunce::canonicalize(&abs).unwrap_or(abs);
                if open_roots.contains(&canon) {
                    spans.push(Span::styled(
                        " [open]".to_string(),
                        Style::default()
                            .fg(theme.commit_id)
                            .bg(style.bg.unwrap_or(theme.bg))
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            }
        }
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        spans.push(Span::styled(
            " ".repeat(inner_w.saturating_sub(used)),
            style,
        ));
        lines.push(Line::from(spans));
    }
    (lines, hit)
}

/// The start screen (`activegit --pick`): the project browser on its own,
/// full height, before any project is open.
pub fn render_start(frame: &mut Frame, screen: &mut crate::start::StartScreen, theme: Theme) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);
    let width = area.width.min(92);
    let x = area.x + (area.width - width) / 2;
    let line_at = |y: u16| Rect::new(x, area.y + y, width, 1);
    if area.height < 8 || width < 24 {
        frame.render_widget(Paragraph::new("ActiveGit: window too small"), area);
        screen.hits.clear();
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " ActiveGit",
                Style::default()
                    .fg(theme.border_focused)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "   which project do you want to open?",
                Style::default().fg(theme.hint),
            ),
        ])),
        line_at(1),
    );
    let list = Rect::new(x, area.y + 3, width, area.height - 6);
    let inner_w = list.width.saturating_sub(2) as usize;
    let inner_h = list.height.saturating_sub(2) as usize;
    let path_visible = tail_fit(
        &screen.path_line,
        inner_w.saturating_sub("path: ".len() + 1),
    );
    let (lines, hits) = browser_lines(
        &screen.browser,
        theme,
        (inner_w, inner_h),
        list.y + 1,
        &[],
        &path_visible,
    );
    screen.hits = hits;
    let block = panel_block(true, theme, " Open project ".to_string());
    frame.render_widget(Paragraph::new(lines).block(block), list);
    if screen.browser.editing_path {
        let cursor_x = list.x + 1 + "path: ".len() as u16 + path_visible.width() as u16;
        frame.set_cursor_position((cursor_x.min(list.x + list.width - 2), list.y + 2));
    }
    let footer = match &screen.confirm_init {
        Some(dir) => Line::from(vec![Span::styled(
            format!(
                " Not a git repository yet: {}   enter: create one here (git init) · esc: back",
                tail_fit(&dir.display().to_string(), 30)
            ),
            Style::default()
                .fg(theme.branch_current)
                .add_modifier(Modifier::BOLD),
        )]),
        None if screen.browser.editing_path => Line::styled(
            " enter: go to this folder · esc: cancel",
            Style::default().fg(theme.hint),
        ),
        None => Line::styled(
            " enter: open · \u{2192}: into · \u{2190}: up · type: filter · tab: type a path · esc: quit",
            Style::default().fg(theme.hint),
        ),
    };
    frame.render_widget(Paragraph::new(footer), line_at(area.height - 2));
}

/// Heading of the browser's places level (above every drive).
pub(crate) const PLACES_TITLE: &str = "This PC: drives, home folder, recent projects";

fn render_open_browser_modal(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    open_roots: &[std::path::PathBuf],
) {
    let theme = app.theme();
    let Some(browser) = app.open_browser() else {
        return;
    };
    let popup = centered_rect(area, 76, 18);
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let block = panel_block(true, theme, " Open project ".to_string());
    let inner_w = popup.width.saturating_sub(2) as usize;
    let inner_h = popup.height.saturating_sub(2) as usize;
    let (path_visible, _) = input_window(
        app.draft(),
        app.draft_cursor(),
        inner_w.saturating_sub("path: ".len()),
    );
    let (lines, hit) = browser_lines(
        browser,
        theme,
        (inner_w, inner_h),
        popup.y.saturating_add(1),
        open_roots,
        &path_visible,
    );
    app.set_hit(HitMap::Browser, hit);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    popup_buttons(frame, app, popup, &[("Open", OK), ("Close", CANCEL)]);
    if browser.editing_path {
        let (_, col) = input_window(
            app.draft(),
            app.draft_cursor(),
            inner_w.saturating_sub("path: ".len()),
        );
        let cursor_x = popup.x + 1 + "path: ".len() as u16 + col as u16;
        let cursor_y = popup.y + 2;
        if cursor_x < popup.x + popup.width.saturating_sub(1) {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    } else if !browser.filter.is_empty() {
        let cursor_x = popup.x + 1 + "filter: ".len() as u16 + browser.filter.len() as u16;
        let cursor_y = popup.y + 2;
        if cursor_x < popup.x + popup.width.saturating_sub(1) {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

/// Confirm step for opening a plain directory: not a git repo yet, so the
/// user explicitly opts into `git init` instead of it happening silently.
fn render_confirm_init_modal(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let popup = centered_rect(area, 64, 6);
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let block = panel_block(true, theme, " Init new repository? ".to_string());
    let inner_w = popup.width.saturating_sub(2) as usize;
    let path = tail_fit(app.draft(), inner_w);
    let lines = vec![
        Line::raw("Not a git repository:"),
        Line::from(vec![Span::styled(
            path,
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        )]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "Enter: git init here · Esc: back · any other key: edit path",
            Style::default().fg(theme.hint),
        )]),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    popup_buttons(frame, app, popup, &[("Init", OK), ("Back", CANCEL)]);
}

/// Confirm step for pushing to the tracked upstream (`P` with an
/// upstream set): Enter pushes, Esc backs out. The `[Push]`/`[Cancel]`
/// buttons are clickable; their screen position is recorded on the app
/// for mouse hit-testing (must stay in lockstep with
/// [`App::click_confirm_push`](crate::app::App)).
fn render_confirm_push_modal(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let popup = centered_rect(area, 64, 7);
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    let block = panel_block(true, theme, " Push to upstream? ".to_string());
    let target = app
        .pending_push()
        .map(|(remote, branch)| {
            // The tracked branch can be named differently from the local.
            let upstream = app
                .sync()
                .and_then(|s| s.upstream.clone())
                .unwrap_or_else(|| format!("{remote}/{branch}"));
            format!("{branch} → {upstream}")
        })
        .unwrap_or_else(|| "(loading…)".to_string());
    let btn_style = Style::default()
        .fg(theme.border_focused)
        .add_modifier(Modifier::BOLD);
    let lines = vec![
        Line::raw("Push the current branch to its tracked upstream:"),
        Line::from(vec![Span::styled(
            target,
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        )]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("[Push] ".to_string(), btn_style),
            Span::styled(" ".to_string(), btn_style),
            Span::styled("[Cancel] ".to_string(), btn_style),
            Span::styled(
                " Enter push · Esc cancel".to_string(),
                Style::default().fg(theme.hint),
            ),
        ]),
    ];
    // Buttons start one cell past the popup border, on the last line.
    app.set_confirm_btn(Some((
        popup.x.saturating_add(1),
        popup.y.saturating_add(1).saturating_add(3),
    )));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    app.set_modal_rect(popup);
}

/// First-run welcome overlay: what activegit is, its features, and the
/// keybindings to start with. Dismissed with enter/esc/q (see the hint).
fn render_welcome_modal(frame: &mut Frame, area: Rect, theme: Theme) {
    let mut lines: Vec<Line<'static>> = vec![
        Line::styled(
            "A fast, keyboard-driven git TUI with AI commit messages",
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
    ];
    for feature in WELCOME_FEATURES {
        lines.push(Line::from(vec![
            Span::styled("· ", Style::default().fg(theme.border_focused)),
            Span::styled((*feature).to_string(), Style::default().fg(theme.fg)),
        ]));
    }
    lines.push(Line::raw(""));
    for (keys, action) in WELCOME_KEYS {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {keys:<9}"),
                Style::default()
                    .fg(theme.branch_current)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled((*action).to_string(), Style::default().fg(theme.fg)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        "enter / esc: start · o: open a project · q: dismiss",
        Style::default().fg(theme.hint),
    )]));
    let popup = centered_rect(area, 76, lines.len() as u16 + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(
                true,
                theme,
                " Welcome to activegit ".to_string(),
            ))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn branch_list_items(branches: &[BranchInfo], theme: Theme) -> Vec<ListItem<'static>> {
    branches
        .iter()
        .map(|b| {
            let (marker, style) = if b.is_head {
                (
                    "* ",
                    Style::default()
                        .fg(theme.branch_current)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("  ", Style::default().fg(theme.hint))
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(
                    b.name.clone(),
                    if b.is_head {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    format!("  {}", b.tip_summary),
                    Style::default().fg(theme.hint),
                ),
            ]))
        })
        .collect()
}

fn render_branches_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme();
    let focused = app.focus() == Focus::Branches;
    let Some(branches) = app.branches() else {
        frame.render_widget(
            Paragraph::new("loading…").block(panel_block(
                focused,
                theme,
                "[2]-Local branches".to_string(),
            )),
            area,
        );
        return;
    };
    if branches.is_empty() {
        frame.render_widget(
            Paragraph::new("(none)").block(panel_block(
                focused,
                theme,
                "[2]-Local branches (0)".to_string(),
            )),
            area,
        );
        return;
    }
    let sel = app.branch_selected().min(branches.len() - 1);
    let visible = area.height.saturating_sub(2) as usize;
    let off = follow_selection(sel, visible, app.branch_scroll());
    app.set_branch_scroll(off);
    // Slice to the scrolled window (like the files panel): the highlight
    // index is relative to the visible rows, so the list actually scrolls
    // instead of pinning the highlight to the wrong row.
    let items = branch_list_items(branches, theme);
    let list = List::new(
        items
            .into_iter()
            .skip(off)
            .take(visible)
            .collect::<Vec<_>>(),
    )
    .block(panel_block(
        focused,
        theme,
        format!("[2]-Local branches ({} of {})", sel + 1, branches.len()),
    ))
    .highlight_style(selection_style(theme))
    .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select((visible > 0).then(|| sel.saturating_sub(off)));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Lane color for the commit graph: cycles a small palette so adjacent
/// lanes stay distinguishable on every theme.
fn graph_lane_color(theme: Theme, lane: usize) -> Color {
    const SLOTS: [fn(Theme) -> Color; 6] = [
        |t| t.branch_current,
        |t| t.commit_id,
        |t| t.hunk_header,
        |t| t.both_staged,
        |t| t.syntax_function,
        |t| t.syntax_type,
    ];
    SLOTS[lane % SLOTS.len()](theme)
}

/// Two-letter author initials, like `VD` / `Aa` in the reflog graph.
/// First ASCII alphanumerics of the first and last word (`"Test User"` →
/// `"TU"`); single-word names use their first two characters.
fn author_initials(author: &str) -> String {
    let words: Vec<&str> = author.split_whitespace().collect();
    let first = words
        .first()
        .and_then(|w| w.chars().find(|c| c.is_alphanumeric()));
    let last = words
        .last()
        .and_then(|w| w.chars().find(|c| c.is_alphanumeric()));
    match (first, last) {
        (Some(a), Some(b)) if words.len() > 1 => {
            format!("{}{}", a.to_ascii_uppercase(), b.to_ascii_uppercase())
        }
        (Some(a), _) => {
            let mut chars = words
                .first()
                .unwrap_or(&"??")
                .chars()
                .filter(|c| c.is_alphanumeric());
            let x = a.to_ascii_uppercase();
            let y = chars.nth(1).map(|c| c.to_ascii_uppercase()).unwrap_or('?');
            format!("{x}{y}")
        }
        _ => "??".to_string(),
    }
}

/// Reflog-style commit rows: `<id> <initials> <graph> <tags> <summary>`,
/// where the graph is an inline lane segment — a filled `●` on the commit's
/// lane when pushed, an open `○` when still local-only (yet to push), `│`
/// on the other active lanes, joined with `─` across merges and fork/close
/// rows. Every lane keeps its own color, so each branch reads as its own
/// colored thread (node, rails, and initials all share it).
fn log_lines(entries: &[CommitInfo], theme: Theme) -> Vec<Line<'static>> {
    let lanes = compute_lanes(entries);
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let row = lanes
                .get(i)
                .copied()
                .unwrap_or(git_tui_core::log::GraphRow {
                    lane: 0,
                    width: 1,
                    is_merge: e.is_merge(),
                });
            // Cap rendered lanes so a very branchy repo can't push the
            // message off the narrow left rail.
            let width = row.width.clamp(1, 8);
            let lane = row.lane.min(width - 1);
            let prev_width = if i > 0 {
                lanes.get(i - 1).map(|r| r.width.clamp(1, 8)).unwrap_or(1)
            } else {
                width
            };
            // Merges and fork/close rows join lanes with `─`; steady-state
            // rows just stand the rails side by side.
            let joined = row.is_merge || width != prev_width;
            let lane_color = graph_lane_color(theme, lane);
            let mut spans = vec![
                Span::styled(
                    e.id.clone(),
                    Style::default()
                        .fg(theme.commit_id)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    author_initials(&e.author),
                    Style::default().fg(lane_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ];
            for l in 0..width {
                let color = graph_lane_color(theme, l);
                let glyph = if l == lane {
                    if e.pushed {
                        "●"
                    } else {
                        "○"
                    }
                } else {
                    "│"
                };
                spans.push(Span::styled(
                    glyph.to_string(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
                if l + 1 < width {
                    spans.push(Span::styled(
                        if joined { "─" } else { " " }.to_string(),
                        Style::default().fg(lane_color),
                    ));
                }
            }
            // Tags render bare (`v0.2.2`, yellow); branch refs stay
            // parenthesized green (`(HEAD -> main, side)`).
            let (tags, branches): (Vec<&String>, Vec<&String>) =
                e.refs.iter().partition(|r| r.starts_with("tag: "));
            for tag in tags {
                spans.push(Span::styled(
                    format!(" {}", tag.trim_start_matches("tag: ")),
                    Style::default()
                        .fg(theme.commit_id)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if !branches.is_empty() {
                let names: Vec<&str> = branches.iter().map(|s| s.as_str()).collect();
                spans.push(Span::styled(
                    format!(" ({})", names.join(", ")),
                    Style::default()
                        .fg(theme.branch_current)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::raw(format!(" {}", e.summary)));
            Line::from(spans)
        })
        .collect()
}

fn render_commits_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme();
    let focused = app.focus() == Focus::Log;
    let Some(entries) = app.log() else {
        frame.render_widget(
            Paragraph::new("loading…").block(panel_block(
                focused,
                theme,
                "[3]-Commits".to_string(),
            )),
            area,
        );
        return;
    };
    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new("(no commits yet)").block(panel_block(
                focused,
                theme,
                "[3]-Commits (0)".to_string(),
            )),
            area,
        );
        return;
    }
    let sel = app.log_selected().min(entries.len().saturating_sub(1));
    let visible = area.height.saturating_sub(2) as usize;
    let off = follow_selection(sel, visible, app.log_scroll());
    app.set_log_scroll(off);
    let items: Vec<ListItem<'static>> = log_lines(entries, theme)
        .into_iter()
        .map(ListItem::new)
        .skip(off)
        .take(visible)
        .collect();
    let list = List::new(items)
        .block(panel_block(
            focused,
            theme,
            format!("[3]-Commits ({} of {})", sel + 1, entries.len()),
        ))
        .highlight_style(selection_style(theme))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select((visible > 0).then(|| sel.saturating_sub(off)));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The selected commit's detail, shown in the [5] panel while the
/// Commits panel is focused: `git show --stat` distilled to message,
/// identity and a per-file change count — mirrors `render_diff_preview_panel`
/// but keyed off the log selection instead of the status file list.
fn render_commit_overview_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme();
    let focused = app.focus() == Focus::Diff;
    let Some(entries) = app.log() else {
        frame.render_widget(
            Paragraph::new("").block(panel_block(focused, theme, " [5]-Commit ".to_string())),
            area,
        );
        return;
    };
    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new("").block(panel_block(focused, theme, " [5]-Commit ".to_string())),
            area,
        );
        return;
    }
    let Some(overview) = app.commit_overview() else {
        frame.render_widget(
            Paragraph::new("loading…").block(panel_block(
                focused,
                theme,
                " [5]-Commit (loading…) ".to_string(),
            )),
            area,
        );
        return;
    };
    let title = format!(" [5]-Commit {} ", overview.id);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            format!("commit {}", overview.oid),
            Style::default()
                .fg(theme.commit_id)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Author: {} <{}>", overview.author, overview.email)),
        Line::from(format!("Date:   {}", overview.date)),
        Line::from(""),
        Line::from(Span::styled(
            overview.summary.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    for body_line in overview.body.lines() {
        lines.push(Line::from(format!("    {body_line}")));
    }
    lines.push(Line::from(""));
    if overview.files.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no file changes)",
            Style::default().fg(theme.hint),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(
                "{} file{} changed, +{} -{}",
                overview.files.len(),
                if overview.files.len() == 1 { "" } else { "s" },
                overview.insertions,
                overview.deletions,
            ),
            Style::default().fg(theme.hint),
        )));
        for file in &overview.files {
            let status_color = match file.status {
                'A' => theme.staged,
                'D' => theme.conflicted,
                _ => theme.hint,
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", file.status),
                    Style::default()
                        .fg(status_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(file.path.clone()),
                Span::raw("  "),
                Span::styled(
                    format!("+{}", file.insertions),
                    Style::default().fg(theme.staged),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("-{}", file.deletions),
                    Style::default().fg(theme.conflicted),
                ),
            ]));
        }
    }
    let inner_h = area.height.saturating_sub(2) as usize;
    // PgUp/PgDn and the wheel scroll a long message or file list.
    let off = app.clamp_overview_scroll(lines.len().saturating_sub(inner_h.max(1)));
    let shown: Vec<Line<'static>> = lines.into_iter().skip(off).take(inner_h.max(1)).collect();
    frame.render_widget(
        Paragraph::new(shown).block(panel_block(focused, theme, title)),
        area,
    );
}

fn stash_list_items(entries: &[StashEntry], theme: Theme) -> Vec<ListItem<'static>> {
    entries
        .iter()
        .map(|e| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("stash@{} ", e.index),
                    Style::default()
                        .fg(theme.commit_id)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(e.message.clone()),
            ]))
        })
        .collect()
}

fn render_stash_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme();
    let focused = app.focus() == Focus::Stash;
    let Some(entries) = app.stash() else {
        frame.render_widget(
            Paragraph::new("loading…").block(panel_block(focused, theme, "[4]-Stash".to_string())),
            area,
        );
        return;
    };
    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new("(no stashes)").block(panel_block(
                focused,
                theme,
                "[4]-Stash (0)".to_string(),
            )),
            area,
        );
        return;
    }
    let sel = app.stash_selected().min(entries.len() - 1);
    let visible = area.height.saturating_sub(2) as usize;
    let off = follow_selection(sel, visible, app.stash_scroll());
    app.set_stash_scroll(off);
    // Slice to the scrolled window (like the files panel) so the list
    // scrolls instead of pinning the highlight to the wrong row.
    let list = List::new(
        stash_list_items(entries, theme)
            .into_iter()
            .skip(off)
            .take(visible)
            .collect::<Vec<_>>(),
    )
    .block(panel_block(
        focused,
        theme,
        format!("[4]-Stash ({} of {})", sel + 1, entries.len()),
    ))
    .highlight_style(selection_style(theme))
    .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select((visible > 0).then(|| sel.saturating_sub(off)));
    frame.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::workspace::Workspace;
    use git_tui_core::jobqueue::JobQueue;
    use git_tui_core::status::{RepoStatus, StatusEntry};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let app = App::new(JobQueue::spawn(dir.path()).unwrap());
        (dir, app)
    }

    fn with_files(names: &[(&str, FileState)]) -> (tempfile::TempDir, App) {
        let (dir, mut app) = test_app();
        app.set_status_for_test(RepoStatus {
            branch: "main".into(),
            head_summary: "init".into(),
            files: names
                .iter()
                .map(|(p, s)| StatusEntry {
                    path: p.to_string(),
                    state: *s,
                })
                .collect(),
            tracked_files: vec![],
        });
        (dir, app)
    }

    fn screen(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn wscreen(ws: &Workspace, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_workspace(f, ws)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn welcome_overlay_introduces_activegit_and_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let mut ws = Workspace::open(vec![dir.path().to_path_buf()], Config::default()).unwrap();
        // Hidden by default: the normal UI shows, no intro.
        assert!(
            !wscreen(&ws, 100, 32).contains("Welcome to activegit"),
            "welcome leaked into normal render"
        );
        ws.set_show_welcome(true);
        let s = wscreen(&ws, 100, 32);
        assert!(s.contains("Welcome to activegit"), "title missing:\n{s}");
        assert!(s.contains("activegit"), "product name missing:\n{s}");
        for key in ["j / k", "enter", "space", "Ctrl+G", "q / Q"] {
            assert!(s.contains(key), "key {key} missing:\n{s}");
        }
        assert!(s.contains("open a project"), "dismiss hint missing:\n{s}");
    }
    #[test]
    fn renders_file_list_with_branch_and_selection() {
        let (_dir, app) =
            with_files(&[("a.txt", FileState::Unstaged), ("b.txt", FileState::Staged)]);
        let s = screen(&app, 80, 28);
        assert!(s.contains("main"), "branch missing:\n{s}");
        assert!(s.contains("a.txt"), "file missing:\n{s}");
        assert!(s.contains("b.txt"), "file missing:\n{s}");
        assert!(s.contains("[1]-Files"), "panel number missing:\n{s}");
    }

    #[test]
    fn renders_empty_tree_message() {
        let (_dir, app) = with_files(&[]);
        let s = screen(&app, 80, 28);
        assert!(s.contains("clean"), "empty message missing:\n{s}");
        assert!(s.contains("✓"), "clean marker missing:\n{s}");
    }

    #[test]
    fn llm_setup_modal_shows_all_fields_and_save_hint() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = test_app();
        app.on_key(KeyCode::Char('A'));
        let s = screen(&app, 100, 32);
        assert!(s.contains("LLM setup"), "modal title missing:\n{s}");
        for label in ["Provider", "Model", "API key", "Base URL"] {
            assert!(s.contains(label), "field {label} missing:\n{s}");
        }
        assert!(s.contains("openai"), "prefilled provider missing:\n{s}");
        assert!(s.contains("‹"), "picker framing missing:\n{s}");
        assert!(s.contains("←/→ choose"), "in-modal keymap missing:\n{s}");
        assert!(s.contains("enter save"), "save hint missing:\n{s}");
    }

    #[test]
    fn highlight_follows_selection_into_clean_tracked_files() {
        use crossterm::event::KeyCode;
        // a.txt changed, b.txt clean-but-tracked: both are browsable, so
        // moving down must move the `>` highlight onto b.txt.
        let (_dir, mut app) = test_app();
        app.set_status_for_test(RepoStatus {
            branch: "main".into(),
            head_summary: "init".into(),
            files: vec![StatusEntry {
                path: "a.txt".into(),
                state: FileState::Unstaged,
            }],
            tracked_files: vec!["a.txt".into(), "b.txt".into()],
        });
        app.on_key(KeyCode::Char('j'));
        assert_eq!(app.selected_file().unwrap().path, "b.txt");
        let s = screen(&app, 80, 28);
        let row_with = |name: &str| {
            s.lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("{name} row missing:\n{s}"))
                .to_string()
        };
        assert!(
            row_with("b.txt").contains('>'),
            "highlight never reached b.txt:\n{s}"
        );
        assert!(
            !row_with("a.txt").contains('>'),
            "highlight stuck on a.txt:\n{s}"
        );
    }

    #[test]
    fn file_rows_builds_nested_tree_in_first_seen_order() {
        let files = ["b.txt", "packages/agent-worker/src/worker.ts", "docs/x.md"]
            .iter()
            .map(|p| StatusEntry {
                path: p.to_string(),
                state: FileState::Unstaged,
            })
            .collect::<Vec<_>>();
        let rows = file_rows(&files);
        let dir_paths: Vec<(&str, usize)> = rows
            .iter()
            .filter_map(|r| match r {
                FileRow::Dir { path, depth } => Some((*path, *depth)),
                _ => None,
            })
            .collect();
        assert_eq!(
            dir_paths,
            vec![
                ("packages", 0),
                ("packages/agent-worker", 1),
                ("packages/agent-worker/src", 2),
                ("docs", 0),
            ]
        );
        // Root file first with no header, then headers, then files at depth.
        assert_eq!(rows[0], FileRow::File { index: 0, depth: 0 });
        assert_eq!(rows[4], FileRow::File { index: 1, depth: 3 });
        assert_eq!(rows[6], FileRow::File { index: 2, depth: 1 });
        assert_eq!(rows.len(), 7);
    }

    #[test]
    fn open_folder_header_takes_the_highlight() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[
            ("src/a.rs", FileState::Unstaged),
            ("src/nested/b.rs", FileState::Unstaged),
        ]);
        app.on_key(KeyCode::Down);
        assert_eq!(app.cursor_dir(), Some("src/nested"));
        let s = screen(&app, 100, 32);
        let row = |needle: &str| s.lines().find(|l| l.contains(needle)).unwrap().to_string();
        assert!(
            row("▼ nested/").contains('>'),
            "header not highlighted:\n{s}"
        );
        assert!(
            !row("b.rs").contains('>'),
            "file row must not be highlighted:\n{s}"
        );
    }

    #[test]
    fn collapsed_dir_hides_its_children_and_shows_folded_marker() {
        let (_dir, mut app) = with_files(&[
            ("src/a.rs", FileState::Unstaged),
            ("src/nested/b.rs", FileState::Unstaged),
            ("z.txt", FileState::Unstaged),
        ]);
        app.set_collapsed("src", true);
        let s = screen(&app, 100, 32);
        assert!(s.contains("▶ src/"), "collapsed header missing:\n{s}");
        assert!(!s.contains("a.rs"), "hidden child leaked:\n{s}");
        assert!(!s.contains("b.rs"), "hidden nested child leaked:\n{s}");
        assert!(s.contains("z.txt"), "visible file missing:\n{s}");
    }

    #[test]
    fn collapsing_nested_dir_keeps_sibling_visible() {
        let (_dir, mut app) = with_files(&[
            ("src/a.rs", FileState::Unstaged),
            ("src/nested/b.rs", FileState::Unstaged),
            ("z.txt", FileState::Unstaged),
        ]);
        app.set_collapsed("src/nested", true);
        let s = screen(&app, 100, 32);
        assert!(s.contains("▼ src/"), "expanded parent missing:\n{s}");
        assert!(s.contains("▶ nested/"), "collapsed header missing:\n{s}");
        assert!(s.contains("a.rs"), "visible sibling missing:\n{s}");
        assert!(!s.contains("b.rs"), "hidden nested child leaked:\n{s}");
        assert!(s.contains("z.txt"), "visible file missing:\n{s}");
    }

    #[test]
    fn files_panel_shows_tree_with_basename_and_counter() {
        let (_dir, app) = with_files(&[
            ("src/main.rs", FileState::Unstaged),
            ("src/app.rs", FileState::Staged),
            ("README.md", FileState::Untracked),
        ]);
        let s = screen(&app, 100, 32);
        assert!(s.contains("[1]-Files"), "panel number missing:\n{s}");
        assert!(s.contains("src/"), "dir header missing:\n{s}");
        assert!(s.contains("main.rs"), "basename missing:\n{s}");
        assert!(s.contains("1 of 3"), "counter missing:\n{s}");
        assert!(
            !s.contains("src/main.rs"),
            "full path should collapse to basename:\n{s}"
        );
    }

    #[test]
    fn status_panel_shows_repo_branch_and_dirty_count() {
        let (_dir, app) = with_files(&[("a.txt", FileState::Unstaged)]);
        let s = screen(&app, 80, 28);
        assert!(s.contains("[1]-Status"), "panel number missing:\n{s}");
        assert!(s.contains("repo → main"), "repo/branch missing:\n{s}");
        assert!(s.contains("(1)"), "dirty count missing:\n{s}");
    }

    #[test]
    fn workspace_bar_lists_every_project() {
        let dir_a = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir_a.path()).unwrap();
        let dir_b = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir_b.path()).unwrap();
        let ws = Workspace::open(
            vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()],
            Config::default(),
        )
        .unwrap();
        let backend = TestBackend::new(170, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_workspace(f, &ws)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        assert!(out.contains("Projects"), "project bar missing:\n{out}");
        assert!(
            out.contains(ws.project_name(0)),
            "first project missing:\n{out}"
        );
        assert!(
            out.contains(ws.project_name(1)),
            "second project missing:\n{out}"
        );
        assert!(out.contains("[ ] project"), "switch hint missing:\n{out}");
    }

    #[test]
    fn single_project_has_no_project_bar() {
        let (_dir, app) = with_files(&[("a.txt", FileState::Unstaged)]);
        let s = screen(&app, 80, 28);
        assert!(!s.contains("Projects"), "bar should be hidden:\n{s}");
    }

    #[test]
    fn compute_layout_gives_right_preview_most_space() {
        let l = compute_layout(Rect::new(0, 0, 100, 32), 1, LayoutOverrides::default());
        assert_eq!(l.status, Rect::new(0, 0, 30, 3));
        assert_eq!(l.diff, Rect::new(30, 0, 70, 31));
        assert_eq!(l.files.x, 0);
        assert_eq!(l.files.width, 30);
        assert!(l.files.height >= 12);
        assert_eq!(l.footer, Rect::new(0, 31, 100, 1));
    }

    #[test]
    fn layout_overrides_resize_rail_and_panels() {
        let area = Rect::new(0, 0, 100, 32);
        let l = compute_layout(
            area,
            2,
            LayoutOverrides {
                rail_w: Some(50),
                heights: [None, Some(10), None, None, None],
            },
        );
        assert_eq!(l.diff.x, 50, "preview must start at the dragged divider");
        assert_eq!(l.files.height, 10);
        // Rail stays seamless: panels tile the body with no gaps.
        let total = l.status.height
            + l.files.height
            + l.branches.height
            + l.commits.height
            + l.stash.height;
        assert_eq!(total, 30, "panels must tile the 30-row body");
        // Defaults are untouched without overrides.
        let d = compute_layout(area, 2, LayoutOverrides::default());
        assert_eq!(d.diff.x, 30);
    }

    fn rail_heights(l: &ScreenLayout) -> [u16; 5] {
        [
            l.status.height,
            l.files.height,
            l.branches.height,
            l.commits.height,
            l.stash.height,
        ]
    }

    #[test]
    fn focused_rail_panel_grows_and_siblings_shrink() {
        let area = Rect::new(0, 0, 100, 32);
        let base = compute_layout(area, 2, LayoutOverrides::default());
        let focused = compute_layout_focused(area, 2, LayoutOverrides::default(), Focus::Branches);
        assert!(
            focused.branches.height > base.branches.height,
            "focused branches must grow: base={} focused={}",
            base.branches.height,
            focused.branches.height
        );
        assert!(
            focused.files.height < base.files.height,
            "room must come from the roomiest sibling: base={} focused={}",
            base.files.height,
            focused.files.height
        );
        // Rail stays seamless: panels tile the body with no gaps.
        let total: u16 = rail_heights(&focused).iter().sum();
        assert_eq!(total, 30, "panels must tile the 30-row body");
        // Nobody collapses below the drag minimum.
        for h in rail_heights(&focused) {
            assert!(h >= MIN_PANEL_H, "panel collapsed to {h}");
        }
    }

    #[test]
    fn focused_commits_and_stash_grow() {
        let area = Rect::new(0, 0, 100, 32);
        let base = compute_layout(area, 2, LayoutOverrides::default());
        let log = compute_layout_focused(area, 2, LayoutOverrides::default(), Focus::Log);
        assert!(
            log.commits.height > base.commits.height,
            "focused commits must grow: base={} focused={}",
            base.commits.height,
            log.commits.height
        );
        let stash = compute_layout_focused(area, 2, LayoutOverrides::default(), Focus::Stash);
        assert!(
            stash.stash.height > base.stash.height,
            "focused stash must grow: base={} focused={}",
            base.stash.height,
            stash.stash.height
        );
        for l in [&log, &stash] {
            let total: u16 = rail_heights(l).iter().sum();
            assert_eq!(total, 30, "panels must tile the 30-row body");
        }
    }

    #[test]
    fn focused_diff_widens_preview() {
        let area = Rect::new(0, 0, 100, 32);
        let base = compute_layout(area, 2, LayoutOverrides::default());
        let focused = compute_layout_focused(area, 2, LayoutOverrides::default(), Focus::Diff);
        assert!(
            focused.diff.width > base.diff.width,
            "focused diff must widen: base={} focused={}",
            base.diff.width,
            focused.diff.width
        );
        assert!(
            focused.diff.x < base.diff.x,
            "rail must narrow for the preview: base={} focused={}",
            base.diff.x,
            focused.diff.x
        );
    }

    #[test]
    fn focused_layout_tiles_small_screens_without_panic() {
        let area = Rect::new(0, 0, 60, 10);
        for focus in [
            Focus::Status,
            Focus::Branches,
            Focus::Log,
            Focus::Stash,
            Focus::Diff,
        ] {
            let l = compute_layout_focused(area, 2, LayoutOverrides::default(), focus);
            let total: u16 = rail_heights(&l).iter().sum();
            assert_eq!(total, 8, "panels must tile the 8-row body");
        }
    }

    #[test]
    fn divider_geometry_matches_layout() {
        let l = compute_layout(Rect::new(0, 0, 100, 32), 2, LayoutOverrides::default());
        assert_eq!(rail_divider_x(&l), l.diff.x);
        let ys = panel_divider_ys(&l);
        let panels = [l.status, l.files, l.branches, l.commits];
        for (i, p) in panels.iter().enumerate() {
            assert_eq!(
                ys[i],
                p.y + p.height - 1,
                "divider {i} must be the panel bottom"
            );
            if i > 0 {
                assert!(ys[i] > ys[i - 1], "dividers must run top to bottom");
            }
        }
    }

    #[test]
    fn footer_buttons_match_mode_and_hit_testing() {
        let (_dir, app) = with_files(&[("a.txt", FileState::Unstaged)]);
        let buttons = footer_buttons(&app);
        assert!(
            buttons.iter().any(|(_, a)| *a == FooterAction::Stage),
            "file list must offer Stage: {buttons:?}"
        );
        // Buttons lay out as `[label] ` left to right.
        assert_eq!(footer_button_at(&buttons, 0), Some(FooterAction::Stage));
        assert_eq!(footer_button_at(&buttons, 7), Some(FooterAction::Stage));
        assert_eq!(footer_button_at(&buttons, 8), Some(FooterAction::Commit));
        // Nothing on the bar throws work away; that lives behind the menu.
        assert!(!buttons.iter().any(|(_, a)| *a == FooterAction::Discard));
        assert!(buttons.iter().any(|(_, a)| *a == FooterAction::Menu));
        assert_eq!(footer_button_at(&buttons, 10_000), None);
    }

    #[test]
    fn fullscreen_renders_clickable_button_bar() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(sample_diff(), false);
        app.on_key(KeyCode::Enter);
        let s = screen(&app, 70, 16);
        assert!(
            s.contains("[Stage hunk]"),
            "fullscreen must show clickable buttons:\n{s}"
        );
    }

    #[test]
    fn long_branch_list_scrolls_with_selection() {
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_branches_for_test(
            (0..10)
                .map(|i| BranchInfo {
                    name: format!("feat-{i}"),
                    is_head: i == 0,
                    tip_summary: "x".into(),
                })
                .collect(),
        );
        app.set_branch_selected_for_test(9);
        let buf = render_buf(&app, 100, 28);
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(
            text.contains("feat-9"),
            "selected branch must scroll into view"
        );
        assert!(
            !text.contains("feat-0"),
            "top of the list must scroll out of view:\n{text}"
        );
        // The highlight sits on the selected row, not a pinned wrong row.
        let theme = Theme::default_theme();
        let mut highlighted = false;
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            if line.contains("feat-9") && buf[(1, y)].bg == theme.selection_bg {
                highlighted = true;
            }
        }
        assert!(highlighted, "feat-9 row must carry the selection wash");
    }

    #[test]
    fn long_stash_list_scrolls_with_selection() {
        use git_tui_core::stash::StashEntry;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_stash_for_test(
            (0..10)
                .map(|i| StashEntry {
                    index: i,
                    message: format!("wip-{i}"),
                })
                .collect(),
        );
        app.set_stash_selected_for_test(9);
        let buf = render_buf(&app, 100, 28);
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(
            text.contains("wip-9"),
            "selected stash must scroll into view"
        );
        assert!(
            !text.contains("wip-0"),
            "top of the list must scroll out:\n{text}"
        );
    }

    #[test]
    fn footer_renders_clickable_button_bar() {
        let (_dir, app) = with_files(&[("a.txt", FileState::Unstaged)]);
        let s = screen(&app, 100, 28);
        assert!(s.contains("[Stage]"), "footer must show buttons:\n{s}");
        assert!(s.contains("[Find]"), "footer must show Find:\n{s}");
    }

    #[test]
    fn push_confirm_modal_shows_target_and_buttons() {
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_pending_push_for_test("origin", "main");
        let s = screen(&app, 100, 32);
        assert!(
            s.contains("Push to upstream?"),
            "confirm title missing:\n{s}"
        );
        assert!(
            s.contains("main → origin/main"),
            "push target missing:\n{s}"
        );
        assert!(s.contains("[Push]"), "push button missing:\n{s}");
        assert!(s.contains("[Cancel]"), "cancel button missing:\n{s}");
    }

    #[test]
    fn tab_indented_lines_expand_to_tab_stops() {
        let side = Side {
            no: Some(1),
            segs: vec![WordSeg {
                text: "\tfn main() {}".into(),
                changed: false,
            }],
            kind: SideKind::Context,
        };
        let rows = render_side(&side, "main.rs", 40, 4, Theme::tokyo_night(), None);
        assert_eq!(rows.len(), 1, "short line must stay on one row");
        let text: String = rows[0].iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains('\t'),
            "raw tab leaks into rendering: {text:?}"
        );
        // Gutter is "   1 " (5 cells); the tab jumps to stop 8: 3 spaces.
        assert!(
            text.starts_with("   1    fn main() {}"),
            "tab must expand to the next stop, got: {text:?}"
        );
        assert_eq!(rows[0].iter().map(Span::width).sum::<usize>(), 40);
    }

    #[test]
    fn long_tabbed_lines_wrap_to_two_exact_width_rows() {
        let side = Side {
            no: Some(1),
            segs: vec![WordSeg {
                text: format!("\t{}", "x".repeat(100)),
                changed: false,
            }],
            kind: SideKind::Context,
        };
        for width in [10, 20, 40] {
            let rows = render_side(&side, "main.rs", width, 4, Theme::tokyo_night(), None);
            assert_eq!(rows.len(), 2, "overlong line must wrap, width {width}");
            for (i, spans) in rows.iter().enumerate() {
                let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
                assert!(
                    !text.contains('\t'),
                    "raw tab leaks into rendering: {text:?}"
                );
                assert_eq!(
                    spans.iter().map(Span::width).sum::<usize>(),
                    width,
                    "row {i} must fill the pane, width {width}"
                );
            }
            // Still more content past two rows: the tail is cut with `…`.
            let tail: String = rows[1].iter().map(|s| s.content.as_ref()).collect();
            assert!(
                tail.ends_with('…'),
                "overflow must be marked, got: {tail:?}"
            );
        }
    }

    #[test]
    fn split_sides_wrap_long_unicode_lines_to_their_width() {
        let side = Side {
            no: Some(1),
            segs: vec![WordSeg {
                text: "界".repeat(60),
                changed: true,
            }],
            kind: SideKind::Del,
        };
        for width in [0, 2, 6, 20, 31] {
            let rows = render_side(&side, "a.rs", width, 4, Theme::tokyo_night(), None);
            assert!(rows.len() <= 2, "at most two visual rows, width {width}");
            for (i, spans) in rows.iter().enumerate() {
                assert_eq!(
                    spans.iter().map(Span::width).sum::<usize>(),
                    width,
                    "row {i} must fill the pane, width {width}"
                );
            }
        }
    }

    #[test]
    fn wrapped_continuation_row_keeps_blank_gutter() {
        let side = Side {
            no: Some(42),
            segs: vec![WordSeg {
                text: "x".repeat(100),
                changed: false,
            }],
            kind: SideKind::Context,
        };
        let rows = render_side(&side, "a.txt", 40, 4, Theme::tokyo_night(), None);
        assert_eq!(rows.len(), 2);
        let first: String = rows[0].iter().map(|s| s.content.as_ref()).collect();
        let second: String = rows[1].iter().map(|s| s.content.as_ref()).collect();
        // Line number only on the first row: gutter is 4 + 1 cells.
        assert!(first.starts_with("  42 "), "number missing: {first:?}");
        assert!(
            second.starts_with("     "),
            "continuation gutter must be blank: {second:?}"
        );
        assert!(!second.contains("42"), "number must not repeat: {second:?}");
    }

    #[test]
    fn short_lines_stay_on_a_single_row_without_ellipsis() {
        let side = Side {
            no: Some(7),
            segs: vec![WordSeg {
                text: "hello".into(),
                changed: false,
            }],
            kind: SideKind::Context,
        };
        let rows = render_side(&side, "a.txt", 40, 4, Theme::tokyo_night(), None);
        assert_eq!(rows.len(), 1);
        let text: String = rows[0].iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains('…'),
            "short line must not be marked: {text:?}"
        );
    }

    #[test]
    fn diff_washes_preserve_readable_syntax_colors() {
        for theme in [Theme::default_theme(), Theme::tokyo_night()] {
            for kind in [SideKind::Del, SideKind::Add] {
                let side = Side {
                    no: Some(1),
                    segs: vec![WordSeg {
                        text: "fn main() {}".into(),
                        changed: true,
                    }],
                    kind,
                };
                let rows = render_side(&side, "main.rs", 40, 4, theme, None);
                assert_eq!(rows.len(), 1, "short line must stay on one row");
                let keyword = rows[0].iter().find(|s| s.content == "fn").unwrap();
                let Color::Rgb(r, g, b) = keyword.style.bg.unwrap() else {
                    panic!("RGB wash required")
                };
                assert!(
                    r.max(g).max(b) < 100,
                    "wash must be dark enough for syntax colors"
                );
                assert_ne!(keyword.style.fg, keyword.style.bg);
            }
        }
    }

    #[test]
    fn renders_commit_modal_with_draft() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.on_key(KeyCode::Char('c'));
        app.on_key(KeyCode::Char('h'));
        app.on_key(KeyCode::Char('i'));
        let s = screen(&app, 40, 10);
        assert!(s.contains("Commit"), "modal title missing:\n{s}");
        assert!(s.contains("hi"), "draft missing:\n{s}");
    }

    #[test]
    fn input_window_keeps_short_text_whole() {
        let (visible, col) = input_window("hi", 2, 20);
        assert_eq!(visible, "hi");
        assert_eq!(col, 2);
    }

    #[test]
    fn input_window_scrolls_long_text_to_cursor() {
        let text: String = "x".repeat(100);
        // Cursor at the end: the tail is visible, cursor in the last cell
        // (9 chars + the cursor itself fill the 10-cell window).
        let (visible, col) = input_window(&text, 100, 10);
        assert_eq!(visible, "x".repeat(9));
        assert_eq!(col, 9);
        // Cursor at the start: the head is visible.
        let (visible, col) = input_window(&text, 0, 10);
        assert_eq!(visible, "x".repeat(10));
        assert_eq!(col, 0);
        // Cursor in the middle stays on screen.
        let (visible, col) = input_window(&text, 50, 10);
        assert_eq!(visible, "x".repeat(10));
        assert_eq!(col, 9);
        assert!(visible.len() <= 10);
    }

    #[test]
    fn wrap_draft_lines_wraps_and_tracks_cursor() {
        // Soft wrap at the width; cursor at the boundary sits at the row end.
        let (rows, crow, ccol) = wrap_draft_lines("hello world", 5, 5);
        assert_eq!(rows, vec!["hello", " worl", "d"]);
        assert_eq!((crow, ccol), (0, 5));
        // Cursor inside the second row.
        let (_, crow, ccol) = wrap_draft_lines("hello world", 7, 5);
        assert_eq!((crow, ccol), (1, 2));
        // Hard breaks split rows; trailing newline opens an empty row.
        let (rows, crow, ccol) = wrap_draft_lines("ab\nc\n", 5, 10);
        assert_eq!(rows, vec!["ab", "c", ""]);
        assert_eq!((crow, ccol), (2, 0));
        // Cursor clamps past the end.
        let (_, crow, ccol) = wrap_draft_lines("hi", 99, 10);
        assert_eq!((crow, ccol), (0, 2));
        // Empty draft is one empty row.
        let (rows, crow, ccol) = wrap_draft_lines("", 0, 10);
        assert_eq!(rows, vec![""]);
        assert_eq!((crow, ccol), (0, 0));
    }

    #[test]
    fn wrap_draft_lines_prefers_word_boundaries() {
        // "hello world foo" at width 11: break after the last space that
        // fits, so "world foo" stays together on the next row.
        let (rows, _, _) = wrap_draft_lines("hello world foo", 0, 11);
        assert_eq!(rows, vec!["hello ", "world foo"]);
        // Overlong words fall back to a hard char break.
        let (rows, _, _) = wrap_draft_lines("abcdefghij", 0, 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
    }

    /// Each buffer row as a String of first-chars (one per cell).
    fn commit_rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect()
            })
            .collect()
    }

    /// (x, y) of the commit modal's top-left `╭`: walk left from the
    /// " Commit message" title on its border row. Char indices, not bytes.
    fn commit_modal_origin(rows: &[String]) -> Option<(u16, u16)> {
        for (y, row) in rows.iter().enumerate() {
            let chars: Vec<char> = row.chars().collect();
            let title: Vec<char> = " Commit message".chars().collect();
            if chars.len() < title.len() {
                continue;
            }
            for i in 0..=chars.len() - title.len() {
                if chars[i..i + title.len()] == title[..] {
                    let x = (0..i).rev().find(|&j| chars[j] == '╭')?;
                    return Some((x as u16, y as u16));
                }
            }
        }
        None
    }

    #[test]
    fn commit_modal_has_themed_chrome_and_placeholder() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.on_key(KeyCode::Char('c'));
        let theme = app.theme();
        let buf = render_buf(&app, 80, 24);
        let rows = commit_rows(&buf);
        let (tx, ty) = commit_modal_origin(&rows).expect("commit modal title/corner");
        assert_eq!(buf[(tx, ty)].symbol(), "╭", "origin must be the corner");
        assert_eq!(
            buf[(tx, ty)].fg,
            theme.border_focused,
            "modal border must use the focused theme color"
        );
        // "╭ Commit message" → C is two cells right of the corner.
        let title_x = tx + 2;
        assert_eq!(buf[(title_x, ty)].symbol(), "C");
        assert_eq!(buf[(title_x, ty)].fg, theme.border_focused);
        let s = screen(&app, 80, 24);
        assert!(
            s.contains("Type a message"),
            "empty-state placeholder missing:\n{s}"
        );
        assert!(
            !s.contains("(Ctrl+G generates)"),
            "title should not carry the permanent Ctrl+G hint:\n{s}"
        );
    }

    #[test]
    fn commit_modal_min_height_when_empty() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.on_key(KeyCode::Char('c'));
        let buf = render_buf(&app, 80, 24);
        let rows = commit_rows(&buf);
        let (tx, ty) = commit_modal_origin(&rows).expect("commit modal");
        let by = (ty as usize + 1..rows.len())
            .find(|&y| rows[y].chars().nth(tx as usize) == Some('╰'))
            .expect("modal bottom-left");
        let height = by - ty as usize + 1;
        assert!(
            height >= 5,
            "empty commit box must keep min height 5, got {height}"
        );
    }

    #[test]
    fn commit_modal_grows_vertically_with_long_message() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.on_key(KeyCode::Char('c'));
        // Far wider than the 60-cell modal: it wraps onto extra rows so
        // head and tail are visible at the same time (no horizontal scroll).
        for c in "commit-message-".chars().cycle().take(120) {
            app.on_key(KeyCode::Char(c));
        }
        let s = screen(&app, 80, 24);
        assert!(s.contains("Commit"), "modal title missing:\n{s}");
        assert!(s.contains("message-"), "tail of long draft missing:\n{s}");
        assert!(s.contains("commit-m"), "head and tail show together:\n{s}");
        assert!(
            s.matches("commit-message-").count() >= 3,
            "long message must wrap over several rows:\n{s}"
        );
    }

    #[test]
    fn commit_modal_renders_hard_breaks_on_separate_rows() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.on_key(KeyCode::Char('c'));
        for c in "subject".chars() {
            app.on_key(KeyCode::Char(c));
        }
        app.push_draft_char('\n');
        for c in "body line".chars() {
            app.on_key(KeyCode::Char(c));
        }
        let s = screen(&app, 80, 24);
        let subject_row = s
            .lines()
            .position(|l| l.contains("subject"))
            .expect("subject row");
        let body_row = s
            .lines()
            .position(|l| l.contains("body line"))
            .expect("body row");
        assert_eq!(
            body_row,
            subject_row + 1,
            "hard break must start a new row:\n{s}"
        );
    }

    #[test]
    fn renders_error_line() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Conflicted)]);
        app.on_key(KeyCode::Char('d'));
        let s = screen(&app, 40, 10);
        assert!(s.contains("conflicted"), "error missing:\n{s}");
    }

    fn sample_diff() -> git_tui_core::diff::FileDiff {
        use git_tui_core::diff::{DiffLine, Hunk, LineKind};
        git_tui_core::diff::FileDiff {
            path: "a.txt".into(),
            binary: false,
            hunks: vec![
                Hunk {
                    header: "@@ -1,3 +1,3 @@".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        DiffLine {
                            kind: LineKind::Context,
                            text: "same".into(),
                        },
                        DiffLine {
                            kind: LineKind::Del,
                            text: "hello world".into(),
                        },
                        DiffLine {
                            kind: LineKind::Add,
                            text: "hello WORLD".into(),
                        },
                    ],
                },
                Hunk {
                    header: "@@ -30,2 +30,2 @@".into(),
                    old_start: 30,
                    new_start: 30,
                    lines: vec![DiffLine {
                        kind: LineKind::Add,
                        text: "brand new".into(),
                    }],
                },
            ],
        }
    }

    fn render_buf(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Single-hunk diff small enough to fit the inline preview whole.
    fn mini_diff() -> git_tui_core::diff::FileDiff {
        use git_tui_core::diff::{DiffLine, Hunk, LineKind};
        git_tui_core::diff::FileDiff {
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
                        text: "hello world".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        text: "hello WORLD".into(),
                    },
                ],
            }],
        }
    }

    #[test]
    fn renders_diff_pane_with_hunks_and_changed_words() {
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(mini_diff(), false);
        let s = screen(&app, 70, 14);
        assert!(s.contains("a.txt"), "diff title missing:\n{s}");
        assert!(s.contains("@@ -1,2 +1,2 @@"), "hunk header missing:\n{s}");
        assert!(s.contains("WORLD"), "added line missing:\n{s}");
        assert!(s.contains("unstaged"), "staged label missing:\n{s}");
    }

    /// A diff with one very long changed line: the marker sits past the
    /// old clipping point but within the two-row wrap budget.
    /// `pad` is the marker's cell offset into the line.
    fn long_line_diff(pad: usize) -> git_tui_core::diff::FileDiff {
        use git_tui_core::diff::{DiffLine, Hunk, LineKind};
        git_tui_core::diff::FileDiff {
            path: "a.txt".into(),
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".into(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    DiffLine {
                        kind: LineKind::Del,
                        text: format!("{}VISIBLE{}", "x".repeat(pad), "y".repeat(100)),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        text: format!("{}VISIBLE{}", "x".repeat(pad), "z".repeat(100)),
                    },
                ],
            }],
        }
    }

    #[test]
    fn fullscreen_long_line_wraps_instead_of_clipping() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(long_line_diff(30), false);
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        let s = screen(&app, 70, 14);
        // Each half-pane fits ~28 content cells; "VISIBLE" starts at cell
        // 30, so clipping would hide it while wrapping shows it.
        assert!(s.contains("VISIBLE"), "wrapped tail missing:\n{s}");
        assert!(s.contains("│"), "divider missing:\n{s}");
    }

    #[test]
    fn preview_long_line_wraps_instead_of_clipping() {
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(long_line_diff(65), false);
        let s = screen(&app, 100, 32);
        // The inline preview fits ~61 content cells per row; "VISIBLE"
        // starts at cell 65, so clipping would hide it while the wrapped
        // continuation row shows it.
        assert!(s.contains("VISIBLE"), "wrapped tail missing:\n{s}");
    }

    #[test]
    fn diff_rows_pair_old_and_new_numbers() {
        use git_tui_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        let diff = FileDiff {
            path: "a.txt".into(),
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -10,3 +20,3 @@".into(),
                old_start: 10,
                new_start: 20,
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
                    DiffLine {
                        kind: LineKind::Add,
                        text: "extra".into(),
                    },
                ],
            }],
        };
        let rows = diff_rows(&diff);
        assert!(matches!(rows[0], DiffRow::Header { index: 0 }));
        // Context fills both sides with their own numbers.
        let DiffRow::Split { left, right } = &rows[1] else {
            panic!("expected split, got {:?}", rows[1]);
        };
        assert_eq!((left.no, right.no), (Some(10), Some(20)));
        assert_eq!(left.kind, SideKind::Context);
        // Del/add pair shares one row, old on the left, new on the right.
        let DiffRow::Split { left, right } = &rows[2] else {
            panic!("expected split, got {:?}", rows[2]);
        };
        assert_eq!((left.no, right.no), (Some(11), Some(21)));
        assert_eq!(left.kind, SideKind::Del);
        assert_eq!(right.kind, SideKind::Add);
        assert_eq!(
            left.segs
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>(),
            "old"
        );
        assert_eq!(
            right
                .segs
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>(),
            "new"
        );
        // Leftover add gets a blank left counterpart; numbering continues.
        let DiffRow::Split { left, right } = &rows[3] else {
            panic!("expected split, got {:?}", rows[3]);
        };
        assert_eq!((left.no, right.no), (None, Some(22)));
        assert_eq!(left.kind, SideKind::Blank);
        assert_eq!(right.kind, SideKind::Add);
    }

    #[test]
    fn hunk_start_row_counts_header_and_paired_rows() {
        let diff = sample_diff();
        let rows = diff_rows(&diff);
        assert_eq!(rows_hunk_start(&rows, 0), 0);
        // Hunk 0 = header + context row + one paired del/add row.
        assert_eq!(rows_hunk_start(&rows, 1), 3);
        // Unknown hunk falls back to the top.
        assert_eq!(rows_hunk_start(&rows, 9), 0);
    }

    #[test]
    fn side_by_side_renders_gutters_divider_and_single_add_wash() {
        use crate::config::Theme;
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(sample_diff(), false);
        // Fullscreen overlay: the side-by-side view.
        app.on_key(KeyCode::Enter);
        let s = screen(&app, 70, 14);
        // Old|new divider and both gutter numbers on the context row.
        assert!(s.contains("│"), "divider missing:\n{s}");
        assert!(s.contains("   1 "), "line numbers missing:\n{s}");
        assert!(s.contains("brand new"), "added line missing:\n{s}");
        // Additions carry one uniform light-green wash: even the changed
        // "WORLD" run keeps the line wash so syntax colors stay readable
        // (green is never painted twice). The shared "hello " prefix carries
        // each side's own wash (del red left, light green right).
        let buf = render_buf(&app, 70, 14);
        let theme = Theme::default_theme();
        let mut found_word = false;
        let mut hello_washes = std::collections::HashSet::new();
        for y in 0..buf.area.height {
            let cells: Vec<String> = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            for x in 0..cells.len().saturating_sub(5) {
                if cells[x..x + 5] == ["W", "O", "R", "L", "D"] {
                    found_word = true;
                    for i in 0..5 {
                        let cell = &buf[(x as u16 + i as u16, y)];
                        assert_eq!(
                            cell.bg,
                            theme.diff_add_bg,
                            "added run must carry the single add wash at ({}, {y})",
                            x + i as usize
                        );
                    }
                }
                if cells[x..x + 5] == ["h", "e", "l", "l", "o"] {
                    hello_washes.insert(buf[(x as u16, y)].bg);
                }
            }
        }
        assert!(found_word, "WORLD not rendered");
        assert!(
            hello_washes.contains(&theme.diff_del_bg),
            "old side missing del wash: {hello_washes:?}"
        );
        assert!(
            hello_washes.contains(&theme.diff_add_bg),
            "new side missing add wash: {hello_washes:?}"
        );
    }

    #[test]
    fn addition_lines_carry_single_light_wash() {
        use crate::config::Theme;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(mini_diff(), false);
        let buf = render_buf(&app, 70, 14);
        let theme = Theme::default_theme();
        // "WORLD" is a changed run on an added line, but additions are
        // painted once: every cell carries the light line wash (background
        // only, syntax foreground stays readable).
        // NOTE: compare per-cell (box-drawing borders are multi-byte, so
        // byte indices from String::find do not equal cell columns).
        let mut found = false;
        for y in 0..buf.area.height {
            let cells: Vec<String> = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            for x in 0..cells.len().saturating_sub(5) {
                if cells[x..x + 5] == ["W", "O", "R", "L", "D"] {
                    found = true;
                    for i in 0..5 {
                        let cell = &buf[(x as u16 + i as u16, y)];
                        assert_eq!(
                            cell.bg,
                            theme.diff_add_bg,
                            "added run must carry the single add wash at ({}, {y})",
                            x + i as usize
                        );
                    }
                }
            }
        }
        assert!(found, "WORLD not rendered");
    }

    #[test]
    fn selected_hunk_is_marked() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(sample_diff(), false);
        app.on_key(KeyCode::Enter);
        app.on_key(KeyCode::Char('J'));
        let s = screen(&app, 70, 16);
        assert!(
            s.contains("> @@ -30,2 +30,2 @@"),
            "selected hunk not marked:\n{s}"
        );
    }

    #[test]
    fn block_cursor_reverses_exactly_one_cell() {
        use crossterm::event::KeyCode;
        use ratatui::style::Modifier;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(mini_diff(), false);
        app.on_key(KeyCode::Enter);
        // Row 1 is the `same` context line; column 1 is its `a`.
        app.on_key(KeyCode::Char('j'));
        app.on_key(KeyCode::Char('l'));
        let buf = render_buf(&app, 70, 20);
        let mut reversed: Vec<(u16, u16)> = Vec::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].modifier.contains(Modifier::REVERSED) {
                    reversed.push((x, y));
                }
            }
        }
        assert_eq!(reversed.len(), 1, "block cursor is one cell: {reversed:?}");
        let (x, y) = reversed[0];
        assert_eq!(buf[(x, y)].symbol(), "a");
        // It sits inside a `same` run (the cursor line shows twice:
        // left and right panes; the block is on the new side).
        let line: String = (0..buf.area.width)
            .map(|xx| buf[(xx, y)].symbol().to_string())
            .collect();
        assert!(line.contains("same"), "block must sit on `same`:\n{line}");
    }

    #[test]
    fn block_cursor_sits_on_header_text_too() {
        use crossterm::event::KeyCode;
        use ratatui::style::Modifier;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(sample_diff(), false);
        app.on_key(KeyCode::Enter);
        // Cursor starts on header row 0; column 2 is inside `@@ -1,3 ...`.
        assert_eq!(app.cursor_row(), 0);
        app.on_key(KeyCode::Char('l'));
        app.on_key(KeyCode::Char('l'));
        assert_eq!(app.cursor_col(), 2);
        let buf = render_buf(&app, 70, 20);
        let mut reversed = 0;
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            if line.contains("@@ -1,3 +1,3 @@") {
                reversed += (0..buf.area.width)
                    .filter(|&x| buf[(x, y)].modifier.contains(Modifier::REVERSED))
                    .count();
            }
        }
        assert_eq!(reversed, 1, "header line must carry one block cell");
    }

    #[test]
    fn visual_linewise_washes_whole_rows() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(mini_diff(), false);
        app.on_key(KeyCode::Enter);
        // Anchor on the header, extend onto the `same` line, linewise.
        app.on_key(KeyCode::Char('V'));
        app.on_key(KeyCode::Char('j'));
        let buf = render_buf(&app, 70, 20);
        let theme = crate::config::Theme::default_theme();
        let mut same_cells = 0;
        let mut same_washed = 0;
        let mut world_washed = 0;
        for y in 0..buf.area.height {
            let cells: Vec<String> = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            for x in 0..cells.len().saturating_sub(4) {
                if cells[x..x + 4] == ["s", "a", "m", "e"] {
                    same_cells += 1;
                    if buf[(x as u16, y)].bg == theme.selection_bg {
                        same_washed += 1;
                    }
                }
                if x + 5 <= cells.len()
                    && cells[x..x + 5] == ["W", "O", "R", "L", "D"]
                    && buf[(x as u16, y)].bg == theme.selection_bg
                {
                    world_washed += 1;
                }
            }
        }
        assert!(same_cells > 0, "`same` not rendered");
        assert_eq!(
            same_washed, same_cells,
            "every `same` cell must carry the selection wash"
        );
        assert_eq!(world_washed, 0, "row outside the selection must not wash");
    }

    #[test]
    fn visual_charwise_washes_exact_columns() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(mini_diff(), false);
        app.on_key(KeyCode::Enter);
        // Anchor (1,1) on `same`, extend to column 2: selects `am`.
        app.on_key(KeyCode::Char('j'));
        app.on_key(KeyCode::Char('l'));
        app.on_key(KeyCode::Char('v'));
        app.on_key(KeyCode::Char('l'));
        let buf = render_buf(&app, 70, 20);
        let theme = crate::config::Theme::default_theme();
        // Right (new-side) pane holds the cursor side: `a`+`m` washed,
        // `s`+`e` plain. Left pane mirrors context text, washed the same.
        let mut washed: Vec<String> = Vec::new();
        let mut plain: Vec<String> = Vec::new();
        for y in 0..buf.area.height {
            let cells: Vec<String> = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            for x in 0..cells.len().saturating_sub(4) {
                if cells[x..x + 4] == ["s", "a", "m", "e"] {
                    for (i, ch) in ["s", "a", "m", "e"].iter().enumerate() {
                        if buf[(x as u16 + i as u16, y)].bg == theme.selection_bg {
                            washed.push(ch.to_string());
                        } else {
                            plain.push(ch.to_string());
                        }
                    }
                }
            }
        }
        assert!(
            washed.contains(&"a".to_string()),
            "no selected cells: {washed:?}"
        );
        assert!(
            washed.contains(&"m".to_string()),
            "no selected cells: {washed:?}"
        );
        assert!(
            !washed.contains(&"s".to_string()),
            "s must stay plain: {washed:?}"
        );
        assert!(
            !washed.contains(&"e".to_string()),
            "e must stay plain: {washed:?}"
        );
        assert!(plain.contains(&"s".to_string()));
        assert!(plain.contains(&"e".to_string()));
    }

    #[test]
    fn preview_visual_linewise_washes_rows() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(mini_diff(), false);
        app.on_key(KeyCode::Char('5'));
        // Anchor on the header, extend onto the `same` line, linewise.
        app.on_key(KeyCode::Char('V'));
        app.on_key(KeyCode::Char('j'));
        let buf = render_buf(&app, 100, 32);
        let theme = crate::config::Theme::default_theme();
        let mut found = 0;
        let mut washed = 0;
        for y in 0..buf.area.height {
            let cells: Vec<String> = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            for x in 0..cells.len().saturating_sub(4) {
                if cells[x..x + 4] == ["s", "a", "m", "e"] {
                    found += 1;
                    if buf[(x as u16, y)].bg == theme.selection_bg {
                        washed += 1;
                    }
                }
            }
        }
        assert!(found > 0, "`same` not rendered in preview");
        assert_eq!(washed, found, "preview selection must wash the row");
    }

    #[test]
    fn cursor_line_text_prefers_new_side() {
        use git_tui_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        let diff = FileDiff {
            path: "a.txt".into(),
            binary: false,
            hunks: vec![
                Hunk {
                    header: "@@ -1,1 +1,1 @@".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![DiffLine {
                        kind: LineKind::Context,
                        text: "same".into(),
                    }],
                },
                Hunk {
                    header: "@@ -30,2 +30,2 @@".into(),
                    old_start: 30,
                    new_start: 30,
                    lines: vec![DiffLine {
                        kind: LineKind::Add,
                        text: "brand new".into(),
                    }],
                },
            ],
        };
        // Header rows carry the hunk header, so h/l never goes dead there.
        assert_eq!(
            cursor_line_text(&diff, &DiffRow::Header { index: 1 }),
            "@@ -30,2 +30,2 @@"
        );
        // Modified pair: the cursor rides the added (new) line.
        let pair = DiffRow::Split {
            left: Side {
                no: Some(1),
                segs: vec![crate::words::WordSeg {
                    text: "old".into(),
                    changed: true,
                }],
                kind: SideKind::Del,
            },
            right: Side {
                no: Some(1),
                segs: vec![crate::words::WordSeg {
                    text: "new".into(),
                    changed: true,
                }],
                kind: SideKind::Add,
            },
        };
        assert_eq!(cursor_line_text(&diff, &pair), "new");
        // Deleted-only row: the cursor rides the deleted line.
        let del = DiffRow::Split {
            left: Side {
                no: Some(2),
                segs: vec![crate::words::WordSeg {
                    text: "gone".into(),
                    changed: false,
                }],
                kind: SideKind::Del,
            },
            right: Side {
                no: None,
                segs: Vec::new(),
                kind: SideKind::Blank,
            },
        };
        assert_eq!(cursor_line_text(&diff, &del), "gone");
    }

    #[test]
    fn expanded_col_counts_tabs_like_the_renderer() {
        // gutter 4: content starts at cell 5, tab stops every 8.
        assert_eq!(expanded_col("a\tb", 0, 4), 0);
        assert_eq!(expanded_col("a\tb", 1, 4), 1);
        // `a` fills cell 0; the tab jumps cells 1-2; `b` sits at cell 3.
        assert_eq!(expanded_col("a\tb", 2, 4), 3);
        assert_eq!(expanded_col("abcd", 3, 4), 3);
    }

    #[test]
    fn line_cursor_row_carries_selection_wash() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(mini_diff(), false);
        app.on_key(KeyCode::Enter);
        // Cursor row 1 is the `same` context line (header is row 0).
        app.on_key(KeyCode::Char('j'));
        assert_eq!(app.cursor_row(), 1);
        let buf = render_buf(&app, 70, 20);
        let theme = crate::config::Theme::default_theme();
        let mut found_cursor = false;
        let mut found_other = false;
        for y in 0..buf.area.height {
            let cells: Vec<String> = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            for x in 0..cells.len().saturating_sub(4) {
                if cells[x..x + 4] == ["s", "a", "m", "e"] {
                    found_cursor = true;
                    assert_eq!(
                        buf[(x as u16, y)].bg,
                        theme.selection_bg,
                        "cursor line must carry the selection wash"
                    );
                }
                if cells[x..x + 5] == ["W", "O", "R", "L", "D"] {
                    found_other = true;
                    assert_ne!(
                        buf[(x as u16, y)].bg,
                        theme.selection_bg,
                        "non-cursor lines must not carry the selection wash"
                    );
                }
            }
        }
        assert!(found_cursor, "cursor line not rendered");
        assert!(found_other, "other changed line not rendered");
    }

    #[test]
    fn esc_closes_fullscreen_back_to_files() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(sample_diff(), false);
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        let s = screen(&app, 100, 32);
        assert!(s.contains("Full diff"), "fullscreen title missing:\n{s}");
        assert!(s.contains("│"), "fullscreen divider missing:\n{s}");
        app.on_key(KeyCode::Esc);
        assert_eq!(app.mode(), Mode::Normal);
        let s = screen(&app, 100, 32);
        assert!(!s.contains("Full diff"), "overlay should be gone:\n{s}");
        assert!(s.contains("[1]-Files"), "rail should be back:\n{s}");
    }

    #[test]
    fn fullscreen_whole_file_view_has_file_title() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(mini_diff(), false);
        app.set_whole_file_for_test(true);
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        let s = screen(&app, 100, 32);
        assert!(s.contains("Full file"), "whole-file title missing:\n{s}");
        assert!(s.contains("a.txt"), "filename missing:\n{s}");
    }

    #[test]
    fn control_bytes_in_diff_text_never_reach_the_terminal() {
        use crossterm::event::KeyCode;
        use git_tui_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        let line = |kind, text: &str| DiffLine {
            kind,
            text: text.into(),
        };
        app.set_diff_for_test(
            FileDiff {
                path: "a.txt".into(),
                binary: false,
                hunks: vec![Hunk {
                    header: "@@ -1,1 +1,1 @@".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        line(LineKind::Del, "plain"),
                        line(LineKind::Add, "x\u{1b}[2Jy\u{0}z\u{7f}w\u{9b}"),
                    ],
                }],
            },
            false,
        );
        let check = |app: &App, view: &str| {
            let buf = render_buf(app, 100, 32);
            let mut all = String::new();
            for cell in buf.content() {
                all.push_str(cell.symbol());
            }
            assert!(
                !all.chars().any(|c| c.is_control()),
                "{view}: control char reached the buffer"
            );
            for pic in ['␛', '␀', '␡'] {
                assert!(all.contains(pic), "{view}: missing {pic}");
            }
        };
        check(&app, "inline");
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        check(&app, "fullscreen");
    }

    #[test]
    fn binary_diff_shows_notice_not_bytes() {
        use git_tui_core::diff::FileDiff;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_diff_for_test(
            FileDiff {
                path: "a.txt".into(),
                hunks: Vec::new(),
                binary: true,
            },
            false,
        );
        let s = screen(&app, 100, 32);
        assert!(s.contains("Binary file"), "notice missing:\n{s}");
        assert!(
            !s.contains("(no changes)"),
            "binary is not 'no changes':\n{s}"
        );
    }

    #[test]
    fn focus_ring_highlights_active_pane() {
        use crossterm::event::KeyCode;
        use ratatui::layout::Rect;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        let layout = compute_layout(Rect::new(0, 0, 100, 32), 1, LayoutOverrides::default());
        // Colors come from the active theme, not hard-coded legacy values.
        let (bright, dim) = (app.theme().border_focused, app.theme().border_unfocused);
        assert_ne!(bright, dim, "theme must distinguish focus");
        // Status focus drives the files list, so the files panel glows
        // while the status strip and diff preview stay dim.
        let buf = render_buf(&app, 100, 32);
        assert_eq!(buf[(layout.files.x, layout.files.y)].fg, bright);
        assert_eq!(buf[(layout.status.x, layout.status.y)].fg, dim);
        assert_eq!(buf[(layout.diff.x, layout.diff.y)].fg, dim);
        // Focus branches: branches corner goes bright, files goes dim.
        app.on_key(KeyCode::Tab);
        let buf = render_buf(&app, 100, 32);
        assert_eq!(buf[(layout.branches.x, layout.branches.y)].fg, bright);
        assert_eq!(buf[(layout.files.x, layout.files.y)].fg, dim);
    }

    /// End-to-end: a loaded theme (here tokyo-night) must reach the pixels,
    /// not just sit in the config struct.
    #[test]
    fn loaded_theme_reaches_the_screen() {
        use crate::config::{Config, KeyBindings, Theme};
        use crossterm::event::KeyCode;
        use ratatui::layout::Rect;
        let dir = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let config = Config {
            keys: KeyBindings::default(),
            theme: Theme::by_name("tokyo-night").unwrap(),
            ..Default::default()
        };
        let mut app = App::new_with_config(JobQueue::spawn(dir.path()).unwrap(), config);
        app.set_status_for_test(RepoStatus {
            branch: "main".into(),
            head_summary: "init".into(),
            files: vec![StatusEntry {
                path: "a.txt".into(),
                state: FileState::Unstaged,
            }],
            tracked_files: vec!["a.txt".into()],
        });
        // Tokyo-night focused border is blue #7aa2f7, unfocused #3b4261 —
        // neither equals the legacy White/DarkGray. Status focus drives
        // the files list, so the files panel glows first.
        let layout = compute_layout(Rect::new(0, 0, 100, 32), 1, LayoutOverrides::default());
        let buf = render_buf(&app, 100, 32);
        assert_eq!(
            buf[(layout.files.x, layout.files.y)].fg,
            Color::Rgb(122, 162, 247)
        );
        assert_eq!(
            buf[(layout.diff.x, layout.diff.y)].fg,
            Color::Rgb(59, 66, 97)
        );
        // Focusing branches flips that panel's border to the focused color.
        app.on_key(KeyCode::Tab);
        let buf = render_buf(&app, 100, 32);
        assert_eq!(
            buf[(layout.branches.x, layout.branches.y)].fg,
            Color::Rgb(122, 162, 247)
        );
    }

    fn with_branches() -> (tempfile::TempDir, App) {
        use git_tui_core::branch::BranchInfo;
        let (dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_branches_for_test(vec![
            BranchInfo {
                name: "feat".into(),
                is_head: false,
                tip_summary: "wip".into(),
            },
            BranchInfo {
                name: "main".into(),
                is_head: true,
                tip_summary: "init".into(),
            },
        ]);
        (dir, app)
    }

    #[test]
    fn renders_branches_pane_with_current_marked() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_branches();
        app.on_key(KeyCode::Char('2'));
        let s = screen(&app, 100, 32);
        assert!(s.contains("Local branches"), "pane title missing:\n{s}");
        assert!(s.contains("feat"), "branch missing:\n{s}");
        assert!(s.contains("main"), "branch missing:\n{s}");
        assert!(s.contains("wip"), "tip summary missing:\n{s}");
        assert!(s.contains('*'), "current marker missing:\n{s}");
    }

    #[test]
    fn renders_new_branch_modal() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_branches();
        app.on_key(KeyCode::Char('2'));
        app.on_key(KeyCode::Char('a'));
        app.on_key(KeyCode::Char('x'));
        let s = screen(&app, 70, 12);
        assert!(s.contains("New branch"), "modal title missing:\n{s}");
    }

    fn with_log() -> (tempfile::TempDir, App) {
        use git_tui_core::log::CommitInfo;
        let (dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_log_for_test(vec![
            CommitInfo {
                id: "abc1234".into(),
                oid: "abc1234full".into(),
                summary: "second".into(),
                author: "Test User".into(),
                parents: vec!["def5678full".into()],
                refs: vec!["HEAD -> main".into()],
                pushed: true,
            },
            CommitInfo {
                id: "def5678".into(),
                oid: "def5678full".into(),
                summary: "init".into(),
                author: "Test User".into(),
                parents: vec![],
                refs: vec![],
                pushed: true,
            },
        ]);
        (dir, app)
    }

    #[test]
    fn renders_log_pane_newest_first() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_log();
        app.on_key(KeyCode::Char('3'));
        // Wide frame: the 44-col rail fits the whole decorated line.
        let s = screen(&app, 150, 32);
        assert!(s.contains("Commits"), "pane title missing:\n{s}");
        assert!(s.contains("second"), "entry missing:\n{s}");
        assert!(s.contains("abc1234"), "short id missing:\n{s}");
        let newest = s.find("abc1234").unwrap();
        let older = s.find("def5678").unwrap();
        assert!(newest < older, "newest must come first:\n{s}");
    }

    /// Whether the row containing `needle` carries the selection wash
    /// (same scan pattern as `long_branch_list_scrolls_with_selection`).
    fn row_with_text_is_highlighted(
        buf: &ratatui::buffer::Buffer,
        theme: Theme,
        needle: &str,
    ) -> bool {
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            if line.contains(needle) && buf[(1, y)].bg == theme.selection_bg {
                return true;
            }
        }
        false
    }

    #[test]
    fn j_moves_the_commit_highlight_and_updates_the_count_label() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_log();
        app.on_key(KeyCode::Char('3'));
        let s = screen(&app, 150, 32);
        assert!(s.contains("(1 of 2)"), "count label missing:\n{s}");
        let theme = Theme::default_theme();
        let buf = render_buf(&app, 150, 32);
        assert!(
            row_with_text_is_highlighted(&buf, theme, "second"),
            "newest commit must carry the selection wash before any move"
        );
        app.on_key(KeyCode::Char('j'));
        let s = screen(&app, 150, 32);
        assert!(s.contains("(2 of 2)"), "count label must follow j:\n{s}");
        let buf = render_buf(&app, 150, 32);
        assert!(
            row_with_text_is_highlighted(&buf, theme, "init"),
            "older commit must carry the wash after j"
        );
        assert!(
            !row_with_text_is_highlighted(&buf, theme, "second"),
            "newest commit must drop the wash after j"
        );
    }

    #[test]
    fn commit_overview_panel_shows_message_author_and_file_stat() {
        use crossterm::event::KeyCode;
        use git_tui_core::log::{CommitFileStat, CommitOverview};
        let (_dir, mut app) = with_log();
        app.on_key(KeyCode::Char('3'));
        assert_eq!(app.focus(), Focus::Log);
        app.set_commit_overview_for_test(CommitOverview {
            id: "abc1234".into(),
            oid: "abc1234full".into(),
            author: "Test User".into(),
            email: "test@example.com".into(),
            date: "2024-01-01 00:00:00 +0000".into(),
            summary: "second".into(),
            body: "more detail".into(),
            parents: 1,
            files: vec![CommitFileStat {
                path: "a.txt".into(),
                insertions: 2,
                deletions: 1,
                status: 'M',
            }],
            insertions: 2,
            deletions: 1,
        });
        let s = screen(&app, 150, 32);
        assert!(s.contains("Commit abc1234"), "panel title missing:\n{s}");
        assert!(s.contains("Test User"), "author missing:\n{s}");
        assert!(s.contains("test@example.com"), "email missing:\n{s}");
        assert!(s.contains("more detail"), "body missing:\n{s}");
        assert!(s.contains("a.txt"), "changed file missing:\n{s}");
        assert!(
            s.contains("1 file changed, +2 -1"),
            "file stat summary missing:\n{s}"
        );
    }

    #[test]
    fn log_graph_renders_lanes_and_refs() {
        use crossterm::event::KeyCode;
        use git_tui_core::log::CommitInfo;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        let entries = vec![
            CommitInfo {
                id: "merge12".into(),
                oid: "merge-full".into(),
                summary: "merge side".into(),
                author: "Test User".into(),
                parents: vec!["main-full".into(), "side-full".into()],
                refs: vec!["HEAD -> main".into()],
                pushed: true,
            },
            CommitInfo {
                id: "main000".into(),
                oid: "main-full".into(),
                summary: "main work".into(),
                author: "Test User".into(),
                parents: vec!["base-full".into()],
                refs: vec![],
                pushed: true,
            },
            CommitInfo {
                id: "side000".into(),
                oid: "side-full".into(),
                summary: "side work".into(),
                author: "Test User".into(),
                parents: vec!["base-full".into()],
                refs: vec!["side".into()],
                pushed: true,
            },
        ];
        // Full text (no panel clipping): every marker present.
        let theme = crate::config::Theme::default_theme();
        let full: String = super::log_lines(&entries, theme)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full.contains("●"), "graph node missing:\n{full}");
        assert!(full.contains("─"), "merge join missing:\n{full}");
        assert!(full.contains("│"), "fork rail missing:\n{full}");
        assert!(full.contains("HEAD -> main"), "HEAD label missing:\n{full}");
        assert!(full.contains("side"), "branch label missing:\n{full}");
        // Reflog columns: author initials ride next to the id.
        assert!(full.contains("TU"), "author initials missing:\n{full}");
        // Lanes carry distinct colors: the fork rail's fg must differ
        // from the commit node's fg on the two-lane row.
        let lines = super::log_lines(&entries, theme);
        let forked = &lines[1];
        let node_fg = forked
            .spans
            .iter()
            .find(|s| s.content == "●")
            .map(|s| s.style.fg)
            .unwrap();
        let rail_fg = forked
            .spans
            .iter()
            .find(|s| s.content == "│")
            .map(|s| s.style.fg)
            .unwrap();
        assert_ne!(node_fg, rail_fg, "lanes must differ in color");
        app.set_log_for_test(entries);
        app.on_key(KeyCode::Char('3'));
        let s = screen(&app, 150, 32);
        assert!(s.contains("●"), "graph node missing:\n{s}");
        assert!(s.contains("─"), "merge join missing:\n{s}");
        assert!(s.contains("│"), "fork rail missing:\n{s}");
        assert!(s.contains("HEAD -> main"), "HEAD label missing:\n{s}");
    }

    #[test]
    fn log_graph_marks_unpushed_with_open_dot() {
        use git_tui_core::log::CommitInfo;
        let theme = crate::config::Theme::default_theme();
        let entries = vec![
            CommitInfo {
                id: "abc1234".into(),
                oid: "abc-full".into(),
                summary: "local work".into(),
                author: "Test User".into(),
                parents: vec!["def-full".into()],
                refs: vec!["HEAD -> main".into()],
                pushed: false,
            },
            CommitInfo {
                id: "def5678".into(),
                oid: "def-full".into(),
                summary: "init".into(),
                author: "Test User".into(),
                parents: vec![],
                refs: vec![],
                pushed: true,
            },
        ];
        let full: String = super::log_lines(&entries, theme)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full.contains("○"), "unpushed open dot missing:\n{full}");
        assert!(full.contains("●"), "pushed filled dot missing:\n{full}");
        let lines = super::log_lines(&entries, theme);
        let top: String = lines[0].spans.iter().map(|s| s.content.clone()).collect();
        let bottom: String = lines[1].spans.iter().map(|s| s.content.clone()).collect();
        assert!(top.contains("○"), "top (unpushed) must be open:\n{top}");
        assert!(
            !top.contains("●"),
            "top (unpushed) must not be filled:\n{top}"
        );
        assert!(
            bottom.contains("●"),
            "bottom (pushed) must be filled:\n{bottom}"
        );
    }

    #[test]
    fn log_graph_renders_tags_bare_and_initials() {
        use git_tui_core::log::CommitInfo;
        let theme = crate::config::Theme::default_theme();
        let entries = vec![CommitInfo {
            id: "d089546".into(),
            oid: "d089546full".into(),
            summary: "chore(release): bump version to 0.2.2".into(),
            author: "Vasani Devarsh".into(),
            parents: vec!["prev-full".into()],
            refs: vec!["HEAD -> main".into(), "tag: v0.2.2".into()],
            pushed: true,
        }];
        let full: String = super::log_lines(&entries, theme)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full.contains("VD"), "initials missing:\n{full}");
        assert!(full.contains("v0.2.2"), "tag missing:\n{full}");
        assert!(
            !full.contains("tag: v0.2.2"),
            "tag prefix must be bare:\n{full}"
        );
        assert_eq!(super::author_initials("Test User"), "TU");
        assert_eq!(super::author_initials("Aayush"), "AA");
    }

    fn with_stash() -> (tempfile::TempDir, App) {
        use git_tui_core::stash::StashEntry;
        let (dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.set_stash_for_test(vec![StashEntry {
            index: 0,
            message: "On main: wip".into(),
        }]);
        (dir, app)
    }

    #[test]
    fn renders_stash_pane_with_entries() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_stash();
        app.on_key(KeyCode::Char('4'));
        let s = screen(&app, 100, 32);
        assert!(s.contains("Stash"), "pane title missing:\n{s}");
        assert!(s.contains("stash@0"), "entry missing:\n{s}");
        assert!(s.contains("wip"), "message missing:\n{s}");
    }

    /// All-context diff: what a clean file's whole-file view loads.
    fn whole_file_sample() -> git_tui_core::diff::FileDiff {
        use git_tui_core::diff::{DiffLine, Hunk, LineKind};
        git_tui_core::diff::FileDiff {
            path: "main.rs".into(),
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -1,3 +1,3 @@".into(),
                old_start: 1,
                new_start: 1,
                lines: vec![
                    DiffLine {
                        kind: LineKind::Context,
                        text: "fn main() {".into(),
                    },
                    DiffLine {
                        kind: LineKind::Context,
                        text: "println!(\"hi\");".into(),
                    },
                    DiffLine {
                        kind: LineKind::Context,
                        text: "}".into(),
                    },
                ],
            }],
        }
    }

    #[test]
    fn whole_file_fullscreen_paints_each_line_once() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("main.rs", FileState::Clean)]);
        app.set_diff_for_test(whole_file_sample(), false);
        app.set_whole_file_for_test(true);
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        let s = screen(&app, 100, 32);
        // Single LazyVim buffer: the code line appears exactly once (never
        // mirrored into a second half-pane) and there is no side-by-side
        // divider.
        assert_eq!(
            s.matches("println!").count(),
            1,
            "whole-file line painted more than once:\n{s}"
        );
        // No side-by-side divider: interior cells (outside the rounded
        // panel borders) never contain the column separator.
        for line in s.lines() {
            let chars: Vec<char> = line.chars().collect();
            if chars.len() > 2 {
                let interior: String = chars[1..chars.len() - 1].iter().collect();
                assert!(
                    !interior.contains("\u{2502}"),
                    "single-pane file view must not have a divider:\n{s}"
                );
            }
        }
    }

    #[test]
    fn whole_file_fullscreen_has_opaque_background() {
        use crate::config::Theme;
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("main.rs", FileState::Clean)]);
        app.set_diff_for_test(whole_file_sample(), false);
        app.set_whole_file_for_test(true);
        app.on_key(KeyCode::Enter);
        let buf = render_buf(&app, 100, 32);
        let theme = Theme::default_theme();
        // Code row is fully opaque: every cell of the `println!` line sits
        // on the editor background (no wallpaper bleed-through).
        let mut found = false;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            if row.contains("println!") {
                found = true;
                for x in 0..buf.area.width {
                    assert_eq!(
                        buf[(x, y)].bg,
                        theme.bg,
                        "transparent cell at ({x}, {y}): {row:?}"
                    );
                }
            }
        }
        assert!(found, "println! line not rendered");
    }

    #[test]
    fn renders_finder_popup_with_filtered_matches() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[
            ("src/main.rs", FileState::Unstaged),
            ("src/app.rs", FileState::Staged),
        ]);
        app.on_key(KeyCode::Char('/'));
        for c in "main".chars() {
            app.on_key(KeyCode::Char(c));
        }
        let s = screen(&app, 100, 32);
        assert!(s.contains("Find files"), "finder title missing:\n{s}");
        assert!(s.contains("main.rs"), "match missing:\n{s}");
        assert!(s.contains("1 match"), "match count missing:\n{s}");
    }

    #[test]
    fn renders_finder_modal_over_fullscreen_diff() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[
            ("a.txt", FileState::Unstaged),
            ("b.txt", FileState::Unstaged),
        ]);
        app.on_key(KeyCode::Enter);
        assert_eq!(app.mode(), Mode::FullDiff);
        app.on_key(KeyCode::Char('/'));
        assert_eq!(app.mode(), Mode::FindFile);
        let s = screen(&app, 100, 32);
        assert!(s.contains("Find files"), "finder title missing:\n{s}");
        assert!(
            s.contains("Full diff"),
            "fullscreen backdrop missing behind finder:\n{s}"
        );
    }

    #[test]
    fn renders_stash_push_modal() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_stash();
        app.on_key(KeyCode::Char('4'));
        app.on_key(KeyCode::Char('a'));
        app.on_key(KeyCode::Char('w'));
        let s = screen(&app, 70, 12);
        assert!(s.contains("Stash message"), "modal title missing:\n{s}");
    }

    #[test]
    fn theme_picker_lists_themes_and_recolors_the_screen() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        let before = render_buf(&app, 100, 32);
        app.on_key(KeyCode::Char('T'));
        let s = screen(&app, 100, 32);
        for name in Theme::names() {
            assert!(
                s.contains(name),
                "{name} missing from the picker:
{s}"
            );
        }
        assert!(
            s.contains("> default"),
            "active theme not selected:
{s}"
        );
        // Moving the selection repaints everything behind the popup too.
        app.on_key(KeyCode::Down);
        let after = render_buf(&app, 100, 32);
        assert_eq!(before[(0, 0)].bg, Theme::default_theme().bg);
        assert_eq!(after[(0, 0)].bg, Theme::by_name("tokyo-night").unwrap().bg);
        assert!(screen(&app, 100, 32).contains("> tokyo-night"));
    }

    #[test]
    fn action_menu_lists_grouped_actions_with_their_keys() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.on_key(KeyCode::Char('?'));
        let s = screen(&app, 110, 50);
        assert!(s.contains("Actions"), "menu title missing:\n{s}");
        for text in ["Sync", "Settings", "Stage / unstage selected", "Theme"] {
            assert!(s.contains(text), "{text} missing:\n{s}");
        }
        // The key column: `space` stages.
        let stage = s
            .lines()
            .find(|l| l.contains("Stage / unstage selected"))
            .unwrap();
        assert!(stage.contains("space"), "key hint missing: {stage}");
    }

    #[test]
    fn confirm_box_names_the_target_and_has_buttons() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.on_key(KeyCode::Char('d'));
        let s = screen(&app, 110, 40);
        assert!(s.contains("Confirm"), "title missing:\n{s}");
        assert!(
            s.contains("Discard all changes to a.txt?"),
            "question missing:\n{s}"
        );
        assert!(
            s.contains("[Yes]") && s.contains("[No]"),
            "buttons missing:\n{s}"
        );
    }

    #[test]
    fn popups_carry_clickable_buttons() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.on_key(KeyCode::Char('c'));
        let s = screen(&app, 110, 40);
        for button in ["[Commit]", "[Generate]", "[Close]"] {
            assert!(s.contains(button), "{button} missing:\n{s}");
        }
        app.on_key(KeyCode::Esc);
        app.on_key(KeyCode::Char('T'));
        let s = screen(&app, 110, 40);
        assert!(s.contains("[Keep]") && s.contains("[Cancel]"), "{s}");
    }

    #[test]
    fn long_non_ascii_paths_are_cut_on_character_boundaries() {
        // Byte slicing here used to panic mid-character.
        let path = "C:/Users/Zoë/проекты/日本語のフォルダ/very/long/path/that/does/not/fit";
        for width in 1..40 {
            let cut = tail_fit(path, width);
            assert!(cut.width() <= width.max(1), "{cut:?} wider than {width}");
        }
        assert_eq!(tail_fit("short", 10), "short");
    }

    #[test]
    fn action_menu_fits_a_short_terminal_and_follows_the_selection() {
        use crossterm::event::KeyCode;
        let (_dir, mut app) = with_files(&[("a.txt", FileState::Unstaged)]);
        app.on_key(KeyCode::Char('?'));
        // Far fewer rows than entries: wrap up to the last entry.
        app.on_key(KeyCode::Up);
        let s = screen(&app, 100, 16);
        assert!(s.contains("> Quit"), "last entry not visible:\n{s}");
    }

    #[test]
    fn help_screen_lists_keys_in_the_users_bindings() {
        use crossterm::event::KeyCode;
        let dir = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let mut config = crate::config::Config::default();
        config.keys.commit = vec![KeyCode::Char('C')];
        let mut app = App::new_with_config(JobQueue::spawn(dir.path()).unwrap(), config);
        app.on_key(KeyCode::F(1));
        let s = screen(&app, 120, 200);
        assert!(s.contains("Keys"), "title missing:\n{s}");
        for heading in ["Everywhere", "Files", "Text boxes", "Mouse"] {
            assert!(s.contains(heading), "{heading} missing:\n{s}");
        }
        let commit = s.lines().find(|l| l.contains("commit box")).unwrap();
        assert!(commit.contains(" C "), "rebound key not shown: {commit}");
        // On a short terminal it scrolls instead of being cut off.
        let s = screen(&app, 120, 20);
        assert!(s.contains("of "), "no scroll position in the title:\n{s}");
        assert!(s.contains("[Close]"));
    }

    #[test]
    fn every_bound_action_appears_on_the_help_screen() {
        let keys = crate::config::KeyBindings::default();
        let text: String = crate::menu::help_sections(&keys)
            .iter()
            .flat_map(|(_, rows)| rows.iter().map(|(key, what)| format!("{key} {what}\n")))
            .collect();
        // One spot check per group of bindings, so a new action cannot be
        // added without a line of help.
        for key in [
            "?", "F1", "space", "d", "c", "p", "P", "f", "u", "T", "A", "o", "q", "Q", "/",
        ] {
            assert!(
                text.lines().any(|l| l.split_whitespace().any(|w| w == key)),
                "{key} missing from help"
            );
        }
    }
    #[test]
    fn start_screen_lists_recent_projects_and_drives() {
        let dir = tempfile::TempDir::new().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let mut start = crate::start::StartScreen::new(vec![dir.path().to_path_buf()]);
        let draw = |start: &mut crate::start::StartScreen| {
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal
                .draw(|f| render_start(f, start, Theme::default_theme()))
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            let mut out = String::new();
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    out.push_str(buf[(x, y)].symbol());
                }
                out.push('\n');
            }
            out
        };
        let text = draw(&mut start);
        assert!(text.contains("ActiveGit"), "{text}");
        assert!(text.contains(PLACES_TITLE), "{text}");
        assert!(text.contains("[repo]"), "{text}");
        assert!(text.contains("recent project"), "{text}");
        assert!(text.contains("home folder"), "{text}");
        assert!(text.contains("esc: quit"), "{text}");
        // One clickable row per place, the first under the heading.
        assert_eq!(start.hits.len(), start.browser.view.len());
        assert_eq!(start.hits[0], (5, 0));
        // Inside a folder the usual `.` and `..` rows are back.
        let root = std::path::PathBuf::from(&start.browser.view[0].name);
        start.browser.goto(root);
        let text = draw(&mut start);
        assert!(text.contains("(open this folder)"), "{text}");
        assert!(text.contains("../"), "{text}");
        // Too small to draw: no panic, nothing clickable.
        let mut tiny = Terminal::new(TestBackend::new(20, 4)).unwrap();
        tiny.draw(|f| render_start(f, &mut start, Theme::default_theme()))
            .unwrap();
        assert!(start.hits.is_empty());
    }
}
