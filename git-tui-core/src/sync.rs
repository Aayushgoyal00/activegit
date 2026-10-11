//! Remote sync: upstream tracking, push/pull/publish.
//!
//! Reads (remotes, upstream, ahead/behind) use libgit2 directly and never
//! touch the network. Push/pull/publish shell out to the `git` CLI —
//! exactly like lazygit does — so authentication (ssh keys and agent,
//! credential helpers, `gh auth`) works the way the user's shell works.

use crate::error::GitError;
use crate::repo::{repo_state, RepoState};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Upstream tracking state for the current branch. Pure local reads, no
/// network: `ahead`/`behind` compare HEAD against the last-fetched
/// upstream commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStatus {
    /// Upstream shorthand, e.g. `origin/main`. `None` when the branch has
    /// no upstream (or there are no commits yet).
    pub upstream: Option<String>,
    /// Commits on HEAD not on the upstream.
    pub ahead: usize,
    /// Upstream commits not on HEAD (as of the last fetch/pull).
    pub behind: usize,
    /// Configured remote names, sorted.
    pub remotes: Vec<String>,
}

/// Local tracking state for the current branch. Pure libgit2 reads, no
/// network: `ahead`/`behind` compare HEAD against the last-fetched
/// upstream commit, so they only move on fetch/pull/push.
pub fn sync_status(repo: &git2::Repository) -> Result<SyncStatus, GitError> {
    let remotes = list_remotes(repo)?;
    let none = || SyncStatus {
        upstream: None,
        ahead: 0,
        behind: 0,
        remotes: remotes.clone(),
    };
    let Some(name) = current_local_branch(repo) else {
        return Ok(none());
    };
    let branch = match repo.find_branch(&name, git2::BranchType::Local) {
        Ok(b) => b,
        Err(_) => return Ok(none()),
    };
    let upstream = match branch.upstream() {
        Ok(u) => u,
        Err(_) => return Ok(none()),
    };
    let upstream_name = upstream.get().shorthand().unwrap_or("upstream").to_string();
    let (ahead, behind) = match (
        branch.get().peel_to_commit(),
        upstream.get().peel_to_commit(),
    ) {
        (Ok(local), Ok(remote)) => repo
            .graph_ahead_behind(local.id(), remote.id())
            .unwrap_or((0, 0)),
        _ => (0, 0),
    };
    Ok(SyncStatus {
        upstream: Some(upstream_name),
        ahead,
        behind,
        remotes,
    })
}

/// Sorted remote names (`origin`, …).
pub fn list_remotes(repo: &git2::Repository) -> Result<Vec<String>, GitError> {
    let list = repo.remotes()?;
    let mut out: Vec<String> = list.iter().flatten().map(|s| s.to_string()).collect();
    out.sort();
    Ok(out)
}

/// Fetch URL of remote `name`, or `None` when it is not configured.
pub fn remote_url(repo: &git2::Repository, name: &str) -> Result<Option<String>, GitError> {
    match repo.find_remote(name) {
        Ok(r) => Ok(r.url().map(|s| s.to_string())),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(GitError::Git(e)),
    }
}

/// `git remote add <name> <url>`. Errors on empty input or an existing
/// remote of the same name.
pub fn add_remote(repo: &git2::Repository, name: &str, url: &str) -> Result<(), GitError> {
    if name.trim().is_empty() {
        return Err(GitError::Sync("remote name is empty".into()));
    }
    if url.trim().is_empty() {
        return Err(GitError::Sync("remote URL is empty".into()));
    }
    repo.remote(name, url)
        .map(|_| ())
        .map_err(|e| GitError::Sync(format!("cannot add remote {name}: {e}")))?;
    Ok(())
}

/// Push the branch, like lazygit's `P`: `git push [-u] <remote> <branch>`.
/// Shells out so ssh/credential-helper auth works exactly like the shell.
/// Returns the command output on success.
pub fn push(
    workdir: &Path,
    remote: &str,
    branch: &str,
    set_upstream: bool,
) -> Result<String, GitError> {
    if remote.trim().is_empty() || branch.trim().is_empty() {
        return Err(GitError::Sync("push needs a remote and a branch".into()));
    }
    let mut args = vec!["push"];
    if set_upstream {
        args.push("--set-upstream");
    }
    args.push(remote);
    args.push(branch);
    run_git(workdir, &args)
}

