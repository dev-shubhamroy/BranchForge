//! Typed errors for BranchForge.
//!
//! Every failure the tool can report is a variant of [`Error`]. `thiserror`
//! generates the `Display` and `std::error::Error` impls from the
//! `#[error("...")]` attributes, so messages live next to the variants.

use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // ---- Git subprocess ----
    #[error("could not run `git` (is Git installed and on your PATH?)")]
    GitSpawn(#[source] std::io::Error),

    #[error("`git {command}` failed ({status}): {stderr}")]
    GitFailed {
        command: String,
        status: String,
        stderr: String,
    },

    #[error("`git {command}` produced output that is not valid UTF-8")]
    NonUtf8Output { command: String },

    #[error("unexpected output from git: {0}")]
    Parse(String),

    // ---- Repository ----
    #[error("not inside a Git repository")]
    NotARepository,

    #[error("bare repositories are not supported")]
    BareRepository,

    #[error("cannot derive a managed directory from repository path {0}")]
    UnsupportedRepoPath(PathBuf),

    #[error(
        "managed directory {0} exists but is not a plain directory (symlinks and files are refused)"
    )]
    BadManagedRoot(PathBuf),

    // ---- `new` ----
    #[error("invalid branch name `{name}`: {reason}")]
    InvalidBranchName { name: String, reason: String },

    #[error("base `{0}` is not a valid commit-ish in this repository")]
    InvalidBase(String),

    #[error(
        "branch `{0}` already exists, so --base would be ignored; drop --base or pick a new branch name"
    )]
    BaseWithExistingBranch(String),

    #[error(
        "the repository has no commits yet, so there is nothing to branch from; create an initial commit first (for example `git commit --allow-empty -m \"Initial commit\"`) and try again"
    )]
    NoCommits,

    #[error("branch `{branch}` is already checked out at {path}")]
    BranchCheckedOut { branch: String, path: PathBuf },

    #[error("{0} already exists; refusing to overwrite it")]
    PathExists(PathBuf),

    // ---- `remove` ----
    #[error("no worktree has branch `{0}` checked out")]
    WorktreeNotFound(String),

    #[error("`{0}` is checked out in the main worktree, which BranchForge never removes")]
    MainWorktree(String),

    #[error(
        "worktree for `{branch}` at {path} is not managed by BranchForge (expected it at {expected}); remove it with `git worktree remove` yourself"
    )]
    NotManaged {
        branch: String,
        path: PathBuf,
        expected: PathBuf,
    },

    #[error("worktree at {path} is locked{reason}; unlock it with `git worktree unlock` first")]
    Locked { path: PathBuf, reason: String },

    #[error(
        "the current directory is inside the worktree being removed ({0}); `cd` somewhere else first"
    )]
    InsideWorktree(PathBuf),

    #[error(
        "worktree directory {0} is missing on disk; run `git worktree prune` if it is really gone"
    )]
    WorktreeMissing(PathBuf),

    #[error(
        "worktree at {path} has uncommitted changes ({summary}); commit or stash them, or re-run with --force to discard them"
    )]
    DirtyWorktree { path: PathBuf, summary: String },

    #[error(
        "worktree at {path} contains {count} git-ignored item(s) that would be permanently deleted (e.g. {examples}); ignored files such as .env or node_modules are not stored in Git, so back them up or re-run with --force to delete them"
    )]
    IgnoredContent {
        path: PathBuf,
        count: u32,
        examples: String,
    },

    // ---- Filesystem ----
    #[error("I/O error on {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
