//! Commit creation from the index.

use crate::error::GitError;

/// Create a commit from the current index. Returns the new commit id.
///
/// The commit itself is made by the `git` CLI (`git commit --file=-`),
/// not libgit2, so it behaves like a commit from the shell: pre-commit /
/// prepare-commit-msg / commit-msg / post-commit hooks run,
/// `commit.gpgsign` is honored, and a merge in progress is concluded
/// with both parents.
///
/// Errors with [`GitError::EmptyCommit`] when there is nothing staged,
/// with [`GitError::HunkStaging`] when the message is empty, and with
/// [`GitError::Commit`] carrying git's output when git refuses (a hook
/// said no, signing failed, no identity configured, …).
pub fn commit(repo: &git2::Repository, message: &str) -> Result<git2::Oid, GitError> {
    if message.trim().is_empty() {
        return Err(GitError::HunkStaging("commit message is empty".into()));
    }
    // The CLI wrote the index on the last commit / pull: pick that up.
    let mut index = repo.index()?;
    index.read(false)?;
    let tree_id = index.write_tree()?;

    // Nothing staged? Compare index tree against HEAD tree.
    match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
        Some(head_commit) => {
            // Concluding a merge is a real commit even when the merged
            // tree equals HEAD's (the other side changed nothing new).
            let merging = repo.path().join("MERGE_HEAD").exists();
            if !merging && head_commit.tree_id() == tree_id {
                return Err(GitError::EmptyCommit);
            }
        }
        // Unborn HEAD: the initial commit needs a non-empty index.
        None => {
            if index.is_empty() {
                return Err(GitError::EmptyCommit);
            }
        }
    }
    run_commit(repo, &["commit", "--file=-"], message)
}

/// Replace the HEAD commit with a new one made from the current index
/// and `message`, keeping HEAD's parents (`git commit --amend`). Hooks
/// and signing apply as in [`commit`]. Returns the new commit id.
///
/// Errors with [`GitError::Commit`] when there is no commit to amend or
/// git refuses, and with [`GitError::HunkStaging`] on an empty message.
pub fn amend(repo: &git2::Repository, message: &str) -> Result<git2::Oid, GitError> {
    if message.trim().is_empty() {
        return Err(GitError::HunkStaging("commit message is empty".into()));
    }
    if repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .is_none()
    {
        return Err(GitError::Commit(
            "nothing to amend: this repository has no commits yet".into(),
        ));
    }
    run_commit(repo, &["commit", "--amend", "--file=-"], message)
}

/// Full message of the HEAD commit (summary and body, trailing newlines
/// trimmed), for prefilling an amend box. `None` when there is no commit
/// yet. Bytes that are not UTF-8 are replaced.
pub fn last_commit_message(repo: &git2::Repository) -> Option<String> {
    let head = repo.head().ok()?.peel_to_commit().ok()?;
    let message = String::from_utf8_lossy(head.message_bytes());
    Some(message.trim_end_matches(['\n', '\r']).to_string())
}

