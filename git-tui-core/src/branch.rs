//! Local branch operations.

use crate::error::GitError;
use std::cell::RefCell;

/// Owned branch summary — sendable across the job channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    /// One-line tip commit summary.
    pub tip_summary: String,
}

/// Sorted local branches, HEAD flagged.
pub fn list_branches(repo: &git2::Repository) -> Result<Vec<BranchInfo>, GitError> {
    let mut out = Vec::new();
    let branches = repo
        .branches(Some(git2::BranchType::Local))
        .map_err(|e| GitError::Branch(format!("cannot list branches: {e}")))?;
    for item in branches {
        let (branch, _) = item.map_err(|e| GitError::Branch(format!("cannot read branch: {e}")))?;
        let name = branch
            .name()
            .map_err(|e| GitError::Branch(format!("cannot read branch name: {e}")))?
            .unwrap_or("(invalid)")
            .to_string();
        let is_head = branch.is_head();
        let tip_summary = branch
            .get()
            .peel_to_commit()
            .ok()
            .and_then(|c| {
                c.message()
                    .map(|m| m.lines().next().unwrap_or("").trim().to_string())
            })
            .unwrap_or_default();
        out.push(BranchInfo {
            name,
            is_head,
            tip_summary,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Create branch `name` at HEAD.
pub fn create_branch(repo: &git2::Repository, name: &str) -> Result<(), GitError> {
    if name.trim().is_empty() {
        return Err(GitError::Branch("branch name is empty".into()));
    }
    let head = repo
        .head()
        .map_err(|_| GitError::Branch("cannot create branch: no commits yet".into()))?
        .peel_to_commit()
        .map_err(|e| GitError::Branch(format!("cannot read HEAD: {e}")))?;
    repo.branch(name, &head, false)
        .map(|_| ())
        .map_err(|e| GitError::Branch(format!("cannot create branch {name}: {e}")))?;
    Ok(())
}

/// Check out branch `name` (safe checkout: errors on dirty overwrite).
///
/// Order matters: the working tree and index are moved to the target
/// tree *first*, while HEAD still names the old branch, and HEAD is only
/// repointed once that succeeded. Moving HEAD first makes libgit2's safe
/// checkout see the old branch's files as local modifications and keep
/// them, leaving the new branch name over the old branch's content.
///
/// Like `git checkout`, local changes that do not collide with the
/// switch carry over; ones that do fail the checkout and nothing moves.
pub fn checkout_branch(repo: &git2::Repository, name: &str) -> Result<(), GitError> {
    let branch = repo
        .find_branch(name, git2::BranchType::Local)
        .map_err(|_| GitError::Branch(format!("no such branch: {name}")))?;
    let refname = branch
        .get()
        .name()
        .ok_or_else(|| GitError::Branch(format!("invalid branch ref: {name}")))?
        .to_string();
    let target = branch
        .get()
        .peel(git2::ObjectType::Commit)
        .map_err(|e| GitError::Branch(format!("cannot checkout {name}: {e}")))?;

    // Collect the paths that block the switch so the error can name them
    // (libgit2 itself only says "1 conflict prevents checkout").
    let blocked: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let result = {
        let mut builder = git2::build::CheckoutBuilder::new();
        builder
            .safe()
            .notify_on(git2::CheckoutNotificationType::CONFLICT)
            .notify(|_why, path, _baseline, _target, _workdir| {
                if let Some(p) = path {
                    blocked.borrow_mut().push(p.display().to_string());
                }
                true
            });
        repo.checkout_tree(&target, Some(&mut builder))
    };
    if let Err(e) = result {
        let blocked = blocked.into_inner();
        if blocked.is_empty() {
            return Err(GitError::Branch(format!("cannot checkout {name}: {e}")));
        }
        return Err(GitError::Branch(format!(
            "cannot checkout {name}: local changes to {} would be overwritten \
             (commit, stash or discard them first)",
            summarize_paths(&blocked)
        )));
    }
    repo.set_head(&refname)
        .map_err(|e| GitError::Branch(format!("cannot checkout {name}: {e}")))?;
    Ok(())
}

/// `a, b, c` for an error line, capped so a huge list stays readable.
fn summarize_paths(paths: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut out = paths
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > SHOWN {
        out.push_str(&format!(" and {} more", paths.len() - SHOWN));
    }
    out
}

/// Delete a branch. Refuses the checked-out branch.
pub fn delete_branch(repo: &git2::Repository, name: &str) -> Result<(), GitError> {
    let mut branch = repo
        .find_branch(name, git2::BranchType::Local)
        .map_err(|_| GitError::Branch(format!("no such branch: {name}")))?;
    if branch.is_head() {
        return Err(GitError::Branch(format!(
            "cannot delete {name}: it is checked out"
        )));
    }
    branch
        .delete()
        .map_err(|e| GitError::Branch(format!("cannot delete {name}: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn two_branch_repo() -> (tempfile::TempDir, git2::Repository) {
        let (dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::new_branch(&repo, "side");
        testutil::commit_file(&repo, "a.txt", "a\nside\n", "side commit");
        (dir, repo)
    }

    #[test]
    fn lists_branches_with_head_flagged() {
        let (_dir, repo) = two_branch_repo();
        let branches = list_branches(&repo).unwrap();
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main"), "got {names:?}");
        assert!(names.contains(&"side"), "got {names:?}");
        // Currently on side (new_branch checks out).
        let head = branches.iter().find(|b| b.is_head).unwrap();
        assert_eq!(head.name, "side");
    }

    #[test]
    fn create_and_checkout_moves_head() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        create_branch(&repo, "feature").unwrap();
        assert!(list_branches(&repo)
            .unwrap()
            .iter()
            .any(|b| b.name == "feature"));
        checkout_branch(&repo, "feature").unwrap();
        let head = list_branches(&repo).unwrap();
        assert!(head.iter().find(|b| b.is_head).unwrap().name == "feature");
    }

    #[test]
    fn create_duplicate_branch_errors() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        create_branch(&repo, "dup").unwrap();
        assert!(create_branch(&repo, "dup").is_err());
    }

    #[test]
    fn delete_branch_removes_it() {
        let (_dir, repo) = two_branch_repo();
        // On side; delete main.
        delete_branch(&repo, "main").unwrap();
        let names: Vec<String> = list_branches(&repo)
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert!(!names.contains(&"main".to_string()), "got {names:?}");
    }

    #[test]
    fn delete_checked_out_branch_errors() {
        let (_dir, repo) = two_branch_repo();
        assert!(delete_branch(&repo, "side").is_err());
    }

    #[test]
    fn checkout_nonexistent_branch_errors() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        assert!(checkout_branch(&repo, "nope").is_err());
    }

    /// `main`: a.txt = "main", shared.txt, only_main.txt.
    /// `side`: a.txt = "side", shared.txt, only_side.txt (only_main.txt
    /// removed). Ends on `side`.
    fn diverged_repo() -> (tempfile::TempDir, git2::Repository) {
        let (dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "main\n", "init");
        testutil::commit_file(&repo, "shared.txt", "shared\n", "shared");
        testutil::commit_file(&repo, "only_main.txt", "only main\n", "only main");
        testutil::new_branch(&repo, "side");
        testutil::commit_file(&repo, "a.txt", "side\n", "side a");
        testutil::commit_file(&repo, "only_side.txt", "only side\n", "only side");
        testutil::commit_removal(&repo, "only_main.txt", "drop only_main");
        (dir, repo)
    }

    fn read(repo: &git2::Repository, path: &str) -> String {
        std::fs::read_to_string(repo.workdir().unwrap().join(path)).unwrap()
    }

    fn exists(repo: &git2::Repository, path: &str) -> bool {
        repo.workdir().unwrap().join(path).exists()
    }

    fn head_branch(repo: &git2::Repository) -> String {
        repo.head().unwrap().shorthand().unwrap().to_string()
    }

    fn assert_clean(repo: &git2::Repository) {
        let st = crate::status::repo_status(repo).unwrap();
        assert!(st.files.is_empty(), "expected clean status: {:?}", st.files);
    }

    #[test]
    fn checkout_switches_workdir_and_index_both_ways() {
        let (_dir, repo) = diverged_repo();
        assert_eq!(head_branch(&repo), "side");

        checkout_branch(&repo, "main").unwrap();
        assert_eq!(head_branch(&repo), "main");
        assert_eq!(read(&repo, "a.txt"), "main\n");
        assert_eq!(read(&repo, "only_main.txt"), "only main\n");
        assert!(!exists(&repo, "only_side.txt"));
        assert_eq!(read(&repo, "shared.txt"), "shared\n");
        assert_clean(&repo);

        checkout_branch(&repo, "side").unwrap();
        assert_eq!(head_branch(&repo), "side");
        assert_eq!(read(&repo, "a.txt"), "side\n");
        assert_eq!(read(&repo, "only_side.txt"), "only side\n");
        assert!(!exists(&repo, "only_main.txt"));
        assert_clean(&repo);

        // And once more, so a stale index from the first switch would show.
        checkout_branch(&repo, "main").unwrap();
        assert_eq!(read(&repo, "a.txt"), "main\n");
        assert!(exists(&repo, "only_main.txt"));
        assert!(!exists(&repo, "only_side.txt"));
        assert_clean(&repo);
    }

    #[test]
    fn checkout_with_conflicting_dirty_file_fails_and_keeps_head() {
        let (_dir, repo) = diverged_repo();
        // a.txt differs between the branches: a local edit collides.
        std::fs::write(repo.workdir().unwrap().join("a.txt"), "local edit\n").unwrap();
        let err = checkout_branch(&repo, "main").unwrap_err();
        match err {
            GitError::Branch(msg) => assert!(msg.contains("a.txt"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
        // Nothing moved: still on side, edit intact, side's files in place.
        assert_eq!(head_branch(&repo), "side");
        assert_eq!(read(&repo, "a.txt"), "local edit\n");
        assert!(exists(&repo, "only_side.txt"));
        assert!(!exists(&repo, "only_main.txt"));
        let st = crate::status::repo_status(&repo).unwrap();
        assert_eq!(st.files.len(), 1, "only the local edit: {:?}", st.files);
        assert_eq!(st.files[0].path, "a.txt");
    }

    #[test]
    fn checkout_carries_over_non_conflicting_changes() {
        let (_dir, repo) = diverged_repo();
        // shared.txt is identical on both branches: the edit rides along,
        // and so does an untracked file.
        std::fs::write(repo.workdir().unwrap().join("shared.txt"), "edited\n").unwrap();
        std::fs::write(repo.workdir().unwrap().join("scratch.txt"), "wip\n").unwrap();
        checkout_branch(&repo, "main").unwrap();
        assert_eq!(head_branch(&repo), "main");
        assert_eq!(read(&repo, "a.txt"), "main\n");
        assert_eq!(read(&repo, "shared.txt"), "edited\n");
        assert_eq!(read(&repo, "scratch.txt"), "wip\n");
        let st = crate::status::repo_status(&repo).unwrap();
        let mut paths: Vec<&str> = st.files.iter().map(|e| e.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["scratch.txt", "shared.txt"]);
    }

    #[test]
    fn checkout_refuses_to_overwrite_untracked_file() {
        let (_dir, repo) = diverged_repo();
        // only_main.txt exists on main; an untracked file of that name on
        // side must not be clobbered.
        std::fs::write(repo.workdir().unwrap().join("only_main.txt"), "mine\n").unwrap();
        assert!(checkout_branch(&repo, "main").is_err());
        assert_eq!(head_branch(&repo), "side");
        assert_eq!(read(&repo, "only_main.txt"), "mine\n");
        assert_eq!(read(&repo, "a.txt"), "side\n");
    }
}