/// Pull the current branch, like lazygit's `p`: plain `git pull`, so the
/// user's `pull.rebase` / `pull.ff` config decides merge vs rebase.
pub fn pull(workdir: &Path) -> Result<String, GitError> {
    run_git(workdir, &["pull"])
}

/// Publish a repo that has no remote yet (the "not yet on GitHub" case):
/// point `remote` at `url` (refusing to clobber a different URL), then
/// push the branch with `-u` so later `p`/`P` just work.
pub fn publish(
    repo: &git2::Repository,
    remote: &str,
    url: &str,
    branch: &str,
) -> Result<String, GitError> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| GitError::Sync("bare repositories cannot be published".into()))?
        .to_path_buf();
    match remote_url(repo, remote)? {
        Some(existing) if existing == url => {}
        Some(existing) => {
            return Err(GitError::Sync(format!(
                "remote {remote} already points at {existing}"
            )))
        }
        None => add_remote(repo, remote, url)?,
    }
    push(&workdir, remote, branch, true)
}

/// `git fetch --prune`: update the remote-tracking refs (and drop ones
/// whose branch is gone) without touching the working tree, so
/// [`sync_status`] can report what there is to pull. Returns the command
/// output on success.
pub fn fetch(repo: &git2::Repository) -> Result<String, GitError> {
    let workdir = workdir_for(repo, "fetch")?;
    if list_remotes(repo)?.is_empty() {
        return Err(GitError::Sync("no remote to fetch from".into()));
    }
    run_git(&workdir, &["fetch", "--prune"])
}

/// `git push --force-with-lease <remote> <branch>`: overwrite the remote
/// branch after a local history rewrite (amend, rebase), but only if it
/// still is where the last fetch saw it, so nobody else's push is lost.
pub fn push_force_with_lease(
    repo: &git2::Repository,
    remote: &str,
    branch: &str,
) -> Result<String, GitError> {
    if remote.trim().is_empty() || branch.trim().is_empty() {
        return Err(GitError::Sync("push needs a remote and a branch".into()));
    }
    let workdir = workdir_for(repo, "push")?;
    run_git(&workdir, &["push", "--force-with-lease", remote, branch])
}

/// Push the current branch to the branch it tracks, whatever that is
/// called: local `feat` tracking `origin/feature-x` goes to `feature-x`
/// (a plain `git push origin feat` would create `origin/feat` instead).
/// Errors on a detached or unborn HEAD and on a branch with no upstream.
pub fn push_to_upstream(repo: &git2::Repository) -> Result<String, GitError> {
    let workdir = workdir_for(repo, "push")?;
    let local = current_local_branch(repo).ok_or_else(|| {
        GitError::Sync("cannot push: HEAD is detached or has no commits yet".into())
    })?;
    // `branch.<name>.remote` / `.merge` are what `git push -u` records:
    // the remote's name and the full ref on it (`refs/heads/feature-x`).
    let config = repo.config()?;
    let setting = |key: &str| {
        config
            .get_string(&format!("branch.{local}.{key}"))
            .ok()
            .filter(|v| !v.trim().is_empty())
    };
    let (Some(remote), Some(upstream_ref)) = (setting("remote"), setting("merge")) else {
        return Err(GitError::Sync(format!(
            "cannot push: branch {local} has no upstream yet"
        )));
    };
    let refspec = format!("refs/heads/{local}:{upstream_ref}");
    run_git(&workdir, &["push", &remote, &refspec])
}

/// Abort the merge / rebase / cherry-pick / revert in progress
/// (`git <operation> --abort`), putting the repository back where it was
/// before the operation started. Errors when nothing is in progress.
pub fn abort_operation(repo: &git2::Repository) -> Result<String, GitError> {
    let workdir = workdir_for(repo, "abort an operation")?;
    let operation = operation_in_progress(repo, "abort")?;
    run_git(&workdir, &[operation, "--abort"])
}

/// Continue the merge / rebase / cherry-pick / revert in progress once
/// its conflicts are resolved and staged (`git <operation> --continue`).
/// The prepared commit message is taken as is: no editor is opened.
/// Errors when nothing is in progress.
pub fn continue_operation(repo: &git2::Repository) -> Result<String, GitError> {
    let workdir = workdir_for(repo, "continue an operation")?;
    let operation = operation_in_progress(repo, "continue")?;
    // `GIT_EDITOR` (set by `no_editor`) outranks `core.editor`; both are
    // pinned so neither the environment nor the config can open one.
    run_git_raw(
        &workdir,
        &["-c", "core.editor=true", operation, "--continue"],
        None,
        true,
    )
    .map_err(GitError::Sync)
}

