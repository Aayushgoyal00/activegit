//! `Repo`: owning wrapper around `git2::Repository`.
//!
//! The TUI holds `Repo`, never a `git2::*` type directly.

use crate::diff::{self, FileDiff};
use crate::error::GitError;
use crate::status::{self, RepoStatus};
use std::path::Path;

/// Owning handle to a git repository.
pub struct Repo {
    inner: git2::Repository,
}

impl Repo {
    /// Open a repository at exactly `path` (must contain `.git` or be one).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, GitError> {
        let inner = git2::Repository::open(path.as_ref())
            .map_err(|_| GitError::NotARepo(path.as_ref().display().to_string()))?;
        Ok(Self { inner })
    }

    /// Init a new repository at `path` (creates the dir if missing).
    /// Idempotent on an existing repo; a fresh repo points unborn HEAD at
    /// `main` so the first commit lands there and `branch()` reports `main`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self, GitError> {
        let p = path.as_ref();
        if let Err(e) = std::fs::create_dir_all(p) {
            return Err(GitError::HunkStaging(format!(
                "cannot create directory {}: {e}",
                p.display()
            )));
        }
        let inner = git2::Repository::init(p)?;
        if inner.head().is_err() {
            // Unborn repo: default to `main`. Ignore failure (e.g. HEAD
            // already set by global init.defaultBranch template).
            let _ = inner.set_head("refs/heads/main");
        }
        Ok(Self { inner })
    }

    /// Normalize raw user input into a path to discover from.
    /// Trims whitespace, rejects empty input, expands a leading `~` to
    /// [`home_dir`], and rejects paths that do not exist.
    pub fn normalize_project_input(raw: &str) -> Result<std::path::PathBuf, GitError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(GitError::EmptyPath);
        }
        let is_tilde = trimmed == "~"
            || trimmed.starts_with("~/")
            || (cfg!(windows) && trimmed.starts_with("~\\"));
        let expanded = match home_dir() {
            Some(home) if is_tilde => format!("{}{}", home.display(), &trimmed[1..]),
            _ => trimmed.to_string(),
        };
        let path = std::path::PathBuf::from(expanded);
        if !path.exists() {
            return Err(GitError::NotFound(path.display().to_string()));
        }
        Ok(path)
    }

    /// Discover the workdir root for a user-supplied path. Maps every
    /// edge case to a typed [`GitError`]: empty input, missing path,
    /// outside-a-repo, and bare repos.
    pub fn discover_root(input: impl AsRef<Path>) -> Result<std::path::PathBuf, GitError> {
        let start = input.as_ref().to_path_buf();
        let repo = git2::Repository::discover(&start)
            .map_err(|_| GitError::NotARepo(start.display().to_string()))?;
        repo.workdir()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| GitError::BareRepo(start.display().to_string()))
    }

    /// Walk upward from `path` to find `.git`.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self, GitError> {
        let start = path.as_ref().to_path_buf();
        let inner = git2::Repository::discover(&start)
            .map_err(|_| GitError::NotARepo(start.display().to_string()))?;
        Ok(Self { inner })
    }

    /// Borrow the underlying `git2::Repository` (crate-internal).
    #[allow(dead_code)]
    pub(crate) fn inner(&self) -> &git2::Repository {
        &self.inner
    }

    /// Construct from an already-open `git2::Repository` (tests / job thread).
    pub fn from_inner(inner: git2::Repository) -> Self {
        Self { inner }
    }

    /// Workdir root, if this is not a bare repo. Lets the TUI discover the
    /// root and hand it to the job thread without touching `git2` types.
    pub fn workdir(&self) -> Option<std::path::PathBuf> {
        self.inner.workdir().map(|p| p.to_path_buf())
    }

    /// Current branch short name, or `"(detached HEAD)"`.
    pub fn branch(&self) -> Result<String, GitError> {
        branch_name(&self.inner)
    }

    /// One-line HEAD commit summary, or a placeholder when unborn.
    pub fn head_summary(&self) -> Result<String, GitError> {
        head_summary(&self.inner)
    }

    pub fn status(&self) -> Result<RepoStatus, GitError> {
        status::repo_status(&self.inner)
    }

    pub fn unstaged_diff(&self, path: &str) -> Result<FileDiff, GitError> {
        diff::unstaged_diff(&self.inner, path)
    }

    pub fn staged_diff(&self, path: &str) -> Result<FileDiff, GitError> {
        diff::staged_diff(&self.inner, path)
    }

    /// Whole file content for viewing files with no changes.
    pub fn whole_file(&self, path: &str) -> Result<FileDiff, GitError> {
        diff::whole_file_diff(&self.inner, path)
    }

    /// Full new-version text for Markdown preview.
    pub fn new_content(&self, path: &str, staged: bool) -> Result<String, GitError> {
        diff::new_content(&self.inner, path, staged)
    }

    pub fn stage_file(&self, path: &str) -> Result<(), GitError> {
        crate::stage::stage_file(&self.inner, path)
    }

    pub fn unstage_file(&self, path: &str) -> Result<(), GitError> {
        crate::stage::unstage_file(&self.inner, path)
    }

    pub fn discard_file(&self, path: &str) -> Result<(), GitError> {
        crate::stage::discard_file(&self.inner, path)
    }

    pub fn stage_hunk(&self, path: &str, hunk_index: usize) -> Result<(), GitError> {
        crate::stage::stage_hunk(&self.inner, path, hunk_index)
    }

    /// Restore a single hunk (`staged = false` reverts the workdir hunk,
    /// `staged = true` reverts the index hunk toward HEAD).
    pub fn discard_hunk(
        &self,
        path: &str,
        hunk_index: usize,
        staged: bool,
    ) -> Result<(), GitError> {
        crate::stage::discard_hunk(&self.inner, path, hunk_index, staged)
    }

    pub fn commit(&self, message: &str) -> Result<git2::Oid, GitError> {
        crate::commit::commit(&self.inner, message)
    }

    /// Replace the HEAD commit with one made from the current index and
    /// `message`.
    pub fn amend(&self, message: &str) -> Result<git2::Oid, GitError> {
        crate::commit::amend(&self.inner, message)
    }

    /// Full message of the HEAD commit, for prefilling an amend box.
    pub fn last_commit_message(&self) -> Option<String> {
        crate::commit::last_commit_message(&self.inner)
    }

    /// Which merge / rebase / cherry-pick / … is in progress, if any.
    pub fn state(&self) -> RepoState {
        repo_state(&self.inner)
    }

    /// Record `path` before a destructive action so it can be undone.
    pub fn snapshot_path(&self, path: &str) -> Result<crate::stage::Snapshot, GitError> {
        crate::stage::snapshot_path(&self.inner, path)
    }

    /// Undo a destructive action using a snapshot taken before it.
    pub fn restore_snapshot(&self, snap: &crate::stage::Snapshot) -> Result<(), GitError> {
        crate::stage::restore_snapshot(&self.inner, snap)
    }

    /// Paths changed by commit `oid`, in diff order.
    pub fn commit_files(&self, oid: &str) -> Result<Vec<String>, GitError> {
        diff::commit_files(&self.inner, oid)
    }

    /// Diff of `path` as changed by commit `oid`.
    pub fn commit_file_diff(&self, oid: &str, path: &str) -> Result<FileDiff, GitError> {
        diff::commit_file_diff(&self.inner, oid, path)
    }

    pub fn list_branches(&self) -> Result<Vec<crate::branch::BranchInfo>, GitError> {
        crate::branch::list_branches(&self.inner)
    }

    pub fn create_branch(&self, name: &str) -> Result<(), GitError> {
        crate::branch::create_branch(&self.inner, name)
    }

    pub fn checkout_branch(&self, name: &str) -> Result<(), GitError> {
        crate::branch::checkout_branch(&self.inner, name)
    }

    pub fn delete_branch(&self, name: &str) -> Result<(), GitError> {
        crate::branch::delete_branch(&self.inner, name)
    }

    pub fn log(&self, limit: usize) -> Result<Vec<crate::log::CommitInfo>, GitError> {
        crate::log::log(&self.inner, limit)
    }

    pub fn commit_overview(&self, oid: &str) -> Result<crate::log::CommitOverview, GitError> {
        crate::log::commit_overview(&self.inner, oid)
    }

    pub fn list_stash(&mut self) -> Result<Vec<crate::stash::StashEntry>, GitError> {
        crate::stash::list_stash(&mut self.inner)
    }

    pub fn stash_push(&mut self, message: &str) -> Result<(), GitError> {
        crate::stash::stash_push(&mut self.inner, message)
    }

    pub fn stash_pop(&mut self, index: usize) -> Result<(), GitError> {
        crate::stash::stash_pop(&mut self.inner, index)
    }

    pub fn stash_drop(&mut self, index: usize) -> Result<(), GitError> {
        crate::stash::stash_drop(&mut self.inner, index)
    }
}

