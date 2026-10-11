# Changelog

All notable changes to ActiveGit are documented here. Release notes are
built from these entries by cargo-dist.

## Unreleased

### Safety

- **Confirm before anything is thrown away**: discard (file or folder),
  restore hunk, delete branch, drop stash, force push and abort merge go
  through one confirm box that names the target. It applies to the keys
  (`d`, `x`, `D`) as well as the buttons and menu. `[ui]
  confirm_destructive = false` turns it off.
- **Undo** (`u`): puts back the last discard, untracked files included.
- **Ctrl/Alt are no longer ignored**: `ctrl+d` used to discard, `ctrl+x`
  to restore a hunk, `ctrl+p` to pull. Modified keys are now their own
  shortcuts (see below) or do nothing.
- **Paste is text**: a multi-line paste into the commit box used to commit
  at the first line break and run the rest as commands. Pastes now only
  ever fill the open text box, on terminals with and without bracketed
  paste.
- **The commit message survives**: closing the box keeps it, and a failed
  commit reopens the box with it. A generated message no longer overwrites
  text typed while it was generating.
- **Capital `A` can be typed in the commit box.** Message generation
  moved from `Shift+A` to `Ctrl+G`.

### Fixes

- **Branch checkout** changed the branch name but left the previous
  branch's files in place when the two had diverged.
- **Staging or restoring a single hunk** rewrote the whole file: CRLF
  line endings became LF and non-UTF-8 bytes were replaced.
- **Commits** now run through `git commit`, so pre-commit / commit-msg
  hooks run, `commit.gpgsign` is honored, and committing during a merge
  makes a merge commit.
- **Discarding `a[1].txt` also restored `a1.txt`** (and similar glob
  matches); paths are now matched literally.
- **A conflicted pull** showed no useful message; it now lists the
  conflicts, the merge state is shown, and it can be continued or aborted.
- **ssh**: `core.sshCommand` and `GIT_SSH` are respected (they were
  overridden, which broke the Windows OpenSSH agent and per-repo keys).
- **Push** goes to the tracked branch even when it has another name, and
  is refused on a detached HEAD instead of pushing `(detached HEAD)`.
- Staging a deleted file, staged renames, `q` in the fullscreen diff
  (closes the diff, not the project), a crash on long non-ASCII paths in
  the project browser, and errors being wiped before they could be read.

### Mouse

- The wheel scrolls what is under the pointer (diffs, lists, menus)
  without changing focus. Double-click opens; one click only selects.
- Right-click opens the actions menu for what is under the pointer.
- Drag in a diff to select lines, then `y` or `[Copy]`.
- Every popup has clickable buttons and closes on a click outside.
- Click a folder's arrow to fold it. `[ui] mouse = false` gives the mouse
  back to the terminal.

### Keyboard

- `g` / `G` first / last; `ctrl+d` / `ctrl+u` half page; PgUp / PgDn page
  by what is on screen; `shift+tab` cycles panels backwards.
- `ctrl+f` searches the open diff, `n` / `N` step through matches.
- `]` / `[` in the fullscreen diff step through files.
- `F1`: every key on one screen, in your own bindings.
- Text boxes: `ctrl+w`, `ctrl+u`, `ctrl+k`, `ctrl+a`, `ctrl+e`, word
  movement, `alt+enter` for a new line in the commit box.
- Staging a hunk keeps your place instead of jumping to the top.

### Launching

- **App icon**: `activegit.exe` has its own icon and name.
- **`activegit --shortcut`** (Windows) adds ActiveGit to the Start menu and
  the desktop, ready to pin to the taskbar: click it instead of opening a
  terminal and typing.
- Started outside a repository with no previous session, it asks which
  repository to open (and offers `git init`) instead of exiting with an
  error.

### Features

- **Fetch** (`f`), **amend last commit** and **force push with lease**
  (menu).
- **Open a commit**: `enter` on a commit shows what it changed, file by
  file. `L` loads more history.
- **Staged side** (`S`): view what is already staged in a partly staged
  file.
- **Actions menu** (`?` / `[Menu]`), also from the fullscreen diff.
- **Themes**: 11 new ones, and a live picker (`T`).
- Push, pull and LLM calls no longer block status, diffs and staging.
  Returning to the terminal, switching tabs and `r` refresh everything.
- Saving settings from the app keeps the comments in your config file.

## v0.2.3 - 2026-10-06

### Commits panel

- **Branch-graph rendering**: the `[3]-Commits` panel now draws a
  `git log --graph` style lane view with per-branch colors, author
  initials, merge joins, and bare tag names.
- **Push-state dots**: each commit's dot is filled (`●`) when the commit
  exists on the remote and open (`○`) while it is still local-only
  (yet to push) — shared ancestors on untracked branches stay filled.
- **Commit overview**: focusing the Commits panel shows the selected
  commit's message, author, date, and per-file change stats in the
  `[5]-Commit` panel.

### Layout

- **Focused section auto-expands**: the panel you are in automatically
  grows (rail panels gain rows from roomier siblings, the diff preview
  gains columns), so the active section always has the most room.
  Divider drags still apply first — focus only redistributes the rest.

## v0.2.2 - 2026-10-04

### Diff workflow

- **Restore a single hunk**: `x` reverts just the hunk under the cursor
  while leaving every other hunk untouched — workdir hunks revert toward
  the index, staged hunks revert toward HEAD.
- **Hunk navigation in the preview**: `J`/`K` snap the cursor to the next /
  previous hunk top in the right-side diff preview, not just fullscreen.
- **Active-hunk highlight**: the hunk under the cursor reads as one block
  (bold, diff washes preserved) in both the preview and fullscreen views.

### Mouse support

- **Resizable sections**: drag the rail/preview divider to resize the left
  rail, or drag the dividers between status/files/branches/commits/stash to
  resize each panel.
- **Clickable buttons**: context-aware footer buttons (`Stage`, `Discard`,
  `Commit`, `Pull`, `Push`, `Stage hunk`, `Restore hunk`, …) plus a button
  bar inside the fullscreen diff.
- Click files, branches, stashes, diff rows, hunk headers, finder matches,
  and project tabs; the wheel scrolls the panel under the cursor.

### Safer sync

- **Push confirmation**: `P` with a tracked upstream now shows
  `main → origin/main` in a confirm box — `Enter` pushes, `Esc` cancels
  (clickable `[Push]` / `[Cancel]` included).

### LLM setup

- The setup form (`A`) asks the provider for its **live model list**
  instead of shipping hardcoded models, and refetches whenever the
  provider, API key, or base URL changes. A typed custom model id is kept
  when the provider still offers it.

### Docs

- New README comparison section (activegit vs lazygit vs px0) with measured
  startup, binary-size, and hot-path numbers, plus `docs/compare.html`.
