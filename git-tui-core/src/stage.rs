//! Staging operations.

use crate::diff::LineKind;
use crate::error::GitError;
use std::io::Write;
use std::path::Path;

/// Stage a whole file (workdir -> index). A tracked file that is gone
/// from the working tree has its deletion staged, and a conflicted path
/// is marked resolved with the working-tree content (like `git add`).
pub fn stage_file(repo: &git2::Repository, path: &str) -> Result<(), GitError> {
    let rel = Path::new(path);
    let mut index = fresh_index(repo)?;
    if workdir_file_missing(repo, path) && index_knows_path(&index, rel) {
        // Deleted in the working tree: `add_path` would fail on the
        // missing file, so drop the entry instead (stages the deletion).
        index
            .remove_path(rel)
            .map_err(|e| GitError::HunkStaging(format!("cannot stage file {path}: {e}")))?;
    } else {
        index
            .add_path(rel)
            .map_err(|e| GitError::HunkStaging(format!("cannot stage file {path}: {e}")))?;
    }
    index.write()?;
    Ok(())
}

/// The repository index, re-read when it changed on disk. Commits, pulls
/// and merges run through the `git` CLI, which rewrites the index file
/// behind libgit2's cached copy.
fn fresh_index(repo: &git2::Repository) -> Result<git2::Index, GitError> {
    let mut index = repo.index()?;
    index.read(false)?;
    Ok(index)
}