/// The user's home directory: `$HOME`, falling back to `%USERPROFILE%` on
/// Windows (where `HOME` is usually unset). `None` when neither is set.
pub fn home_dir() -> Option<std::path::PathBuf> {
    let from = |var: &str| {
        std::env::var_os(var)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    };
    from("HOME").or_else(|| from("USERPROFILE").filter(|_| cfg!(windows)))
}

/// Free function so tests using raw `git2::Repository` don't need the wrapper.
pub fn branch_name(repo: &git2::Repository) -> Result<String, GitError> {
    if repo.head_detached().unwrap_or(false) {
        return Ok("(detached HEAD)".to_string());
    }
    match repo.head() {
        Ok(head) => {
            if head.is_branch() {
                Ok(head.shorthand().unwrap_or("HEAD").to_string())
            } else if repo.head_detached().unwrap_or(false) {
                Ok("(detached HEAD)".to_string())
            } else {
                // Unborn or otherwise non-branch HEAD: read symbolic target.
                unborn_branch_name(repo)
            }
        }
        Err(_) => unborn_branch_name(repo),
    }
}

fn unborn_branch_name(repo: &git2::Repository) -> Result<String, GitError> {
    // HEAD exists as a symbolic ref like refs/heads/main even with no commits.
    if let Ok(head_ref) = repo.find_reference("HEAD") {
        if let Some(target) = head_ref.symbolic_target() {
            if let Some(name) = target.strip_prefix("refs/heads/") {
                return Ok(name.to_string());
            }
        }
    }
    Ok("(detached HEAD)".to_string())
}

