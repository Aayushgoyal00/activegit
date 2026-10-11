//! Status model per `docs/architecture.md`.

use crate::error::GitError;
use crate::repo::{branch_name, head_summary};

/// The home-screen model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStatus {
    pub branch: String,
    pub head_summary: String,
    pub files: Vec<StatusEntry>,
    /// Every tracked path (index order, sorted), including unchanged files.
    /// The TUI unions this with `files` so clean files are browsable too;
    /// anything here but not in `files` is [`FileState::Clean`].
    pub tracked_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Repo-relative path, forward slashes.
    pub path: String,
    pub state: FileState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Staged,
    Unstaged,
    Untracked,
    Conflicted,
    BothStagedAndUnstaged,
    /// Tracked and identical in workdir, index, and HEAD. Never produced by
    /// `repo_status` itself — the TUI derives it from `tracked_files`.
    Clean,
}

/// Build `RepoStatus` from `git2::Repository::statuses` in one pass.
pub fn repo_status(repo: &git2::Repository) -> Result<RepoStatus, GitError> {
    let branch = branch_name(repo)?;
    let head_summary = head_summary(repo)?;

    // No rename detection: a staged rename is listed as what the index
    // really holds, a deletion of the old path plus an addition of the
    // new one. An entry here is just a path and a state, and stage /
    // unstage / discard act on that one path; a paired-up rename would be
    // reported under the old path (libgit2's choice), which no longer
    // exists, while the new path looked clean.
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .include_unmodified(false)
        .renames_head_to_index(false);

    let statuses = repo.statuses(Some(&mut opts))?;
    let mut files = Vec::with_capacity(statuses.len());
    for entry in statuses.iter() {
        let Some(path) = entry.path() else { continue };
        let flags = entry.status();
        if let Some(state) = classify(flags) {
            files.push(StatusEntry {
                path: path.to_string(),
                state,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let tracked_files = {
        let index = repo.index()?;
        let mut paths: Vec<String> = index
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    };
    Ok(RepoStatus {
        branch,
        head_summary,
        files,
        tracked_files,
    })
}

fn classify(flags: git2::Status) -> Option<FileState> {
    use git2::Status as S;
    if flags.contains(S::CONFLICTED) {
        return Some(FileState::Conflicted);
    }
    // Untracked: only WT_NEW, nothing in the index.
    let index_bits = S::INDEX_NEW
        | S::INDEX_MODIFIED
        | S::INDEX_DELETED
        | S::INDEX_RENAMED
        | S::INDEX_TYPECHANGE;
    let workdir_bits = S::WT_MODIFIED | S::WT_DELETED | S::WT_RENAMED | S::WT_TYPECHANGE;
    let staged = flags.intersects(index_bits);
    let unstaged = flags.intersects(workdir_bits);
    let wt_new = flags.contains(S::WT_NEW);

    if !staged && !unstaged {
        if wt_new {
            return Some(FileState::Untracked);
        }
        return None;
    }
    if staged && (unstaged || wt_new) {
        return Some(FileState::BothStagedAndUnstaged);
    }
    if staged {
        return Some(FileState::Staged);
    }
    // unstaged (tracked modification). WT_NEW alongside nothing else was
    // handled above, so reaching here with wt_new alone is untracked.
    if unstaged {
        return Some(FileState::Unstaged);
    }
    if wt_new {
        return Some(FileState::Untracked);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use std::fs;
    use std::path::Path;

    fn state_of(status: &RepoStatus, path: &str) -> Option<FileState> {
        status
            .files
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.state)
    }

    #[test]
    fn lists_untracked_files() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        fs::write(repo.workdir().unwrap().join("new.txt"), "new\n").unwrap();
        let st = repo_status(&repo).unwrap();
        assert_eq!(state_of(&st, "new.txt"), Some(FileState::Untracked));
    }

    #[test]
    fn lists_unstaged_modifications() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let st = repo_status(&repo).unwrap();
        assert_eq!(state_of(&st, "a.txt"), Some(FileState::Unstaged));
    }

    #[test]
    fn lists_staged_modifications() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let st = repo_status(&repo).unwrap();
        assert_eq!(state_of(&st, "a.txt"), Some(FileState::Staged));
    }

    #[test]
    fn file_modified_in_index_and_workdir_is_both_staged_and_unstaged() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "staged\n");
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        testutil::dirty_file(&repo, "a.txt", "unstaged\n");
        let st = repo_status(&repo).unwrap();
        assert_eq!(
            state_of(&st, "a.txt"),
            Some(FileState::BothStagedAndUnstaged)
        );
    }

    #[test]
    fn unmerged_paths_reported_as_conflicted() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "base\n", "base");
        // Branch with "side" change.
        testutil::new_branch(&repo, "side");
        fs::write(repo.workdir().unwrap().join("a.txt"), "side change\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let sig = repo.signature().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "side", &tree, &[&head])
            .unwrap();
        // Back to main, conflicting change.
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        fs::write(repo.workdir().unwrap().join("a.txt"), "main change\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "main", &tree, &[&head])
            .unwrap();
        // Merge side into main -> conflict in the index.
        let side_ref = repo.find_reference("refs/heads/side").unwrap();
        let annotated = repo.reference_to_annotated_commit(&side_ref).unwrap();
        // git_merge leaves conflicts in the index; it may return Ok.
        let _ = repo.merge(&[&annotated], None, None);
        assert!(
            repo.index().unwrap().has_conflicts(),
            "expected merge conflicts in index"
        );
        let st = repo_status(&repo).unwrap();
        let conflicted: Vec<_> = st
            .files
            .iter()
            .filter(|e| e.state == FileState::Conflicted)
            .collect();
        assert!(
            conflicted.iter().any(|e| e.path == "a.txt"),
            "expected a.txt conflicted, got: {:?}",
            st.files
        );
    }

    /// `git mv old.txt new.txt`: move the file and stage both sides.
    fn stage_rename(repo: &git2::Repository, from: &str, to: &str) {
        let workdir = repo.workdir().unwrap();
        fs::rename(workdir.join(from), workdir.join(to)).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new(from)).unwrap();
        index.add_path(Path::new(to)).unwrap();
        index.write().unwrap();
    }

    #[test]
    fn staged_rename_is_reported_under_paths_that_can_be_acted_on() {
        let (_dir, repo) = testutil::init_repo();
        let body = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
        testutil::commit_file(&repo, "old.txt", &body, "init");
        let workdir = repo.workdir().unwrap().to_path_buf();
        stage_rename(&repo, "old.txt", "new.txt");

        // The new path is listed as staged and is really there; the old
        // one is listed as the staged deletion it is.
        let st = repo_status(&repo).unwrap();
        assert_eq!(state_of(&st, "new.txt"), Some(FileState::Staged));
        assert!(workdir.join("new.txt").exists());
        assert_eq!(state_of(&st, "old.txt"), Some(FileState::Staged));
        assert_eq!(st.files.len(), 2, "got {:?}", st.files);

        // Unstaging each reported entry works and undoes the staging.
        for entry in &st.files {
            crate::stage::unstage_file(&repo, &entry.path).unwrap();
        }
        let st = repo_status(&repo).unwrap();
        assert_eq!(state_of(&st, "new.txt"), Some(FileState::Untracked));
        assert_eq!(state_of(&st, "old.txt"), Some(FileState::Unstaged));

        // Stage the rename again, then discard each reported entry: the
        // repository is back to the commit.
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("old.txt")).unwrap();
        index.add_path(Path::new("new.txt")).unwrap();
        index.write().unwrap();
        let st = repo_status(&repo).unwrap();
        assert_eq!(st.files.len(), 2, "got {:?}", st.files);
        for entry in &st.files {
            crate::stage::discard_file(&repo, &entry.path).unwrap();
        }
        assert!(repo_status(&repo).unwrap().files.is_empty());
        assert!(!workdir.join("new.txt").exists());
        assert_eq!(fs::read_to_string(workdir.join("old.txt")).unwrap(), body);
    }

    #[test]
    fn empty_repo_reports_zero_files_and_no_panic() {
        let (_dir, repo) = testutil::init_repo();
        let st = repo_status(&repo).unwrap();
        assert!(st.files.is_empty());
        assert!(st.tracked_files.is_empty());
        assert_eq!(st.branch, "main");
    }

    #[test]
    fn tracked_files_lists_clean_files_status_omits_them() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::commit_file(&repo, "sub/b.txt", "b\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let st = repo_status(&repo).unwrap();
        // Changed files keep their state…
        assert_eq!(state_of(&st, "a.txt"), Some(FileState::Unstaged));
        assert!(state_of(&st, "sub/b.txt").is_none());
        // …while every tracked path is listed for browsing.
        assert_eq!(
            st.tracked_files,
            vec!["a.txt".to_string(), "sub/b.txt".to_string()]
        );
    }
}
