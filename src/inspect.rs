//! Combines the worktree list with per-worktree `git status` into [`Entry`]s
//! that `list` and `status` can render.

use std::path::Path;

use crate::error::Result;
use crate::git::Git;
use crate::repo::Repo;
use crate::state::{WorktreeState, parse_status_v2};
use crate::worktree::Worktree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Main,
    Managed,
    External,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Main => "main",
            Kind::Managed => "managed",
            Kind::External => "external",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Ready(WorktreeState),
    /// Directory is gone (git calls such entries "prunable").
    Missing,
    /// `git status` failed for this worktree; one broken worktree must not
    /// break the whole listing.
    Unreadable(String),
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub worktree: Worktree,
    pub kind: Kind,
    pub health: Health,
}

/// Normal state (used by `list`/`status`): ignored files are not requested.
pub fn read_state(path: &Path) -> Result<WorktreeState> {
    read_status(path, false)
}

/// Like [`read_state`] but also reports ignored files/directories. Used by
/// `remove`, where ignored content (`.env`, `node_modules/`) would be lost.
pub fn read_state_with_ignored(path: &Path) -> Result<WorktreeState> {
    read_status(path, true)
}

fn read_status(path: &Path, include_ignored: bool) -> Result<WorktreeState> {
    // `--untracked-files=normal` is pinned so a user's
    // `status.showUntrackedFiles=no` can't hide untracked files from us.
    let mut args = vec![
        "status",
        "--porcelain=v2",
        "--branch",
        "--untracked-files=normal",
    ];
    if include_ignored {
        args.push("--ignored=traditional");
    }
    let out = Git::new(path).run(args)?;
    parse_status_v2(&out)
}

fn health_of(wt: &Worktree) -> Health {
    if wt.prunable.is_some() || !wt.path.is_dir() {
        return Health::Missing;
    }
    match read_state(&wt.path) {
        Ok(state) => Health::Ready(state),
        Err(e) => Health::Unreadable(e.to_string()),
    }
}

pub fn inspect(repo: &Repo) -> Vec<Entry> {
    repo.worktrees
        .iter()
        .map(|wt| {
            let kind = if repo.is_main(wt) {
                Kind::Main
            } else if repo.layout.is_managed(wt) {
                Kind::Managed
            } else {
                Kind::External
            };
            Entry {
                worktree: wt.clone(),
                kind,
                health: health_of(wt),
            }
        })
        .collect()
}