/// Free function version of [`Repo::head_summary`].
pub fn head_summary(repo: &git2::Repository) -> Result<String, GitError> {
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok("(no commits yet)".to_string()),
    };
    let commit = match head.peel_to_commit() {
        Ok(c) => c,
        Err(_) => return Ok("(no commits yet)".to_string()),
    };
    let msg = commit
        .message()
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .trim();
    if msg.is_empty() {
        Ok("(empty message)".to_string())
    } else {
        Ok(msg.to_string())
    }
}

/// The multi-step operation a repository is in the middle of, if any.
/// While one is in progress the TUI offers continue / abort instead of
/// treating the leftover conflicts as ordinary changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoState {
    /// Nothing in progress.
    Clean,
    /// A merge stopped before its commit (conflicts, or `--no-commit`).
    Merge,
    /// Any flavour of rebase (apply, merge or interactive backend).
    Rebase,
    /// A cherry-pick, single or part of a sequence.
    CherryPick,
    /// A revert, single or part of a sequence.
    Revert,
    Bisect,
    /// Something else git tracks on disk (e.g. `git am`).
    Other,
}

/// Which operation `repo` is in the middle of. A pure read of the
/// marker files under `.git`, so it also sees operations started from a
/// terminal.
pub fn repo_state(repo: &git2::Repository) -> RepoState {
    use git2::RepositoryState as S;
    match repo.state() {
        S::Clean => RepoState::Clean,
        S::Merge => RepoState::Merge,
        S::Rebase | S::RebaseInteractive | S::RebaseMerge => RepoState::Rebase,
        S::CherryPick | S::CherryPickSequence => RepoState::CherryPick,
        S::Revert | S::RevertSequence => RepoState::Revert,
        S::Bisect => RepoState::Bisect,
        S::ApplyMailbox | S::ApplyMailboxOrRebase => RepoState::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn repo_state_reports_merge_in_progress() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        assert_eq!(repo_state(&repo), RepoState::Clean);

        let (_dir, repo) = testutil::conflicted_merge();
        assert_eq!(repo_state(&repo), RepoState::Merge);
        assert_eq!(Repo::from_inner(repo).state(), RepoState::Merge);
    }

    #[test]
    fn repo_state_maps_marker_files() {
        // The other states are just files git leaves under `.git`.
        let (_dir, repo) = testutil::init_repo();
        let oid = testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let git_dir = repo.path().to_path_buf();
        let mark = |name: &str| std::fs::write(git_dir.join(name), format!("{oid}\n")).unwrap();
        let unmark = |name: &str| std::fs::remove_file(git_dir.join(name)).unwrap();

        mark("CHERRY_PICK_HEAD");
        assert_eq!(repo_state(&repo), RepoState::CherryPick);
        unmark("CHERRY_PICK_HEAD");

        mark("REVERT_HEAD");
        assert_eq!(repo_state(&repo), RepoState::Revert);
        unmark("REVERT_HEAD");

        mark("BISECT_LOG");
        assert_eq!(repo_state(&repo), RepoState::Bisect);
        unmark("BISECT_LOG");

        std::fs::create_dir(git_dir.join("rebase-merge")).unwrap();
        assert_eq!(repo_state(&repo), RepoState::Rebase);
        std::fs::remove_dir(git_dir.join("rebase-merge")).unwrap();

        assert_eq!(repo_state(&repo), RepoState::Clean);
    }

    #[test]
    fn discovers_repo_from_nested_subdir() {
        let (dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "hello\n", "init");
        let nested = dir.path().join("sub").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        let found = Repo::discover(&nested).expect("discover from nested subdir");
        // Same repo: workdir should match.
        assert_eq!(found.inner.workdir().unwrap(), repo.workdir().unwrap());
    }

    #[test]
    fn reports_detached_head() {
        let (_dir, repo) = testutil::init_repo();
        let oid = testutil::commit_file(&repo, "a.txt", "hello\n", "init");
        repo.set_head_detached(oid).unwrap();
        {
            let obj = repo.find_object(oid, None).unwrap();
            repo.checkout_tree(&obj, None).unwrap();
        }
        let r = Repo::from_inner(repo);
        assert_eq!(r.branch().unwrap(), "(detached HEAD)");
    }

    #[test]
    fn reports_branch_name_on_main() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "x\n", "init");
        let r = Repo::from_inner(repo);
        assert_eq!(r.branch().unwrap(), "main");
    }

    #[test]
    fn unborn_repo_does_not_panic() {
        let (_dir, repo) = testutil::init_repo();
        let r = Repo::from_inner(repo);
        assert_eq!(r.branch().unwrap(), "main");
        assert_eq!(r.head_summary().unwrap(), "(no commits yet)");
    }

    /// libgit2 reports long-form paths with `/` separators, while the temp
    /// dir may be a Windows 8.3 short path (`RUNNER~1`); compare resolved.
    fn assert_same_dir(a: &Path, b: &Path) {
        assert_eq!(
            std::fs::canonicalize(a).unwrap(),
            std::fs::canonicalize(b).unwrap()
        );
    }

    #[test]
    fn workdir_returns_repo_root_for_tui_bootstrap() {
        let (dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "x\n", "init");
        let r = Repo::from_inner(repo);
        assert_same_dir(&r.workdir().unwrap(), dir.path());
    }

    #[test]
    fn normalize_rejects_empty_and_missing_paths() {
        assert!(matches!(
            Repo::normalize_project_input(""),
            Err(GitError::EmptyPath)
        ));
        assert!(matches!(
            Repo::normalize_project_input("   "),
            Err(GitError::EmptyPath)
        ));
        assert!(matches!(
            Repo::normalize_project_input("/definitely/not/here-xyz-123"),
            Err(GitError::NotFound(_))
        ));
    }

    #[test]
    fn normalize_trims_and_accepts_existing_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let padded = format!("  {}  ", dir.path().display());
        assert_eq!(
            Repo::normalize_project_input(&padded).unwrap(),
            dir.path().to_path_buf()
        );
    }

    #[test]
    fn discover_root_maps_bare_and_non_repos() {
        // Plain dir: NotARepo.
        let plain = tempfile::TempDir::new().unwrap();
        assert!(matches!(
            Repo::discover_root(plain.path()),
            Err(GitError::NotARepo(_))
        ));
        // Bare repo: BareRepo.
        let bare_dir = tempfile::TempDir::new().unwrap();
        let bare_path = bare_dir.path().join("bare.git");
        git2::Repository::init_bare(&bare_path).unwrap();
        assert!(matches!(
            Repo::discover_root(&bare_path),
            Err(GitError::BareRepo(_))
        ));
    }

    #[test]
    fn init_creates_repo_pointing_at_main() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("fresh");
        let r = Repo::init(&target).unwrap();
        assert!(target.join(".git").exists());
        assert_eq!(r.branch().unwrap(), "main");
        assert_eq!(r.head_summary().unwrap(), "(no commits yet)");
        // Discoverable from a nested subdir, and status works on the
        // empty (unborn, no-file) project.
        let nested = target.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        assert_same_dir(&Repo::discover_root(&nested).unwrap(), &target);
        assert!(r.status().unwrap().files.is_empty());
    }
}