/// Whether `path` is absent from the working tree. A dangling symlink
/// counts as present; any error other than "not found" does too, so the
/// caller's real operation reports it.
fn workdir_file_missing(repo: &git2::Repository, path: &str) -> bool {
    let Some(workdir) = repo.workdir() else {
        return false;
    };
    matches!(
        std::fs::symlink_metadata(workdir.join(Path::new(path))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// Whether the index has `path` at any stage (0 = merged, 1-3 = the
/// sides of a conflict).
fn index_knows_path(index: &git2::Index, rel: &Path) -> bool {
    (0..=3).any(|stage| index.get_path(rel, stage).is_some())
}

/// `path` as a libgit2 pathspec that matches that path and nothing else.
///
/// Reset and checkout take glob patterns, not paths: unescaped,
/// `a[1].txt` also selects `a1.txt`, and `#notes.txt` is read as a
/// comment and dropped, which selects everything starting with that name
/// (`#notes.txt.bak`). For a forced checkout that means discarding
/// unrelated work, so every character the pattern syntax gives a meaning
/// to is neutralised.
///
/// One-character classes (`[[]`) do the escaping rather than backslashes
/// wherever possible: on Windows git2 turns every `\` in a pathspec into
/// `/` before libgit2 sees it.
fn literal_pathspec(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    for (i, c) in path.chars().enumerate() {
        match c {
            // Wildcards, and blanks (trimmed when they end the pattern).
            '[' | '*' | '?' | ' ' | '\t' | '\r' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            // Leading `#` makes the pattern a comment.
            '#' if i == 0 => out.push_str("[#]"),
            // Leading `!` negates the pattern, and `[!]` would open a
            // negated class. No repo path starts with `/`, so this class
            // can only ever match the `!`.
            '!' if i == 0 => out.push_str("[/!]"),
            // The rest cannot occur in a Windows file name, so the
            // backslash is safe where these are reachable.
            '\\' => out.push_str("[\\\\]"),
            '\n' | '\x0b' | '\x0c' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Unstage a whole file (index -> HEAD). New files are removed from the index.
pub fn unstage_file(repo: &git2::Repository, path: &str) -> Result<(), GitError> {
    // If HEAD exists, reset this path in the index to HEAD.
    if let Ok(head) = repo.head() {
        if let Ok(target) = head.peel_to_commit() {
            let obj = target.as_object().clone();
            // `reset_default` edits libgit2's cached index: sync it first.
            fresh_index(repo)?;
            repo.reset_default(Some(&obj), [literal_pathspec(path)])?;
            // reset_default updates the index on disk; ensure it is written.
            return Ok(());
        }
    }
    // No HEAD (or unborn): drop the path from the index if present.
    let mut index = repo.index()?;
    // remove_path errors if absent; treat as success (already unstaged).
    let _ = index.remove_path(Path::new(path));
    index.write()?;
    Ok(())
}

/// Discard all changes in a file (staged and unstaged), restoring it to
/// HEAD. Untracked files are deleted from the workdir. Staged-new files
/// (added but not in HEAD) are unindexed and deleted. Clean files and
/// conflicted files are errors.
pub fn discard_file(repo: &git2::Repository, path: &str) -> Result<(), GitError> {
    use git2::Status as S;
    let rel = Path::new(path);
    let flags = repo
        .status_file(rel)
        .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
    if flags.contains(S::CONFLICTED) {
        return Err(GitError::Discard(format!(
            "conflicted: resolve markers in {path} first"
        )));
    }
    let index_bits = S::INDEX_NEW
        | S::INDEX_MODIFIED
        | S::INDEX_DELETED
        | S::INDEX_RENAMED
        | S::INDEX_TYPECHANGE;
    let workdir_bits = S::WT_MODIFIED | S::WT_DELETED | S::WT_RENAMED | S::WT_TYPECHANGE;
    let staged = flags.intersects(index_bits);
    let unstaged = flags.intersects(workdir_bits);
    let wt_new = flags.contains(S::WT_NEW);

    // Untracked: only WT_NEW, nothing in the index. Deleting restores a
    // clean tree; resetting/checkout would be a no-op.
    if !staged && !unstaged && wt_new {
        return remove_workdir_path(repo, path);
    }
    if !staged && !unstaged {
        return Err(GitError::Discard(format!(
            "{path} is unchanged — nothing to discard"
        )));
    }

    match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
        Some(commit) => {
            let in_head = commit
                .tree()
                .ok()
                .and_then(|t| t.get_path(rel).ok())
                .is_some();
            let obj = commit.as_object().clone();
            let only_this = literal_pathspec(path);
            fresh_index(repo)?;
            repo.reset_default(Some(&obj), [only_this.as_str()])
                .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
            if in_head {
                // Tracked in HEAD: restore the workdir copy (also brings
                // back files deleted from the workdir).
                let mut builder = git2::build::CheckoutBuilder::new();
                builder.force().path(only_this.as_str());
                repo.checkout_head(Some(&mut builder))
                    .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
            } else {
                // Added (staged-new): unstage left it untracked, so delete
                // the workdir copy to actually discard it.
                remove_workdir_path(repo, path)?;
            }
            Ok(())
        }
        None => {
            // Unborn HEAD: everything is new, so drop the index entry and
            // delete the workdir copy.
            let mut index = repo.index()?;
            let _ = index.remove_path(rel);
            index.write()?;
            remove_workdir_path(repo, path)?;
            Ok(())
        }
    }
}

/// Delete `path` from the workdir (file or directory). Missing paths are
/// success (already discarded).
fn remove_workdir_path(repo: &git2::Repository, path: &str) -> Result<(), GitError> {
    let Some(workdir) = repo.workdir() else {
        return Err(GitError::Discard("bare repo".into()));
    };
    let full = workdir.join(Path::new(path));
    if !full.exists() && !full.is_symlink() {
        return Ok(());
    }
    if full.is_dir() && !full.is_symlink() {
        std::fs::remove_dir_all(&full)
            .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
    } else {
        std::fs::remove_file(&full)
            .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
    }
    Ok(())
}

/// Stage a single hunk (by index into the unstaged diff) via partial-index
/// application: only that hunk's changes are written to the index.
///
/// The splice works on bytes: the index blob's lines outside the hunk are
/// kept exactly as they are (terminators, non-UTF-8 bytes, final-newline
/// state), and the hunk's new side is copied from the working-tree file
/// in the form git stores it (after `core.autocrlf` / `.gitattributes`
/// clean filters), so its lines keep the terminators they really have.
pub fn stage_hunk(repo: &git2::Repository, path: &str, hunk_index: usize) -> Result<(), GitError> {
    let diff = crate::diff::unstaged_diff(repo, path)?;
    if hunk_index >= diff.hunks.len() {
        return Err(GitError::HunkStaging(format!(
            "hunk index {hunk_index} out of range ({} hunks in {path})",
            diff.hunks.len()
        )));
    }
    let hunk = &diff.hunks[hunk_index];
    let (old_start, old_lines, new_start, new_lines) = parse_hunk_header_full(&hunk.header)
        .ok_or_else(|| {
            GitError::HunkStaging(format!("cannot parse hunk header: {}", hunk.header))
        })?;

    let rel = Path::new(path);
    let full = repo
        .workdir()
        .ok_or_else(|| GitError::HunkStaging("bare repo".into()))?
        .join(rel);
    let mut index = fresh_index(repo)?;

    let meta = match std::fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The file is gone, so its one hunk removes every line: stage
            // the deletion rather than an empty blob.
            index
                .remove_path(rel)
                .map_err(|e| GitError::HunkStaging(format!("cannot stage {path}: {e}")))?;
            index.write()?;
            return Ok(());
        }
        Err(e) => return Err(GitError::HunkStaging(format!("cannot read {path}: {e}"))),
    };
    if meta.file_type().is_symlink() {
        // Reading through the link would stage the target's content as
        // the link itself.
        return Err(GitError::HunkStaging(format!(
            "{path} is a symlink: stage the whole file instead"
        )));
    }

    let raw = std::fs::read(&full)
        .map_err(|e| GitError::HunkStaging(format!("cannot read {path}: {e}")))?;
    // Old side: the index blob. New side: the working-tree file as stored.
    let old_side = stage0_blob(repo, &index, rel)?;
    let new_side = to_odb_form(repo, path, &raw).map_err(GitError::HunkStaging)?;

    let stale = || {
        GitError::HunkStaging(format!(
            "{path} changed while staging hunk {hunk_index}: reload and retry"
        ))
    };
    let segment =
        take_lines(&new_side, line_index(new_start, new_lines), new_lines).ok_or_else(stale)?;
    let staged = splice_lines(
        &old_side,
        line_index(old_start, old_lines),
        old_lines,
        segment,
    )
    .ok_or_else(stale)?;

    write_index_content(&mut index, path, &staged, new_entry_mode(&meta))
        .map_err(|e| GitError::HunkStaging(format!("cannot write partial hunk to index: {e}")))?;
    Ok(())
}

/// Content of `rel`'s merged (stage 0) index entry; empty when the path
/// is not in the index.
fn stage0_blob(
    repo: &git2::Repository,
    index: &git2::Index,
    rel: &Path,
) -> Result<Vec<u8>, GitError> {
    match index.get_path(rel, 0) {
        Some(entry) => Ok(repo.find_blob(entry.id)?.content().to_vec()),
        None => Ok(Vec::new()),
    }
}

/// `raw` working-tree bytes of `path` in the form git stores them: run
/// through the clean filters (`core.autocrlf`, `.gitattributes` `text` /
/// `eol`, …). Equal to `raw` when no filter applies. This is the content
/// the new side of an index-to-workdir diff is numbered against.
///
/// The filtered content is written to the object database as a loose
/// blob, exactly as `git add` would have done.
fn to_odb_form(repo: &git2::Repository, path: &str, raw: &[u8]) -> Result<Vec<u8>, String> {
    let failed = |e: &dyn std::fmt::Display| format!("cannot filter {path}: {e}");
    let mut writer = repo
        .blob_writer(Some(Path::new(path)))
        .map_err(|e| failed(&e))?;
    writer.write_all(raw).map_err(|e| failed(&e))?;
    let oid = writer.commit().map_err(|e| failed(&e))?;
    let blob = repo.find_blob(oid).map_err(|e| failed(&e))?;
    Ok(blob.content().to_vec())
}

/// Mode for a path entering the index for the first time: executable
/// when the working-tree file is (Unix only; Windows has no such bit).
fn new_entry_mode(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return 0o100755;
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    0o100644
}

/// A stat-less index entry for `path`; git re-stats it on first use.
fn blank_entry(path: &str, id: git2::Oid, mode: u32, size: usize) -> git2::IndexEntry {
    git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        file_size: size as u32,
        id,
        flags: 0,
        flags_extended: 0,
        path: path.as_bytes().to_vec(),
    }
}

/// Store `data` as `path`'s index content and write the index. An
/// existing entry keeps its mode; a new one gets `new_mode`.
fn write_index_content(
    index: &mut git2::Index,
    path: &str,
    data: &[u8],
    new_mode: u32,
) -> Result<(), git2::Error> {
    let entry = match index.get_path(Path::new(path), 0) {
        Some(e) => e,
        None => blank_entry(path, git2::Oid::zero(), new_mode, data.len()),
    };
    index.add_frombuffer(&entry, data)?;
    index.write()
}

/// Byte offset where 0-based line `n` starts in `data`, or `None` when
/// `data` has fewer than `n` line breaks. A line ends at its `\n`; a `\r`
/// before it is part of the line, as is a missing final newline.
fn line_offset(data: &[u8], n: usize) -> Option<usize> {
    if n == 0 {
        return Some(0);
    }
    data.iter()
        .enumerate()
        .filter(|(_, &b)| b == b'\n')
        .nth(n - 1)
        .map(|(i, _)| i + 1)
}

/// Byte range of `count` lines of `data` starting at 0-based line
/// `start`, terminators included. `None` when `data` does not have those
/// lines (the diff the numbers came from no longer describes it). The
/// last line may lack its newline.
fn line_span(data: &[u8], start: usize, count: usize) -> Option<std::ops::Range<usize>> {
    if count == 0 {
        // A pure insertion point: past-the-end clamps to the end.
        let at = line_offset(data, start).unwrap_or(data.len());
        return Some(at..at);
    }
    let from = line_offset(data, start)?;
    let rest = &data[from..];
    let to = match line_offset(rest, count) {
        Some(len) => from + len,
        // Short by exactly the final line's missing newline is fine.
        None if line_offset(rest, count - 1).is_some_and(|o| o < rest.len()) => data.len(),
        None => return None,
    };
    Some(from..to)
}

/// The bytes of `count` lines of `data` from 0-based line `start`.
fn take_lines(data: &[u8], start: usize, count: usize) -> Option<&[u8]> {
    line_span(data, start, count).map(|span| &data[span])
}

/// `target` with `count` lines from 0-based line `start` replaced by
/// `replacement`. Every byte outside that range is copied untouched.
fn splice_lines(target: &[u8], start: usize, count: usize, replacement: &[u8]) -> Option<Vec<u8>> {
    let span = line_span(target, start, count)?;
    let mut out = Vec::with_capacity(target.len() - span.len() + replacement.len());
    out.extend_from_slice(&target[..span.start]);
    out.extend_from_slice(replacement);
    out.extend_from_slice(&target[span.end..]);
    Some(out)
}

/// 0-based line index for one side of a hunk header. With a zero line
/// count the header's start names the line *after which* the change
/// sits, so it is already the index of the insertion point.
fn line_index(start: usize, lines: usize) -> usize {
    if lines == 0 {
        start
    } else {
        start.saturating_sub(1)
    }
}

/// Full hunk header parse: `(old_start, old_lines, new_start, new_lines)`.
/// A missing `,N` means 1 (git convention for single-line hunks).
fn parse_hunk_header_full(header: &str) -> Option<(usize, usize, usize, usize)> {
    let header = header.trim();
    let inner = header.strip_prefix("@@")?.split("@@").next()?.trim();
    let mut parts = inner.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let parse_side = |s: &str| -> Option<(usize, usize)> {
        match s.split_once(',') {
            Some((start, lines)) => Some((start.parse().ok()?, lines.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = parse_side(old)?;
    let (new_start, new_lines) = parse_side(new)?;
    Some((old_start, old_lines, new_start, new_lines))
}

/// `path`'s entry in the HEAD tree as `(object id, file mode)`, or `None`
/// when HEAD does not have it (or there is no HEAD yet).
fn head_entry(repo: &git2::Repository, path: &str) -> Option<(git2::Oid, u32)> {
    let entry = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .and_then(|c| c.tree().ok())
        .and_then(|t| t.get_path(Path::new(path)).ok())?;
    Some((entry.id(), entry.filemode() as u32))
}

/// Number of `\r\n` line endings in `data`.
fn crlf_count(data: &[u8]) -> usize {
    data.windows(2).filter(|w| w == b"\r\n").count()
}

/// `data` with every bare `\n` turned into `\r\n`.
fn lf_to_crlf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 16);
    let mut prev = 0u8;
    for &b in data {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

/// Restore (discard) a single hunk: revert that hunk's region to the
/// "old" side while leaving every other hunk untouched.
///
/// - `staged = false`: the unstaged diff (index -> workdir). The workdir
///   file's hunk region (`new` side) is replaced with the hunk's old
///   segment (context + deletions). An untracked file whose only hunk is
///   discarded is deleted, matching [`discard_file`].
/// - `staged = true`: the staged diff (HEAD -> index). The index blob's
///   hunk region is replaced with the HEAD side. A staged-new file whose
///   only hunk is discarded is removed from the index (leaving the
///   workdir file untracked).
///
/// Like [`stage_hunk`] this splices bytes: lines outside the hunk keep
/// their exact content and terminators, and the restored lines are copied
/// from the blob they come from (index or HEAD). The one adjustment is
/// for a working-tree file whose line endings git converts on the way in
/// (`core.autocrlf`, `eol=crlf`): lines restored into it get `\r\n` back.
///
/// Symlinks and files deleted from the working tree are refused for the
/// unstaged case: restore the whole file instead.
pub fn discard_hunk(
    repo: &git2::Repository,
    path: &str,
    hunk_index: usize,
    staged: bool,
) -> Result<(), GitError> {
    let fail = |msg: String| GitError::Discard(msg);
    let diff = if staged {
        crate::diff::staged_diff(repo, path)?
    } else {
        crate::diff::unstaged_diff(repo, path)?
    };
    if diff.binary {
        return Err(fail(format!("cannot restore hunk of binary file {path}")));
    }
    if hunk_index >= diff.hunks.len() {
        return Err(fail(format!(
            "hunk index {hunk_index} out of range ({} hunks in {path})",
            diff.hunks.len()
        )));
    }
    let hunk = &diff.hunks[hunk_index];
    let has_changes =
        hunk.lines.iter().any(|l| {
            l.kind == LineKind::Context || l.kind == LineKind::Add || l.kind == LineKind::Del
        }) && hunk
            .lines
            .iter()
            .any(|l| l.kind == LineKind::Add || l.kind == LineKind::Del);
    if !has_changes {
        return Err(fail(format!(
            "nothing to restore in hunk {hunk_index} of {path}"
        )));
    }
    let (old_start, old_lines, new_start, new_lines) = parse_hunk_header_full(&hunk.header)
        .ok_or_else(|| fail(format!("cannot parse hunk header: {}", hunk.header)))?;
    let old_at = line_index(old_start, old_lines);
    let new_at = line_index(new_start, new_lines);
    let stale = || {
        fail(format!(
            "{path} changed while restoring hunk {hunk_index}: reload and retry"
        ))
    };

    let rel = Path::new(path);
    let mut index = fresh_index(repo)?;
    let in_index = index.get_path(rel, 0).is_some();
    let index_blob = stage0_blob(repo, &index, rel)?;
    let head = head_entry(repo, path);

    if staged {
        // HEAD -> index: the index blob's region goes back to HEAD's lines.
        let head_blob = match head {
            Some((id, _)) => repo.find_blob(id)?.content().to_vec(),
            None => Vec::new(),
        };
        let segment = take_lines(&head_blob, old_at, old_lines).ok_or_else(stale)?;
        let restored = splice_lines(&index_blob, new_at, new_lines, segment).ok_or_else(stale)?;
        if restored.is_empty() && head.is_none() {
            // Staged-new file fully restored: drop it from the index so
            // the workdir copy becomes untracked again.
            let _ = index.remove_path(rel);
            index.write()?;
            return Ok(());
        }
        // A staged deletion has no index entry left: it comes back with
        // HEAD's mode.
        let mode = head.map_or(0o100644, |(_, mode)| mode);
        write_index_content(&mut index, path, &restored, mode)
            .map_err(|e| fail(format!("cannot write {path} to index: {e}")))?;
        return Ok(());
    }

    // Unstaged: rewrite the workdir file.
    let full = repo
        .workdir()
        .ok_or_else(|| fail("bare repo".to_string()))?
        .join(rel);
    match std::fs::symlink_metadata(&full) {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Reading and writing would go through the link and rewrite
            // whatever it points at.
            return Err(fail(format!(
                "{path} is a symlink: restore the whole file instead"
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(fail(format!(
                "{path} was deleted: restore the whole file instead"
            )));
        }
        Err(e) => return Err(fail(format!("cannot read {path}: {e}"))),
    }
    let raw = std::fs::read(&full).map_err(|e| fail(format!("cannot read {path}: {e}")))?;
    let mut segment = take_lines(&index_blob, old_at, old_lines)
        .ok_or_else(stale)?
        .to_vec();
    // The hunk's new side is numbered against the file as git stores it.
    // When storing strips carriage returns, the lines coming back from the
    // blob need theirs again or the file ends up with mixed endings.
    let stored = to_odb_form(repo, path, &raw).map_err(fail)?;
    if stored != raw {
        let breaks = |d: &[u8]| d.iter().filter(|&&b| b == b'\n').count();
        if breaks(&stored) != breaks(&raw) {
            return Err(fail(format!(
                "{path} is rewritten by a filter: restore the whole file instead"
            )));
        }
        if crlf_count(&raw) > crlf_count(&stored) {
            segment = lf_to_crlf(&segment);
        }
    }
    let restored = splice_lines(&raw, new_at, new_lines, &segment).ok_or_else(stale)?;
    if restored.is_empty() && head.is_none() && !in_index {
        // Untracked file fully restored: delete it like discard_file.
        remove_workdir_path(repo, path).map_err(|e| fail(e.to_string()))?;
        return Ok(());
    }
    std::fs::write(&full, &restored).map_err(|e| fail(format!("cannot write {path}: {e}")))?;
    Ok(())
}

/// What a path looked like before a destructive action, so it can be put
/// back. Taken with [`snapshot_path`], undone with [`restore_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Repo-relative path, forward slashes.
    pub path: String,
    /// Blob holding the working-tree file's raw bytes, or `None` when the
    /// file did not exist. The blob is unreferenced: it lives in the
    /// object database until git garbage-collects it (two weeks by
    /// default), which bounds how long an undo stays possible.
    pub workdir: Option<git2::Oid>,
    /// Whether the working-tree file was executable (always `false` on
    /// Windows).
    pub executable: bool,
    /// The index entry as `(blob id, file mode)`, or `None` when the path
    /// was not in the index.
    pub index: Option<(git2::Oid, u32)>,
}

/// Record `path`'s working-tree content and index entry without changing
/// either. The file's bytes are stored raw (no line-ending conversion) so
/// [`restore_snapshot`] puts back exactly what was on disk.
///
/// Files only: directories and symlinks are refused, and a conflicted
/// path has no single index entry to record.
pub fn snapshot_path(repo: &git2::Repository, path: &str) -> Result<Snapshot, GitError> {
    let fail = |msg: String| GitError::Discard(format!("cannot snapshot {path}: {msg}"));
    let rel = Path::new(path);
    let full = repo
        .workdir()
        .ok_or_else(|| fail("bare repo".into()))?
        .join(rel);
    let (workdir, executable) = match std::fs::symlink_metadata(&full) {
        Ok(meta) if meta.file_type().is_symlink() => return Err(fail("it is a symlink".into())),
        Ok(meta) if meta.is_dir() => return Err(fail("it is a directory".into())),
        Ok(meta) => {
            let bytes = std::fs::read(&full).map_err(|e| fail(e.to_string()))?;
            let oid = repo.blob(&bytes).map_err(|e| fail(e.to_string()))?;
            (Some(oid), new_entry_mode(&meta) == 0o100755)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, false),
        Err(e) => return Err(fail(e.to_string())),
    };
    let index = fresh_index(repo)?;
    if (1..=3).any(|stage| index.get_path(rel, stage).is_some()) {
        return Err(fail("it has unresolved conflicts".into()));
    }
    let index = index.get_path(rel, 0).map(|e| (e.id, e.mode));
    Ok(Snapshot {
        path: path.to_string(),
        workdir,
        executable,
        index,
    })
}

/// Put a path back the way [`snapshot_path`] found it: rewrite the
/// working-tree file (creating parent directories), or remove it when it
/// did not exist, and restore or drop its index entry.
pub fn restore_snapshot(repo: &git2::Repository, snap: &Snapshot) -> Result<(), GitError> {
    let path = snap.path.as_str();
    let fail = |msg: String| GitError::Discard(format!("cannot restore {path}: {msg}"));
    let rel = Path::new(path);
    let full = repo
        .workdir()
        .ok_or_else(|| fail("bare repo".into()))?
        .join(rel);

    match snap.workdir {
        Some(oid) => {
            let blob = repo
                .find_blob(oid)
                .map_err(|e| fail(format!("saved content is gone: {e}")))?;
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).map_err(|e| fail(e.to_string()))?;
            }
            std::fs::write(&full, blob.content()).map_err(|e| fail(e.to_string()))?;
            #[cfg(unix)]
            if snap.executable {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&full)
                    .map_err(|e| fail(e.to_string()))?
                    .permissions();
                perms.set_mode(perms.mode() | 0o111);
                std::fs::set_permissions(&full, perms).map_err(|e| fail(e.to_string()))?;
            }
        }
        None => match std::fs::symlink_metadata(&full) {
            Ok(meta) if meta.is_dir() => return Err(fail("it is now a directory".into())),
            Ok(_) => std::fs::remove_file(&full).map_err(|e| fail(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(fail(e.to_string())),
        },
    }

    let mut index = fresh_index(repo)?;
    match snap.index {
        Some((id, mode)) => {
            let size = repo.find_blob(id).map(|b| b.size()).unwrap_or(0);
            index
                .add(&blank_entry(path, id, mode, size))
                .map_err(|e| fail(format!("index entry: {e}")))?;
        }
        // Absent paths are fine: `remove_path` only fails on real errors.
        None => index
            .remove_path(rel)
            .map_err(|e| fail(format!("index entry: {e}")))?,
    }
    index.write()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use std::fs;

    fn two_hunk_repo() -> (tempfile::TempDir, git2::Repository) {
        let (dir, repo) = testutil::init_repo();
        let base = (1..=40).map(|i| format!("line {i}\n")).collect::<String>();
        testutil::commit_file(&repo, "a.txt", &base, "init");
        let dirty = base.replacen("line 5\n", "line 5 CHANGED\n", 1).replacen(
            "line 35\n",
            "line 35 CHANGED\n",
            1,
        );
        fs::write(repo.workdir().unwrap().join("a.txt"), &dirty).unwrap();
        (dir, repo)
    }

    #[test]
    fn stage_single_hunk_leaves_other_hunks_unstaged() {
        let (_dir, repo) = two_hunk_repo();
        let before = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert!(before.hunks.len() >= 2);

        stage_hunk(&repo, "a.txt", 0).unwrap();

        // Staged diff now has exactly one hunk; unstaged has the remainder.
        let staged = crate::diff::staged_diff(&repo, "a.txt").unwrap();
        let unstaged = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert_eq!(staged.hunks.len(), 1, "staged: {staged:?}");
        assert_eq!(unstaged.hunks.len(), before.hunks.len() - 1);

        let status = crate::status::repo_status(&repo).unwrap();
        let entry = status.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, crate::status::FileState::BothStagedAndUnstaged);
    }

    #[test]
    fn unstage_file_restores_index_to_head() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        stage_file(&repo, "a.txt").unwrap();
        assert!(!crate::diff::staged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
        unstage_file(&repo, "a.txt").unwrap();
        assert!(crate::diff::staged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
        assert!(!crate::diff::unstaged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
    }

    #[test]
    fn stage_out_of_range_hunk_index_errors() {
        let (_dir, repo) = two_hunk_repo();
        let err = stage_hunk(&repo, "a.txt", 99).unwrap_err();
        match err {
            GitError::HunkStaging(_) => {}
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn discard_unstaged_file_restores_head_content() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        discard_file(&repo, "a.txt").unwrap();
        let content = fs::read_to_string(repo.workdir().unwrap().join("a.txt")).unwrap();
        assert_eq!(content, "a\n");
        let status = crate::status::repo_status(&repo).unwrap();
        assert!(status.files.iter().all(|e| e.path != "a.txt"));
    }

    #[test]
    fn discard_staged_and_unstaged_changes_restores_head() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "staged\n");
        stage_file(&repo, "a.txt").unwrap();
        testutil::dirty_file(&repo, "a.txt", "unstaged\n");
        discard_file(&repo, "a.txt").unwrap();
        let content = fs::read_to_string(repo.workdir().unwrap().join("a.txt")).unwrap();
        assert_eq!(content, "a\n");
        assert!(crate::diff::staged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
        let status = crate::status::repo_status(&repo).unwrap();
        assert!(status.files.iter().all(|e| e.path != "a.txt"));
    }

    #[test]
    fn discard_untracked_file_deletes_it() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        fs::write(repo.workdir().unwrap().join("new.txt"), "new\n").unwrap();
        discard_file(&repo, "new.txt").unwrap();
        assert!(!repo.workdir().unwrap().join("new.txt").exists());
    }

    #[test]
    fn discard_staged_new_file_deletes_it() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        fs::write(repo.workdir().unwrap().join("new.txt"), "new\n").unwrap();
        stage_file(&repo, "new.txt").unwrap();
        discard_file(&repo, "new.txt").unwrap();
        assert!(!repo.workdir().unwrap().join("new.txt").exists());
        let status = crate::status::repo_status(&repo).unwrap();
        assert!(status.files.iter().all(|e| e.path != "new.txt"));
    }

    #[test]
    fn discard_clean_file_errors() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let err = discard_file(&repo, "a.txt").unwrap_err();
        match err {
            GitError::Discard(msg) => assert!(msg.contains("nothing to discard"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn discard_unstaged_hunk_leaves_other_hunks() {
        let (_dir, repo) = two_hunk_repo();
        let before = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert!(before.hunks.len() >= 2);
        discard_hunk(&repo, "a.txt", 0, false).unwrap();
        let after = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert_eq!(after.hunks.len(), before.hunks.len() - 1);
        let content = fs::read_to_string(repo.workdir().unwrap().join("a.txt")).unwrap();
        assert!(
            content.contains("line 5\n"),
            "first hunk restored: {content:?}"
        );
        assert!(
            content.contains("line 35 CHANGED"),
            "second hunk kept: {content:?}"
        );
    }

    #[test]
    fn discard_staged_hunk_reverts_index_only() {
        let (_dir, repo) = two_hunk_repo();
        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert_eq!(
            crate::diff::staged_diff(&repo, "a.txt")
                .unwrap()
                .hunks
                .len(),
            1
        );
        discard_hunk(&repo, "a.txt", 0, true).unwrap();
        assert!(crate::diff::staged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
        // Workdir still holds both changes; only the index was restored.
        let content = fs::read_to_string(repo.workdir().unwrap().join("a.txt")).unwrap();
        assert!(content.contains("line 5 CHANGED"));
    }

    #[test]
    fn discard_untracked_single_hunk_deletes_file() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        fs::write(repo.workdir().unwrap().join("new.txt"), "one\ntwo\n").unwrap();
        discard_hunk(&repo, "new.txt", 0, false).unwrap();
        assert!(!repo.workdir().unwrap().join("new.txt").exists());
    }

    #[test]
    fn discard_out_of_range_hunk_errors() {
        let (_dir, repo) = two_hunk_repo();
        let err = discard_hunk(&repo, "a.txt", 99, false).unwrap_err();
        match err {
            GitError::Discard(msg) => assert!(msg.contains("out of range"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
    }

    /// Names the pathspec syntax would read as something other than a
    /// plain path: a character class (matching `a1.txt` too), a comment,
    /// a negation, and embedded whitespace.
    const ODD_NAMES: [&str; 4] = ["#notes.txt", "!bang.txt", "my  file .txt", "a[1].txt"];

    /// Ordinary files that a sloppy match of an odd name would hit: the
    /// class match, and names the odd ones are a prefix of.
    const BYSTANDERS: [&str; 4] = ["a1.txt", "#notes.txt.bak", "!bang.txt.bak", "other.txt"];

    /// Every odd name plus the bystanders, all committed and then edited
    /// in the workdir.
    fn odd_names_repo() -> (tempfile::TempDir, git2::Repository) {
        let (dir, repo) = testutil::init_repo();
        for name in ODD_NAMES.iter().chain(&BYSTANDERS) {
            testutil::commit_file(&repo, name, "committed\n", "add");
            testutil::dirty_file(&repo, name, "edited\n");
        }
        (dir, repo)
    }

    #[test]
    fn literal_pathspec_neutralises_pattern_syntax() {
        assert_eq!(literal_pathspec("src/main.rs"), "src/main.rs");
        assert_eq!(literal_pathspec("a[1]*?.txt"), "a[[]1][*][?].txt");
        assert_eq!(literal_pathspec("#x#"), "[#]x#");
        assert_eq!(literal_pathspec("!x!"), "[/!]x!");
        assert_eq!(literal_pathspec("dir/!x"), "dir/!x");
        assert_eq!(literal_pathspec("a b "), "a[ ]b[ ]");
        assert_eq!(literal_pathspec("a\\b"), "a[\\\\]b");
    }

    #[test]
    fn discard_file_takes_odd_names_literally() {
        for name in ODD_NAMES {
            let (_dir, repo) = odd_names_repo();
            let wd = repo.workdir().unwrap().to_path_buf();
            discard_file(&repo, name).unwrap();
            assert_eq!(
                fs::read_to_string(wd.join(name)).unwrap(),
                "committed\n",
                "{name} itself is restored"
            );
            // Nothing else may be touched: a forced checkout of a pattern
            // that matched more would silently throw work away.
            for other in ODD_NAMES.iter().chain(&BYSTANDERS).filter(|o| **o != name) {
                assert_eq!(
                    fs::read_to_string(wd.join(other)).unwrap(),
                    "committed\nedited\n",
                    "discarding {name} must leave {other} alone"
                );
            }
        }
    }

    #[test]
    fn unstage_file_takes_odd_names_literally() {
        for name in ODD_NAMES {
            let (_dir, repo) = odd_names_repo();
            for staged in ODD_NAMES.iter().chain(&BYSTANDERS) {
                stage_file(&repo, staged).unwrap();
            }
            unstage_file(&repo, name).unwrap();
            let st = crate::status::repo_status(&repo).unwrap();
            for entry in &st.files {
                let expected = if entry.path == name {
                    crate::status::FileState::Unstaged
                } else {
                    crate::status::FileState::Staged
                };
                assert_eq!(
                    entry.state, expected,
                    "{} after unstaging {name}",
                    entry.path
                );
            }
            assert_eq!(st.files.len(), ODD_NAMES.len() + BYSTANDERS.len());
        }
    }

    // ---- byte-exact hunk splicing ----

    /// 40 numbered lines joined by `eol`; lines listed in `changed` get a
    /// ` CHANGED` suffix. `final_eol` controls the last line's terminator.
    fn numbered(eol: &str, changed: &[usize], final_eol: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 1..=40 {
            out.extend_from_slice(format!("line {i}").as_bytes());
            if changed.contains(&i) {
                out.extend_from_slice(b" CHANGED");
            }
            if i < 40 || final_eol {
                out.extend_from_slice(eol.as_bytes());
            }
        }
        out
    }

    fn write_wd(repo: &git2::Repository, path: &str, bytes: &[u8]) {
        fs::write(repo.workdir().unwrap().join(path), bytes).unwrap();
    }

    fn read_wd(repo: &git2::Repository, path: &str) -> Vec<u8> {
        fs::read(repo.workdir().unwrap().join(path)).unwrap()
    }

    /// Bytes of `path`'s index blob.
    fn index_bytes(repo: &git2::Repository, path: &str) -> Vec<u8> {
        let entry = repo
            .index()
            .unwrap()
            .get_path(Path::new(path), 0)
            .unwrap_or_else(|| panic!("{path} not in index"));
        repo.find_blob(entry.id).unwrap().content().to_vec()
    }

    fn in_index(repo: &git2::Repository, path: &str) -> bool {
        repo.index().unwrap().get_path(Path::new(path), 0).is_some()
    }

    /// The added/removed line texts of a diff, as `+text` / `-text`.
    fn changes(diff: &crate::diff::FileDiff) -> Vec<String> {
        diff.hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter_map(|l| match l.kind {
                LineKind::Add => Some(format!("+{}", l.text)),
                LineKind::Del => Some(format!("-{}", l.text)),
                _ => None,
            })
            .collect()
    }

    fn state_of(repo: &git2::Repository, path: &str) -> Option<crate::status::FileState> {
        crate::status::repo_status(repo)
            .unwrap()
            .files
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.state)
    }

    fn lossy(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Committed 40-line file with `eol` endings, then lines 5 and 35
    /// changed in the workdir (two hunks).
    fn two_hunk_bytes_repo(eol: &str, final_eol: bool) -> (tempfile::TempDir, git2::Repository) {
        let (dir, repo) = testutil::init_repo();
        testutil::commit_bytes(&repo, "a.txt", &numbered(eol, &[], final_eol), "init");
        write_wd(&repo, "a.txt", &numbered(eol, &[5, 35], final_eol));
        (dir, repo)
    }

    #[test]
    fn stage_hunk_keeps_crlf_line_endings() {
        let (_dir, repo) = two_hunk_bytes_repo("\r\n", true);
        assert_eq!(
            crate::diff::unstaged_diff(&repo, "a.txt")
                .unwrap()
                .hunks
                .len(),
            2
        );
        stage_hunk(&repo, "a.txt", 0).unwrap();
        let staged = index_bytes(&repo, "a.txt");
        assert_eq!(
            lossy(&staged),
            lossy(&numbered("\r\n", &[5], true)),
            "index blob must stay CRLF with only hunk 0 applied"
        );
        // What is left unstaged is exactly the other hunk.
        let rest = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert_eq!(rest.hunks.len(), 1);
        assert_eq!(changes(&rest), ["-line 35", "+line 35 CHANGED"]);
        let staged_diff = crate::diff::staged_diff(&repo, "a.txt").unwrap();
        assert_eq!(changes(&staged_diff), ["-line 5", "+line 5 CHANGED"]);
        // Staging the second hunk too lands on the workdir content.
        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), read_wd(&repo, "a.txt"));
    }

    #[test]
    fn discard_unstaged_hunk_keeps_crlf_line_endings() {
        let (_dir, repo) = two_hunk_bytes_repo("\r\n", true);
        discard_hunk(&repo, "a.txt", 0, false).unwrap();
        assert_eq!(
            lossy(&read_wd(&repo, "a.txt")),
            lossy(&numbered("\r\n", &[35], true))
        );
        let rest = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert_eq!(changes(&rest), ["-line 35", "+line 35 CHANGED"]);
        // Index untouched.
        assert_eq!(index_bytes(&repo, "a.txt"), numbered("\r\n", &[], true));
    }

    #[test]
    fn discard_staged_hunk_keeps_crlf_line_endings() {
        let (_dir, repo) = two_hunk_bytes_repo("\r\n", true);
        stage_file(&repo, "a.txt").unwrap();
        assert_eq!(
            crate::diff::staged_diff(&repo, "a.txt")
                .unwrap()
                .hunks
                .len(),
            2
        );
        discard_hunk(&repo, "a.txt", 0, true).unwrap();
        assert_eq!(
            lossy(&index_bytes(&repo, "a.txt")),
            lossy(&numbered("\r\n", &[35], true))
        );
        let staged = crate::diff::staged_diff(&repo, "a.txt").unwrap();
        assert_eq!(changes(&staged), ["-line 35", "+line 35 CHANGED"]);
        // Workdir untouched.
        assert_eq!(read_wd(&repo, "a.txt"), numbered("\r\n", &[5, 35], true));
    }

    /// With `core.autocrlf=true` the blob is LF and the workdir CRLF:
    /// staging must add LF lines, restoring must put CRLF lines back.
    #[test]
    fn hunks_follow_autocrlf_conversion() {
        let (dir, repo) = testutil::init_repo();
        repo.config()
            .unwrap()
            .set_str("core.autocrlf", "true")
            .unwrap();
        // Reopen so no cached setting from the fixture survives.
        drop(repo);
        let repo = git2::Repository::open(dir.path()).unwrap();
        testutil::commit_bytes(&repo, "a.txt", &numbered("\r\n", &[], true), "init");
        assert_eq!(
            index_bytes(&repo, "a.txt"),
            numbered("\n", &[], true),
            "fixture: autocrlf should store LF"
        );
        write_wd(&repo, "a.txt", &numbered("\r\n", &[5, 35], true));

        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert_eq!(
            lossy(&index_bytes(&repo, "a.txt")),
            lossy(&numbered("\n", &[5], true)),
            "no CR may leak into the stored blob"
        );
        // The remaining unstaged hunk goes back to the index content.
        discard_hunk(&repo, "a.txt", 0, false).unwrap();
        assert_eq!(
            lossy(&read_wd(&repo, "a.txt")),
            lossy(&numbered("\r\n", &[5], true)),
            "workdir must stay CRLF throughout"
        );
        assert!(crate::diff::unstaged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
    }

    /// 40 lines where line 20 holds a Latin-1 `é` (0xE9: invalid UTF-8).
    fn latin1(changed: &[usize]) -> Vec<u8> {
        let text = numbered("\n", changed, true);
        let needle = b"line 20\n";
        let at = text
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap();
        let mut out = text[..at].to_vec();
        out.extend_from_slice(b"caf\xe9 20\n");
        out.extend_from_slice(&text[at + needle.len()..]);
        out
    }

    #[test]
    fn non_utf8_bytes_on_untouched_lines_survive() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_bytes(&repo, "a.txt", &latin1(&[]), "init");
        write_wd(&repo, "a.txt", &latin1(&[5, 35]));
        assert_eq!(
            crate::diff::unstaged_diff(&repo, "a.txt")
                .unwrap()
                .hunks
                .len(),
            2
        );

        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), latin1(&[5]));

        // Restore the staged hunk in the index, then the first workdir hunk.
        discard_hunk(&repo, "a.txt", 0, true).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), latin1(&[]));
        discard_hunk(&repo, "a.txt", 0, false).unwrap();
        assert_eq!(read_wd(&repo, "a.txt"), latin1(&[35]));
    }

    #[test]
    fn stage_hunk_adds_final_newline() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "one\ntwo\nthree", "init");
        write_wd(&repo, "a.txt", b"one\ntwo\nthree\n");
        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), b"one\ntwo\nthree\n");
        assert!(crate::diff::unstaged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
    }

    #[test]
    fn stage_hunk_removes_final_newline() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "one\ntwo\nthree\n", "init");
        write_wd(&repo, "a.txt", b"one\ntwo\nthree");
        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), b"one\ntwo\nthree");
        assert!(crate::diff::unstaged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
    }

    #[test]
    fn discard_hunk_restores_final_newline_state() {
        // Workdir dropped the final newline: restoring brings it back.
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "one\ntwo\nthree\n", "init");
        write_wd(&repo, "a.txt", b"one\ntwo\nthree");
        discard_hunk(&repo, "a.txt", 0, false).unwrap();
        assert_eq!(read_wd(&repo, "a.txt"), b"one\ntwo\nthree\n");

        // Workdir added one (staged): restoring the index drops it again.
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "one\ntwo\nthree", "init");
        write_wd(&repo, "a.txt", b"one\ntwo\nthree\n");
        stage_file(&repo, "a.txt").unwrap();
        discard_hunk(&repo, "a.txt", 0, true).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), b"one\ntwo\nthree");
        assert_eq!(read_wd(&repo, "a.txt"), b"one\ntwo\nthree\n");
    }

    #[test]
    fn missing_final_newline_is_left_alone_by_hunks_elsewhere() {
        let (_dir, repo) = two_hunk_bytes_repo("\n", false);
        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), numbered("\n", &[5], false));
        // The second hunk (line 35) sits near the end but does not change
        // line 40 either.
        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), numbered("\n", &[5, 35], false));
        discard_hunk(&repo, "a.txt", 0, true).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), numbered("\n", &[35], false));

        let (_dir, repo) = two_hunk_bytes_repo("\n", false);
        discard_hunk(&repo, "a.txt", 0, false).unwrap();
        assert_eq!(read_wd(&repo, "a.txt"), numbered("\n", &[35], false));
        discard_hunk(&repo, "a.txt", 0, false).unwrap();
        assert_eq!(read_wd(&repo, "a.txt"), numbered("\n", &[], false));
    }

    #[test]
    fn stage_hunk_handles_insertions_and_deletions() {
        // Hunk 0 deletes two lines, hunk 1 inserts two: the line numbers
        // of the second hunk differ between the two sides.
        let (_dir, repo) = testutil::init_repo();
        let base = numbered("\n", &[], true);
        testutil::commit_bytes(&repo, "a.txt", &base, "init");
        let text = lossy(&base)
            .replace("line 5\nline 6\n", "")
            .replace("line 30\n", "line 30\nnew A\nnew B\n");
        write_wd(&repo, "a.txt", text.as_bytes());

        stage_hunk(&repo, "a.txt", 1).unwrap();
        let expected = lossy(&base).replace("line 30\n", "line 30\nnew A\nnew B\n");
        assert_eq!(lossy(&index_bytes(&repo, "a.txt")), expected);
        let rest = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert_eq!(changes(&rest), ["-line 5", "-line 6"]);

        // And the mirror image through discard.
        discard_hunk(&repo, "a.txt", 0, false).unwrap();
        assert_eq!(lossy(&read_wd(&repo, "a.txt")), expected);
        assert!(crate::diff::unstaged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
    }

    #[test]
    fn stage_hunk_of_untracked_file_stores_its_bytes() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        write_wd(&repo, "new.txt", b"one\r\ntwo\r\nlast");
        stage_hunk(&repo, "new.txt", 0).unwrap();
        assert_eq!(index_bytes(&repo, "new.txt"), b"one\r\ntwo\r\nlast");
        let entry = repo
            .index()
            .unwrap()
            .get_path(Path::new("new.txt"), 0)
            .unwrap();
        assert_eq!(entry.mode, 0o100644);
        assert_eq!(
            state_of(&repo, "new.txt"),
            Some(crate::status::FileState::Staged)
        );
    }

    #[cfg(unix)]
    #[test]
    fn stage_hunk_of_untracked_executable_keeps_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        write_wd(&repo, "run.sh", b"#!/bin/sh\necho hi\n");
        let full = repo.workdir().unwrap().join("run.sh");
        fs::set_permissions(&full, fs::Permissions::from_mode(0o755)).unwrap();
        stage_hunk(&repo, "run.sh", 0).unwrap();
        let entry = repo
            .index()
            .unwrap()
            .get_path(Path::new("run.sh"), 0)
            .unwrap();
        assert_eq!(entry.mode, 0o100755);
    }

    // ---- deletions ----

    #[test]
    fn stage_file_stages_deletion_of_missing_tracked_file() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::commit_file(&repo, "b.txt", "b\n", "second");
        fs::remove_file(repo.workdir().unwrap().join("a.txt")).unwrap();
        assert_eq!(
            state_of(&repo, "a.txt"),
            Some(crate::status::FileState::Unstaged)
        );
        stage_file(&repo, "a.txt").unwrap();
        assert!(!in_index(&repo, "a.txt"));
        assert!(in_index(&repo, "b.txt"));
        assert_eq!(
            state_of(&repo, "a.txt"),
            Some(crate::status::FileState::Staged)
        );
        let staged = crate::diff::staged_diff(&repo, "a.txt").unwrap();
        assert_eq!(changes(&staged), ["-a"]);
    }

    #[test]
    fn stage_file_of_path_that_exists_nowhere_errors() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let err = stage_file(&repo, "nope.txt").unwrap_err();
        assert!(matches!(err, GitError::HunkStaging(_)), "got {err:?}");
    }

    #[test]
    fn stage_hunk_on_deleted_file_stages_the_deletion() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "one\ntwo\n", "init");
        fs::remove_file(repo.workdir().unwrap().join("a.txt")).unwrap();
        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert!(
            !in_index(&repo, "a.txt"),
            "deletion must drop the entry, not store an empty blob"
        );
        assert_eq!(
            state_of(&repo, "a.txt"),
            Some(crate::status::FileState::Staged)
        );
    }

    #[test]
    fn discard_staged_deletion_hunk_restores_the_index_entry() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "one\r\ntwo", "init");
        fs::remove_file(repo.workdir().unwrap().join("a.txt")).unwrap();
        stage_file(&repo, "a.txt").unwrap();
        assert!(!in_index(&repo, "a.txt"));
        discard_hunk(&repo, "a.txt", 0, true).unwrap();
        assert_eq!(index_bytes(&repo, "a.txt"), b"one\r\ntwo");
        // Back to an unstaged deletion.
        assert_eq!(
            state_of(&repo, "a.txt"),
            Some(crate::status::FileState::Unstaged)
        );
    }

    #[test]
    fn discard_unstaged_hunk_of_deleted_file_is_refused() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "one\ntwo\n", "init");
        fs::remove_file(repo.workdir().unwrap().join("a.txt")).unwrap();
        let err = discard_hunk(&repo, "a.txt", 0, false).unwrap_err();
        match err {
            GitError::Discard(msg) => assert!(msg.contains("deleted"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
    }

    /// Create a file symlink; `false` when the platform refuses (Windows
    /// without developer mode), in which case the caller skips.
    fn symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(target, link);
        made.is_ok()
    }

    #[test]
    fn hunk_operations_refuse_symlinks() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "real.txt", "real\n", "init");
        testutil::commit_file(&repo, "link.txt", "placeholder\n", "link");
        let wd = repo.workdir().unwrap().to_path_buf();
        fs::remove_file(wd.join("link.txt")).unwrap();
        if !symlink(Path::new("real.txt"), &wd.join("link.txt")) {
            eprintln!("skipping: cannot create symlinks here");
            return;
        }
        if crate::diff::unstaged_diff(&repo, "link.txt")
            .unwrap()
            .hunks
            .is_empty()
        {
            eprintln!("skipping: symlink change not visible (core.symlinks=false)");
            return;
        }
        let err = discard_hunk(&repo, "link.txt", 0, false).unwrap_err();
        match err {
            GitError::Discard(msg) => assert!(msg.contains("symlink"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
        let err = stage_hunk(&repo, "link.txt", 0).unwrap_err();
        match err {
            GitError::HunkStaging(msg) => assert!(msg.contains("symlink"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
        // Neither went through the link.
        assert_eq!(read_wd(&repo, "real.txt"), b"real\n");
    }

    #[test]
    fn stage_file_resolves_a_conflicted_path() {
        let (_dir, repo) = testutil::conflicted_merge();
        assert_eq!(
            state_of(&repo, "a.txt"),
            Some(crate::status::FileState::Conflicted)
        );
        write_wd(&repo, "a.txt", b"resolved\n");
        stage_file(&repo, "a.txt").unwrap();
        assert!(!repo.index().unwrap().has_conflicts());
        assert_eq!(index_bytes(&repo, "a.txt"), b"resolved\n");
        assert_ne!(
            state_of(&repo, "a.txt"),
            Some(crate::status::FileState::Conflicted)
        );
    }

    // ---- snapshots (undo for discards) ----

    #[test]
    fn snapshot_restores_discarded_unstaged_change() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\r\n", "init");
        write_wd(&repo, "a.txt", b"a\r\nlocal \xe9\r\nno newline");
        let before = read_wd(&repo, "a.txt");
        let index_before = index_bytes(&repo, "a.txt");

        let snap = snapshot_path(&repo, "a.txt").unwrap();
        // Taking the snapshot changes nothing.
        assert_eq!(read_wd(&repo, "a.txt"), before);
        assert_eq!(index_bytes(&repo, "a.txt"), index_before);

        discard_file(&repo, "a.txt").unwrap();
        assert_eq!(state_of(&repo, "a.txt"), None);
        restore_snapshot(&repo, &snap).unwrap();
        assert_eq!(read_wd(&repo, "a.txt"), before);
        assert_eq!(index_bytes(&repo, "a.txt"), index_before);
        assert_eq!(
            state_of(&repo, "a.txt"),
            Some(crate::status::FileState::Unstaged)
        );
    }

    #[test]
    fn snapshot_restores_discarded_staged_and_unstaged_change() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "staged\n");
        stage_file(&repo, "a.txt").unwrap();
        testutil::dirty_file(&repo, "a.txt", "unstaged\n");
        let staged_before = crate::diff::staged_diff(&repo, "a.txt").unwrap();
        let unstaged_before = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();

        let snap = snapshot_path(&repo, "a.txt").unwrap();
        discard_file(&repo, "a.txt").unwrap();
        assert_eq!(read_wd(&repo, "a.txt"), b"a\n");
        restore_snapshot(&repo, &snap).unwrap();

        assert_eq!(read_wd(&repo, "a.txt"), b"a\nstaged\nunstaged\n");
        assert_eq!(index_bytes(&repo, "a.txt"), b"a\nstaged\n");
        assert_eq!(
            state_of(&repo, "a.txt"),
            Some(crate::status::FileState::BothStagedAndUnstaged)
        );
        assert_eq!(
            crate::diff::staged_diff(&repo, "a.txt").unwrap(),
            staged_before
        );
        assert_eq!(
            crate::diff::unstaged_diff(&repo, "a.txt").unwrap(),
            unstaged_before
        );
    }

    #[test]
    fn snapshot_restores_discarded_untracked_file() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        fs::create_dir_all(repo.workdir().unwrap().join("sub")).unwrap();
        write_wd(&repo, "sub/new.txt", b"brand new\n");

        let snap = snapshot_path(&repo, "sub/new.txt").unwrap();
        assert_eq!(snap.index, None);
        discard_file(&repo, "sub/new.txt").unwrap();
        assert!(!repo.workdir().unwrap().join("sub/new.txt").exists());
        // Even if the now-empty directory went away too.
        let _ = fs::remove_dir(repo.workdir().unwrap().join("sub"));
        restore_snapshot(&repo, &snap).unwrap();

        assert_eq!(read_wd(&repo, "sub/new.txt"), b"brand new\n");
        assert!(!in_index(&repo, "sub/new.txt"));
        assert_eq!(
            state_of(&repo, "sub/new.txt"),
            Some(crate::status::FileState::Untracked)
        );
    }

    #[test]
    fn snapshot_restores_discarded_staged_new_file() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        write_wd(&repo, "new.txt", b"new\n");
        stage_file(&repo, "new.txt").unwrap();

        let snap = snapshot_path(&repo, "new.txt").unwrap();
        discard_file(&repo, "new.txt").unwrap();
        assert!(!in_index(&repo, "new.txt"));
        restore_snapshot(&repo, &snap).unwrap();

        assert_eq!(read_wd(&repo, "new.txt"), b"new\n");
        assert_eq!(index_bytes(&repo, "new.txt"), b"new\n");
        assert_eq!(
            state_of(&repo, "new.txt"),
            Some(crate::status::FileState::Staged)
        );
    }

    #[test]
    fn snapshot_of_missing_file_restores_to_missing() {
        // Tracked file deleted from the workdir: discard brings it back,
        // undoing the discard deletes it again.
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        fs::remove_file(repo.workdir().unwrap().join("a.txt")).unwrap();
        let snap = snapshot_path(&repo, "a.txt").unwrap();
        assert_eq!(snap.workdir, None);
        assert!(snap.index.is_some());
        discard_file(&repo, "a.txt").unwrap();
        assert_eq!(read_wd(&repo, "a.txt"), b"a\n");
        restore_snapshot(&repo, &snap).unwrap();
        assert!(!repo.workdir().unwrap().join("a.txt").exists());
        assert!(in_index(&repo, "a.txt"));
        assert_eq!(
            state_of(&repo, "a.txt"),
            Some(crate::status::FileState::Unstaged)
        );

        // Path that existed nowhere: restoring removes whatever appeared.
        let snap = snapshot_path(&repo, "ghost.txt").unwrap();
        assert_eq!((snap.workdir, snap.index), (None, None));
        write_wd(&repo, "ghost.txt", b"boo\n");
        stage_file(&repo, "ghost.txt").unwrap();
        restore_snapshot(&repo, &snap).unwrap();
        assert!(!repo.workdir().unwrap().join("ghost.txt").exists());
        assert!(!in_index(&repo, "ghost.txt"));
        assert_eq!(state_of(&repo, "ghost.txt"), None);
        // Restoring twice is harmless.
        restore_snapshot(&repo, &snap).unwrap();
    }

    #[test]
    fn snapshot_refuses_directories() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "sub/a.txt", "a\n", "init");
        let err = snapshot_path(&repo, "sub").unwrap_err();
        match err {
            GitError::Discard(msg) => assert!(msg.contains("directory"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn line_helpers_split_on_newlines_only() {
        let data = b"a\r\nb\nc";
        assert_eq!(take_lines(data, 0, 1), Some(&b"a\r\n"[..]));
        assert_eq!(take_lines(data, 1, 2), Some(&b"b\nc"[..]));
        assert_eq!(take_lines(data, 2, 1), Some(&b"c"[..]));
        assert_eq!(take_lines(data, 3, 0), Some(&b""[..]));
        // Asking for lines that are not there is an error, not a clamp.
        assert_eq!(take_lines(data, 2, 2), None);
        assert_eq!(take_lines(data, 4, 1), None);
        assert_eq!(take_lines(b"a\n", 0, 2), None);
        assert_eq!(
            splice_lines(data, 1, 1, b"X\r\nY\r\n").unwrap(),
            b"a\r\nX\r\nY\r\nc"
        );
        assert_eq!(splice_lines(data, 0, 3, b"").unwrap(), b"");
        assert_eq!(lf_to_crlf(b"a\nb\r\nc"), b"a\r\nb\r\nc");
    }
}