/// The `git` subcommand that owns the operation in progress, for
/// `--abort` / `--continue`. `verb` only words the error.
fn operation_in_progress(repo: &git2::Repository, verb: &str) -> Result<&'static str, GitError> {
    match repo_state(repo) {
        RepoState::Merge => Ok("merge"),
        RepoState::Rebase => Ok("rebase"),
        RepoState::CherryPick => Ok("cherry-pick"),
        RepoState::Revert => Ok("revert"),
        RepoState::Clean => Err(GitError::Sync(format!(
            "nothing to {verb}: no merge, rebase, cherry-pick or revert in progress"
        ))),
        RepoState::Bisect => Err(GitError::Sync(format!(
            "cannot {verb} a bisect from here: use git bisect in a terminal"
        ))),
        RepoState::Other => Err(GitError::Sync(format!(
            "cannot {verb} this operation from here: finish it in a terminal"
        ))),
    }
}

/// Workdir root for a command that shells out; bare repos are refused.
/// `action` only words the error.
fn workdir_for(repo: &git2::Repository, action: &str) -> Result<PathBuf, GitError> {
    repo.workdir()
        .map(Path::to_path_buf)
        .ok_or_else(|| GitError::Sync(format!("bare repositories cannot {action}")))
}

/// Short name of the checked-out local branch, or `None` on detached or
/// unborn HEAD.
fn current_local_branch(repo: &git2::Repository) -> Option<String> {
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    head.shorthand().map(|s| s.to_string())
}

/// Run the `git` CLI in `workdir` for a sync operation. See
/// [`run_git_raw`]; failures come back as [`GitError::Sync`].
fn run_git(workdir: &Path, args: &[&str]) -> Result<String, GitError> {
    run_git_raw(workdir, args, None, false).map_err(GitError::Sync)
}