/// Run a `git commit` variant with `message` on stdin and report the
/// resulting HEAD. No `--no-verify`: hooks are the point. An editor can
/// never open (the message comes from stdin and `GIT_EDITOR=true`).
fn run_commit(
    repo: &git2::Repository,
    args: &[&str],
    message: &str,
) -> Result<git2::Oid, GitError> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| GitError::Commit("bare repositories have no index to commit".into()))?;
    crate::sync::run_git_raw(workdir, args, Some(message.as_bytes()), true)
        .map_err(GitError::Commit)?;
    Ok(repo.head()?.peel_to_commit()?.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn commit_advances_head_and_empties_status() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        crate::stage::stage_file(&repo, "a.txt").unwrap();
        let oid = commit(&repo, "second").unwrap();
        assert!(!oid.is_zero());
        let st = crate::status::repo_status(&repo).unwrap();
        assert!(st.files.is_empty(), "status should be clean: {st:?}");
        assert_eq!(st.head_summary, "second");
    }

    #[test]
    fn commit_with_no_staged_changes_errors() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let err = commit(&repo, "nothing").unwrap_err();
        assert!(matches!(err, GitError::EmptyCommit), "got {err:?}");
        // Unstaged changes alone must also error.
        testutil::dirty_file(&repo, "a.txt", "unstaged\n");
        let err = commit(&repo, "nothing").unwrap_err();
        assert!(matches!(err, GitError::EmptyCommit), "got {err:?}");
    }

    /// Install `script` as hook `name`. `core.hooksPath` is pinned to the
    /// repo's own hooks dir so a global setting cannot redirect it.
    /// Git for Windows runs `#!/bin/sh` hooks through its bundled sh.
    fn install_hook(repo: &git2::Repository, name: &str, script: &str) {
        let hooks = repo.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let file = hooks.join(name);
        std::fs::write(&file, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        repo.config()
            .unwrap()
            .set_str(
                "core.hooksPath",
                &hooks.to_string_lossy().replace('\\', "/"),
            )
            .unwrap();
    }

    fn head(repo: &git2::Repository) -> git2::Commit<'_> {
        repo.head().unwrap().peel_to_commit().unwrap()
    }

    fn stage_change(repo: &git2::Repository, path: &str, append: &str) {
        testutil::dirty_file(repo, path, append);
        crate::stage::stage_file(repo, path).unwrap();
    }

    #[test]
    fn empty_index_on_unborn_head_is_an_empty_commit() {
        let (_dir, repo) = testutil::init_repo();
        let err = commit(&repo, "nothing").unwrap_err();
        assert!(matches!(err, GitError::EmptyCommit), "got {err:?}");
    }

    #[test]
    fn initial_commit_lands_on_the_unborn_branch() {
        let (_dir, repo) = testutil::init_repo();
        std::fs::write(repo.workdir().unwrap().join("a.txt"), "a\n").unwrap();
        crate::stage::stage_file(&repo, "a.txt").unwrap();
        let oid = commit(&repo, "first\n\nwith a body").unwrap();
        let head = head(&repo);
        assert_eq!(head.id(), oid);
        assert_eq!(head.parent_count(), 0);
        assert_eq!(head.message().unwrap(), "first\n\nwith a body\n");
        assert_eq!(repo.head().unwrap().shorthand(), Some("main"));
        assert_eq!(head.author().name(), Some("Test User"));
    }

    #[test]
    fn failing_pre_commit_hook_blocks_the_commit_and_is_reported() {
        let (_dir, repo) = testutil::init_repo();
        let before = testutil::commit_file(&repo, "a.txt", "a\n", "init");
        install_hook(
            &repo,
            "pre-commit",
            "#!/bin/sh\necho 'lint says no: fix a.txt' >&2\nexit 1\n",
        );
        stage_change(&repo, "a.txt", "more\n");
        let err = commit(&repo, "second").unwrap_err();
        match err {
            GitError::Commit(msg) => {
                assert!(msg.contains("lint says no: fix a.txt"), "got: {msg}")
            }
            other => panic!("wrong error: {other:?}"),
        }
        // Nothing was committed and the change is still staged.
        assert_eq!(head(&repo).id(), before);
        assert!(!crate::diff::staged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
    }

    #[test]
    fn commit_msg_hook_can_rewrite_the_message() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        install_hook(
            &repo,
            "commit-msg",
            "#!/bin/sh\nprintf 'rewritten by hook\\n' > \"$1\"\n",
        );
        stage_change(&repo, "a.txt", "more\n");
        let oid = commit(&repo, "typed by the user").unwrap();
        assert_eq!(head(&repo).id(), oid);
        assert_eq!(head(&repo).message().unwrap(), "rewritten by hook\n");
    }

    #[test]
    fn commit_during_merge_records_both_parents_and_clears_merge_state() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::new_branch(&repo, "side");
        let side = testutil::commit_file(&repo, "side.txt", "side\n", "side");
        testutil::switch_branch(&repo, "main");
        let main = testutil::commit_file(&repo, "main.txt", "main\n", "main");
        testutil::start_merge(&repo, "side");
        assert_eq!(repo.state(), git2::RepositoryState::Merge);

        let oid = commit(&repo, "merge side into main").unwrap();
        let merged = head(&repo);
        assert_eq!(merged.id(), oid);
        let parents: Vec<git2::Oid> = merged.parent_ids().collect();
        assert_eq!(parents, [main, side]);
        assert_eq!(repo.state(), git2::RepositoryState::Clean);
        assert!(merged.tree().unwrap().get_name("side.txt").is_some());
        assert!(merged.tree().unwrap().get_name("main.txt").is_some());
    }

    #[test]
    fn merge_with_unchanged_tree_is_not_an_empty_commit() {
        // Both branches made the same change in separate commits: the
        // merged tree equals HEAD's, yet the merge still needs its commit.
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::new_branch(&repo, "side");
        let side = testutil::commit_file(&repo, "b.txt", "same\n", "side adds b");
        testutil::switch_branch(&repo, "main");
        let main = testutil::commit_file(&repo, "b.txt", "same\n", "main adds b");
        assert_ne!(side, main);
        testutil::start_merge(&repo, "side");

        commit(&repo, "merge side").unwrap();
        let parents: Vec<git2::Oid> = head(&repo).parent_ids().collect();
        assert_eq!(parents, [main, side]);
        assert_eq!(repo.state(), git2::RepositoryState::Clean);
        // Back to normal: nothing staged is an empty commit again.
        let err = commit(&repo, "nothing").unwrap_err();
        assert!(matches!(err, GitError::EmptyCommit), "got {err:?}");
    }

    #[test]
    fn amend_replaces_message_and_keeps_parent() {
        let (_dir, repo) = testutil::init_repo();
        let first = testutil::commit_file(&repo, "a.txt", "a\n", "first");
        stage_change(&repo, "a.txt", "more\n");
        let second = commit(&repo, "secnod").unwrap();
        assert_eq!(last_commit_message(&repo).as_deref(), Some("secnod"));

        // Message-only amend.
        let amended = amend(&repo, "second\n\nnow with a body").unwrap();
        assert_ne!(amended, second);
        let head_commit = head(&repo);
        assert_eq!(head_commit.id(), amended);
        assert_eq!(head_commit.parent_ids().collect::<Vec<_>>(), [first]);
        assert_eq!(
            last_commit_message(&repo).as_deref(),
            Some("second\n\nnow with a body")
        );
        let tree_before = head_commit.tree_id();

        // Amend that also folds in a newly staged change.
        stage_change(&repo, "a.txt", "even more\n");
        let folded = amend(&repo, "second").unwrap();
        let head_commit = head(&repo);
        assert_eq!(head_commit.id(), folded);
        assert_eq!(head_commit.parent_ids().collect::<Vec<_>>(), [first]);
        assert_ne!(head_commit.tree_id(), tree_before);
        assert!(crate::status::repo_status(&repo).unwrap().files.is_empty());
    }

    #[test]
    fn amend_without_commits_errors_clearly() {
        let (_dir, repo) = testutil::init_repo();
        assert_eq!(last_commit_message(&repo), None);
        let err = amend(&repo, "nothing there").unwrap_err();
        match err {
            GitError::Commit(msg) => assert!(msg.contains("nothing to amend"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
    }
}
