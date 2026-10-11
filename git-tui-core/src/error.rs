use thiserror::Error;

/// All errors from git-tui-core. Typed here; git-tui wraps these in `anyhow`.
#[derive(Debug, Error)]
pub enum GitError {
    #[error("not inside a git repository (searched upward from {0})")]
    NotARepo(String),

    #[error("path is empty: type a directory or repo path")]
    EmptyPath,

    #[error("no such path: {0}")]
    NotFound(String),

    #[error("bare repositories are not supported: {0}")]
    BareRepo(String),

    #[error(transparent)]
    Git(#[from] git2::Error),

    #[error("job channel disconnected")]
    ChannelDisconnected,

    #[error("nothing to commit: index is empty")]
    EmptyCommit,

    /// `git commit` itself failed; carries git's output (a rejecting
    /// hook's message, a signing error, …).
    #[error("commit failed: {0}")]
    Commit(String),

    #[error("hunk staging failed: {0}")]
    HunkStaging(String),

    #[error("discard failed: {0}")]
    Discard(String),

    #[error("branch operation failed: {0}")]
    Branch(String),

    #[error("log failed: {0}")]
    Log(String),

    #[error("stash failed: {0}")]
    Stash(String),

    #[error("sync failed: {0}")]
    Sync(String),

    #[error("commit message generation failed: {0}")]
    Llm(String),

    #[error("could not list provider models: {0}")]
    LlmModels(String),
}