/// Run the `git` CLI in `workdir`. Prompts are disabled
/// (`GIT_TERMINAL_PROMPT=0`, ssh batch mode unless the user configured
/// their own ssh command) so a missing credential fails fast with the
/// remote's message instead of hanging the worker thread forever.
///
/// `stdin` is fed to the child (e.g. a commit message for `--file=-`);
/// without it the child reads from the null device. `no_editor` sets
/// `GIT_EDITOR=true` so nothing can open an editor behind the TUI.
///
/// Returns the command output on success and a one-line, capped message
/// built from git's output on failure; callers wrap it in their own
/// [`GitError`] variant.
pub(crate) fn run_git_raw(
    workdir: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    no_editor: bool,
) -> Result<String, String> {
    use std::process::Stdio;
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .current_dir(workdir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_editor {
        cmd.env("GIT_EDITOR", "true");
    }
    if force_ssh_batch_mode(
        std::env::var_os("GIT_SSH_COMMAND").as_deref(),
        std::env::var_os("GIT_SSH").as_deref(),
        configured_ssh_command(workdir).as_deref(),
    ) {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run git: {e}"))?;
    // Feed stdin from its own thread: git may run a hook (and fill the
    // output pipes) before it gets round to reading the message.
    let feeder = child.stdin.take().zip(stdin).map(|(mut pipe, data)| {
        let data = data.to_vec();
        std::thread::spawn(move || {
            use std::io::Write;
            // A child that exits without reading closes the pipe: fine.
            let _ = pipe.write_all(&data);
        })
    });
    let out = child
        .wait_with_output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    if !out.status.success() {
        return Err(failure_message(args, &out.stdout, &out.stderr));
    }
    let mut combined = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if combined.is_empty() {
        combined = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    Ok(combined)
}

/// Longest failure message kept from git's output: room for a list of
/// conflicted files, still short enough for the status line.
const MAX_FAILURE_LEN: usize = 600;

/// One-line failure text from a failed git command: stdout first (that
/// is where `git pull` / `git merge` print `CONFLICT ...` and `Automatic
/// merge failed`), then stderr, blank lines dropped, capped.
fn failure_message(args: &[&str], stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut msg: String = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if msg.is_empty() {
        msg = format!("git {} failed with no output", args.join(" "));
    }
    if msg.len() > MAX_FAILURE_LEN {
        // Back off to a char boundary without `floor_char_boundary`
        // (stable since 1.91; MSRV is 1.88).
        let mut i = MAX_FAILURE_LEN;
        while !msg.is_char_boundary(i) {
            i -= 1;
        }
        msg.truncate(i);
    }
    msg
}

/// Whether to put ssh in batch mode by exporting `GIT_SSH_COMMAND`. That
/// variable overrides both `GIT_SSH` and `core.sshCommand`, so exporting
/// it is only safe when the user set none of the three: otherwise their
/// ssh setup (Windows OpenSSH agent, per-repo identity, plink, …) would
/// be silently replaced.
fn force_ssh_batch_mode(
    env_git_ssh_command: Option<&OsStr>,
    env_git_ssh: Option<&OsStr>,
    core_ssh_command: Option<&str>,
) -> bool {
    env_git_ssh_command.is_none() && env_git_ssh.is_none() && core_ssh_command.is_none()
}

/// `core.sshCommand` as configured for the repo at `workdir` (any level:
/// system, global, local). Falls back to the user's global config when
/// `workdir` is not a repository. `None` when unset or blank.
fn configured_ssh_command(workdir: &Path) -> Option<String> {
    let config = git2::Repository::open(workdir)
        .and_then(|repo| repo.config())
        .or_else(|_| git2::Config::open_default())
        .ok()?;
    config
        .get_string("core.sshCommand")
        .ok()
        .filter(|cmd| !cmd.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    /// Local repo pushing to / pulling from a bare repo on disk. File
    /// remotes need no network, so push/pull tests run fully offline.
    fn repo_with_bare_origin() -> (tempfile::TempDir, git2::Repository, tempfile::TempDir) {
        let (dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let origin_path = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        add_remote(&repo, "origin", origin_path.to_str().unwrap()).unwrap();
        (dir, repo, origin_dir)
    }

    #[test]
    fn reports_no_upstream_without_remote() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let st = sync_status(&repo).unwrap();
        assert_eq!(st.upstream, None);
        assert_eq!((st.ahead, st.behind), (0, 0));
        assert!(st.remotes.is_empty());
    }

    #[test]
    fn push_records_upstream_and_clears_ahead() {
        let (_dir, repo, _origin) = repo_with_bare_origin();
        // One commit not yet pushed.
        testutil::commit_file(&repo, "a.txt", "a\nmore\n", "second");
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        let st = sync_status(&repo).unwrap();
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (0, 0));
    }

    #[test]
    fn pull_fast_forwards_to_remote_changes() {
        let (_dir, repo, origin) = repo_with_bare_origin();
        let bare = origin.path().join("origin.git");
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        // A second clone advances the remote (re-pointed at the bare repo:
        // cloning the workdir would leave `origin` on a non-bare repo).
        let (dir2, other) = testutil::clone_local(workdir.as_path());
        other
            .remote_set_url("origin", bare.to_str().unwrap())
            .unwrap();
        testutil::commit_file(&other, "b.txt", "b\n", "from other");
        let workdir2 = other.workdir().unwrap().to_path_buf();
        push(&workdir2, "origin", "main", false).unwrap();
        drop(dir2);
        // Pull brings the new commit in.
        pull(&workdir).unwrap();
        assert!(workdir.join("b.txt").exists());
        let st = sync_status(&repo).unwrap();
        assert_eq!((st.ahead, st.behind), (0, 0));
    }

    #[test]
    fn fetch_reveals_behind_count() {
        let (_dir, repo, origin) = repo_with_bare_origin();
        let bare = origin.path().join("origin.git");
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        let (dir2, other) = testutil::clone_local(workdir.as_path());
        other
            .remote_set_url("origin", bare.to_str().unwrap())
            .unwrap();
        testutil::commit_file(&other, "b.txt", "b\n", "from other");
        let workdir2 = other.workdir().unwrap().to_path_buf();
        push(&workdir2, "origin", "main", false).unwrap();
        drop(dir2);
        run_git(&workdir, &["fetch", "origin"]).unwrap();
        let st = sync_status(&repo).unwrap();
        assert_eq!((st.ahead, st.behind), (0, 1));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
    }

    #[test]
    fn publish_adds_remote_and_pushes_new_repo() {
        // Fresh `git init` with no remotes at all — the "not yet on
        // GitHub" case: point it at an (empty) remote and push.
        let (dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let origin_path = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        publish(&repo, "origin", origin_path.to_str().unwrap(), "main").unwrap();
        assert_eq!(
            remote_url(&repo, "origin").unwrap().as_deref(),
            Some(origin_path.to_str().unwrap())
        );
        // The remote actually received the branch.
        let remote = git2::Repository::open_bare(&origin_path).unwrap();
        assert!(remote.find_reference("refs/heads/main").is_ok());
        drop(dir);
    }

    #[test]
    fn local_commit_after_push_shows_ahead() {
        let (_dir, repo, _origin) = repo_with_bare_origin();
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        testutil::commit_file(&repo, "a.txt", "a\nlocal\n", "local work");
        let st = sync_status(&repo).unwrap();
        assert_eq!((st.ahead, st.behind), (1, 0));
    }

    #[test]
    fn pull_without_remote_is_an_error() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let workdir = repo.workdir().unwrap().to_path_buf();
        assert!(pull(&workdir).is_err());
    }

    #[test]
    fn push_with_no_commits_is_an_error() {
        let (dir, repo) = testutil::init_repo();
        let origin_path = dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        add_remote(&repo, "origin", origin_path.to_str().unwrap()).unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();
        assert!(push(&workdir, "origin", "main", true).is_err());
    }

    #[test]
    fn ssh_batch_mode_is_forced_only_when_nothing_is_configured() {
        let set = Some(OsStr::new("ssh -i ~/.ssh/work"));
        // Nothing configured anywhere: safe to export our own command.
        assert!(force_ssh_batch_mode(None, None, None));
        // Any user choice wins: the export would override all three.
        assert!(!force_ssh_batch_mode(set, None, None));
        assert!(!force_ssh_batch_mode(
            None,
            Some(OsStr::new("plink.exe")),
            None
        ));
        assert!(!force_ssh_batch_mode(
            None,
            None,
            Some("C:/Windows/System32/OpenSSH/ssh.exe")
        ));
        assert!(!force_ssh_batch_mode(set, set, Some("ssh")));
    }

    #[test]
    fn core_ssh_command_is_read_from_repo_config() {
        let (_dir, repo) = testutil::init_repo();
        let workdir = repo.workdir().unwrap().to_path_buf();
        repo.config()
            .unwrap()
            .set_str("core.sshCommand", "ssh -i ~/.ssh/work")
            .unwrap();
        assert_eq!(
            configured_ssh_command(&workdir).as_deref(),
            Some("ssh -i ~/.ssh/work")
        );
        // Blank counts as unset.
        repo.config()
            .unwrap()
            .set_str("core.sshCommand", "  ")
            .unwrap();
        assert_eq!(configured_ssh_command(&workdir), None);
    }

    #[test]
    fn failure_message_puts_stdout_first_and_caps_length() {
        let msg = failure_message(
            &["pull"],
            b"Auto-merging a.txt\nCONFLICT (content): Merge conflict in a.txt\n\n",
            b"From /tmp/origin\n   abc..def  main -> origin/main\n",
        );
        assert_eq!(
            msg,
            "Auto-merging a.txt CONFLICT (content): Merge conflict in a.txt \
             From /tmp/origin abc..def  main -> origin/main"
        );
        // Stderr alone still works, and silence names the command.
        assert_eq!(
            failure_message(&["push"], b"", b"fatal: nope\n"),
            "fatal: nope"
        );
        assert_eq!(
            failure_message(&["push", "origin"], b"", b"\n"),
            "git push origin failed with no output"
        );
        // Capped on a char boundary.
        let long = "é".repeat(MAX_FAILURE_LEN);
        let capped = failure_message(&["x"], long.as_bytes(), b"");
        assert!(capped.len() <= MAX_FAILURE_LEN);
        assert!(capped.chars().all(|c| c == 'é'));
    }

    /// `repo_with_bare_origin`, pushed, plus a second clone wired to the
    /// same bare remote. Returns `(dir, repo, origin, other_dir, other)`.
    #[allow(clippy::type_complexity)]
    fn two_clones() -> (
        tempfile::TempDir,
        git2::Repository,
        tempfile::TempDir,
        tempfile::TempDir,
        git2::Repository,
    ) {
        let (dir, repo, origin) = repo_with_bare_origin();
        let bare = origin.path().join("origin.git");
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        let (dir2, other) = testutil::clone_local(workdir.as_path());
        other
            .remote_set_url("origin", bare.to_str().unwrap())
            .unwrap();
        (dir, repo, origin, dir2, other)
    }

    fn push_from(repo: &git2::Repository) {
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", false).unwrap();
    }

    fn remote_tip(origin: &tempfile::TempDir, branch: &str) -> Option<git2::Oid> {
        let remote = git2::Repository::open_bare(origin.path().join("origin.git")).unwrap();
        remote.refname_to_id(&format!("refs/heads/{branch}")).ok()
    }

    #[test]
    fn conflicting_pull_reports_the_conflict_and_can_be_aborted() {
        let (_dir, repo, _origin, _dir2, other) = two_clones();
        testutil::commit_file(&other, "a.txt", "a\nfrom other\n", "other edit");
        push_from(&other);
        testutil::commit_file(&repo, "a.txt", "a\nlocal\n", "local edit");
        // Merge, whatever the machine's global pull settings say.
        repo.config()
            .unwrap()
            .set_str("pull.rebase", "false")
            .unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();

        let err = pull(&workdir).unwrap_err();
        match err {
            GitError::Sync(msg) => {
                assert!(msg.contains("CONFLICT"), "got: {msg}");
                assert!(msg.contains("a.txt"), "got: {msg}");
            }
            other => panic!("wrong error: {other:?}"),
        }
        assert_eq!(repo_state(&repo), RepoState::Merge);

        abort_operation(&repo).unwrap();
        assert_eq!(repo_state(&repo), RepoState::Clean);
        assert_eq!(
            std::fs::read_to_string(workdir.join("a.txt")).unwrap(),
            "a\nlocal\n"
        );
    }

    #[test]
    fn fetch_updates_tracking_refs_without_touching_the_workdir() {
        let (_dir, repo, _origin, _dir2, other) = two_clones();
        testutil::commit_file(&other, "b.txt", "b\n", "from other");
        push_from(&other);
        assert_eq!(sync_status(&repo).unwrap().behind, 0);

        fetch(&repo).unwrap();
        let st = sync_status(&repo).unwrap();
        assert_eq!((st.ahead, st.behind), (0, 1));
        assert!(!repo.workdir().unwrap().join("b.txt").exists());
    }

    #[test]
    fn fetch_without_remote_is_an_error() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        assert!(matches!(fetch(&repo), Err(GitError::Sync(_))));
    }

    /// Drop the HEAD commit and commit something else in its place.
    fn rewrite_tip(repo: &git2::Repository, contents: &str) -> git2::Oid {
        let parent = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent(0)
            .unwrap()
            .into_object();
        repo.reset(&parent, git2::ResetType::Hard, None).unwrap();
        testutil::commit_file(repo, "a.txt", contents, "rewritten")
    }

    #[test]
    fn force_with_lease_pushes_rewritten_history() {
        let (_dir, repo, origin) = repo_with_bare_origin();
        let workdir = repo.workdir().unwrap().to_path_buf();
        let pushed = testutil::commit_file(&repo, "a.txt", "a\nsecond\n", "second");
        push(&workdir, "origin", "main", true).unwrap();
        assert_eq!(remote_tip(&origin, "main"), Some(pushed));

        let rewritten = rewrite_tip(&repo, "a\nrewritten\n");
        // A plain push is rejected (not a fast-forward)…
        assert!(push(&workdir, "origin", "main", false).is_err());
        assert_eq!(remote_tip(&origin, "main"), Some(pushed));
        // …the leased force goes through.
        push_force_with_lease(&repo, "origin", "main").unwrap();
        assert_eq!(remote_tip(&origin, "main"), Some(rewritten));
    }

    #[test]
    fn force_with_lease_refuses_to_overwrite_unseen_remote_commits() {
        let (_dir, repo, origin, _dir2, other) = two_clones();
        let workdir = repo.workdir().unwrap().to_path_buf();
        testutil::commit_file(&repo, "a.txt", "a\nsecond\n", "second");
        push(&workdir, "origin", "main", false).unwrap();
        // Someone else pushes on top; we never fetch it.
        pull(other.workdir().unwrap()).unwrap();
        let theirs = testutil::commit_file(&other, "b.txt", "b\n", "from other");
        push_from(&other);

        rewrite_tip(&repo, "a\nrewritten\n");
        assert!(push_force_with_lease(&repo, "origin", "main").is_err());
        assert_eq!(remote_tip(&origin, "main"), Some(theirs));
    }

    #[test]
    fn push_to_upstream_follows_a_differently_named_upstream() {
        let (_dir, repo, origin) = repo_with_bare_origin();
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        // Local `feat` tracks `origin/feature-x`.
        testutil::new_branch(&repo, "feat");
        testutil::commit_file(&repo, "f.txt", "one\n", "feat one");
        run_git(
            &workdir,
            &["push", "--set-upstream", "origin", "feat:feature-x"],
        )
        .unwrap();
        assert_eq!(
            sync_status(&repo).unwrap().upstream.as_deref(),
            Some("origin/feature-x")
        );

        let tip = testutil::commit_file(&repo, "f.txt", "one\ntwo\n", "feat two");
        assert_eq!(sync_status(&repo).unwrap().ahead, 1);
        push_to_upstream(&repo).unwrap();
        assert_eq!(remote_tip(&origin, "feature-x"), Some(tip));
        assert_eq!(
            remote_tip(&origin, "feat"),
            None,
            "must not create a branch named after the local one"
        );
        let st = sync_status(&repo).unwrap();
        assert_eq!((st.ahead, st.behind), (0, 0));
    }

    #[test]
    fn push_to_upstream_needs_a_branch_with_an_upstream() {
        let (_dir, repo, _origin) = repo_with_bare_origin();
        // On a branch, but nothing tracked yet.
        match push_to_upstream(&repo).unwrap_err() {
            GitError::Sync(msg) => assert!(msg.contains("no upstream"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
        // Detached HEAD.
        let head = repo.head().unwrap().peel_to_commit().unwrap().id();
        repo.set_head_detached(head).unwrap();
        match push_to_upstream(&repo).unwrap_err() {
            GitError::Sync(msg) => assert!(msg.contains("detached"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
        // Unborn HEAD.
        let (_dir, fresh) = testutil::init_repo();
        assert!(push_to_upstream(&fresh).is_err());
    }

    #[test]
    fn abort_operation_backs_out_of_a_conflicted_merge() {
        let (_dir, repo) = testutil::conflicted_merge();
        let workdir = repo.workdir().unwrap().to_path_buf();
        assert_eq!(repo_state(&repo), RepoState::Merge);

        abort_operation(&repo).unwrap();
        assert_eq!(repo_state(&repo), RepoState::Clean);
        assert_eq!(
            std::fs::read_to_string(workdir.join("a.txt")).unwrap(),
            "main change\n"
        );
        assert!(!workdir.join("side.txt").exists());
        let st = crate::status::repo_status(&repo).unwrap();
        assert!(st.files.is_empty(), "got {:?}", st.files);

        // Nothing in progress any more.
        match abort_operation(&repo).unwrap_err() {
            GitError::Sync(msg) => assert!(msg.contains("nothing to abort"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
        assert!(continue_operation(&repo).is_err());
    }

    #[test]
    fn continue_operation_concludes_a_resolved_merge() {
        let (_dir, repo) = testutil::conflicted_merge();
        let workdir = repo.workdir().unwrap().to_path_buf();
        // Still conflicted: git refuses and the merge stays open.
        assert!(continue_operation(&repo).is_err());
        assert_eq!(repo_state(&repo), RepoState::Merge);

        std::fs::write(workdir.join("a.txt"), "resolved\n").unwrap();
        crate::stage::stage_file(&repo, "a.txt").unwrap();
        continue_operation(&repo).unwrap();

        assert_eq!(repo_state(&repo), RepoState::Clean);
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.parent_count(), 2);
        assert!(
            head.message().unwrap().starts_with("Merge"),
            "got: {:?}",
            head.message()
        );
        let blob = head
            .tree()
            .unwrap()
            .get_name("a.txt")
            .unwrap()
            .to_object(&repo)
            .unwrap();
        assert_eq!(blob.as_blob().unwrap().content(), b"resolved\n");
        assert!(head.tree().unwrap().get_name("side.txt").is_some());
        let st = crate::status::repo_status(&repo).unwrap();
        assert!(st.files.is_empty(), "got {:?}", st.files);
    }
}
