//! Test fixtures for Phase 1. Only compiled for tests.
#![cfg(test)]

use git2::{Oid, Repository};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

/// Env vars are process-global and `cargo test` runs threads in parallel:
/// every test that reads or writes LLM key vars must hold this first.
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Init a new repo in a temp dir, default branch `main`, local user config set.
pub fn init_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().expect("create tempdir");
    let repo = Repository::init(dir.path()).expect("init repo");
    // Point unborn HEAD at main so the first commit lands on `main`.
    repo.set_head("refs/heads/main").expect("set head to main");
    {
        let mut cfg = repo.config().expect("open config");
        cfg.set_str("user.name", "Test User")
            .expect("set user.name");
        cfg.set_str("user.email", "test@example.com")
            .expect("set user.email");
        cfg.set_str("init.defaultBranch", "main").ok();
        cfg.set_str("commit.gpgsign", "false").ok();
        // Git for Windows enables autocrlf system-wide; keep checkouts byte-exact.
        cfg.set_str("core.autocrlf", "false").ok();
    }
    (dir, repo)
}

/// Write file, stage it, commit. Returns the new commit id.
pub fn commit_file(repo: &Repository, path: &str, contents: &str, msg: &str) -> Oid {
    commit_bytes(repo, path, contents.as_bytes(), msg)
}

/// [`commit_file`] for content that is not valid UTF-8.
pub fn commit_bytes(repo: &Repository, path: &str, contents: &[u8], msg: &str) -> Oid {
    write_workdir_file(repo, Path::new(path), contents);
    let mut index = repo.index().expect("open index");
    index
        .add_path(Path::new(path))
        .unwrap_or_else(|e| panic!("stage {path}: {e}"));
    index.write().expect("write index");
    write_index_as_commit(repo, msg)
}

/// Delete a tracked file from the workdir and the index, commit. Returns
/// the new commit id.
pub fn commit_removal(repo: &Repository, path: &str, msg: &str) -> Oid {
    fs::remove_file(repo.workdir().expect("workdir").join(path))
        .unwrap_or_else(|e| panic!("remove {path}: {e}"));
    let mut index = repo.index().expect("open index");
    index
        .remove_path(Path::new(path))
        .unwrap_or_else(|e| panic!("unstage {path}: {e}"));
    index.write().expect("write index");
    write_index_as_commit(repo, msg)
}

/// Force-checkout an existing local branch (fixture setup only: the real
/// thing is `branch::checkout_branch`).
pub fn switch_branch(repo: &Repository, name: &str) {
    repo.set_head(&format!("refs/heads/{name}"))
        .expect("set head to branch");
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .expect("checkout branch");
}

/// Start merging local branch `name` into HEAD the way `git merge` does,
/// stopping before the commit: MERGE_HEAD is set, the result (or the
/// conflict) is in the index and the workdir.
pub fn start_merge(repo: &Repository, name: &str) {
    let reference = repo
        .find_reference(&format!("refs/heads/{name}"))
        .expect("branch to merge");
    let annotated = repo
        .reference_to_annotated_commit(&reference)
        .expect("annotated commit");
    // Conflicts are left in the index; libgit2 still returns Ok.
    repo.merge(&[&annotated], None, None).expect("start merge");
}

/// Repo on `main` in the middle of a merge of `side` that conflicts in
/// `a.txt` (base `base`, main `main change`, side `side change`); `side`
/// also adds `side.txt`, which merges cleanly.
pub fn conflicted_merge() -> (TempDir, Repository) {
    let (dir, repo) = init_repo();
    commit_file(&repo, "a.txt", "base\n", "base");
    new_branch(&repo, "side");
    commit_file(&repo, "a.txt", "side change\n", "side");
    commit_file(&repo, "side.txt", "side only\n", "side file");
    switch_branch(&repo, "main");
    commit_file(&repo, "a.txt", "main change\n", "main");
    start_merge(&repo, "side");
    assert!(
        repo.index().expect("open index").has_conflicts(),
        "fixture should leave a conflict"
    );
    (dir, repo)
}

/// Modify a workdir file without staging (append).
pub fn dirty_file(repo: &Repository, path: &str, append_contents: &str) {
    let full = repo.workdir().expect("workdir").join(path);
    let mut f = OpenOptions::new()
        .append(true)
        .open(&full)
        .unwrap_or_else(|e| panic!("open {path} for append: {e}"));
    f.write_all(append_contents.as_bytes())
        .expect("append to file");
}

/// Clone a local repo (file path) into a fresh temp dir. The clone keeps
/// the `origin` remote pointing at `src`, so push/pull tests run offline.
pub fn clone_local(src: &Path) -> (TempDir, Repository) {
    let dir = TempDir::new().expect("create tempdir");
    let repo = Repository::clone(src.to_str().expect("src is utf-8"), dir.path())
        .expect("clone local repo");
    {
        let mut cfg = repo.config().expect("open config");
        cfg.set_str("user.name", "Test User")
            .expect("set user.name");
        cfg.set_str("user.email", "test@example.com")
            .expect("set user.email");
        cfg.set_str("commit.gpgsign", "false").ok();
        // Git for Windows enables autocrlf system-wide; keep checkouts byte-exact.
        cfg.set_str("core.autocrlf", "false").ok();
    }
    // The clone's own checkout ran before that setting existed, so where
    // autocrlf is on system-wide the files came out CRLF and now count as
    // modified. Reopen (the handle cached the old setting) and check out
    // again from an empty index, so every file is really rewritten.
    drop(repo);
    let repo = Repository::open(dir.path()).expect("reopen clone");
    if repo.head().is_ok() {
        let mut index = repo.index().expect("open index");
        index.clear().expect("clear index");
        index.write().expect("write index");
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .expect("re-checkout clone");
    }
    (dir, repo)
}

/// Create branch `name` at HEAD and check it out.
pub fn new_branch(repo: &Repository, name: &str) {
    let head = repo.head().expect("HEAD exists for new_branch");
    let commit = head.peel_to_commit().expect("peel to commit");
    repo.branch(name, &commit, false).expect("create branch");
    repo.set_head(&format!("refs/heads/{name}"))
        .expect("set head to new branch");
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .expect("checkout new branch");
}

fn write_workdir_file(repo: &Repository, rel: &Path, contents: &[u8]) {
    let full = repo.workdir().expect("workdir").join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(&full, contents).expect("write file");
}

/// Commit whatever is staged (for fixtures that stage several paths
/// themselves). Returns the new commit id.
pub fn commit_index(repo: &Repository, msg: &str) -> Oid {
    write_index_as_commit(repo, msg)
}

fn write_index_as_commit(repo: &Repository, msg: &str) -> Oid {
    let mut index = repo.index().expect("open index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let sig = repo.signature().expect("signature");
    let parents: Vec<git2::Commit> = match repo.head() {
        Ok(head) if head.peel_to_commit().is_ok() => {
            vec![head.peel_to_commit().expect("head commit")]
        }
        _ => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
        .expect("create commit")
}
