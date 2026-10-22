//! Repository discovery: one `git worktree list` call gives us everything
//! the commands need (main worktree, linked worktrees, and the managed layout).

use std::path::Path;

use crate::error::{Error, Result};
use crate::git::Git;
use crate::layout::Layout;
use crate::worktree::{Worktree, parse_porcelain};

#[derive(Debug)]
pub struct Repo {
    /// All worktrees; index 0 is always the main worktree.
    pub worktrees: Vec<Worktree>,
    pub layout: Layout,
    git: Git,
}

impl Repo {
    /// Find the repository containing `cwd` (works from the main worktree or
    /// from any linked worktree).
    pub fn discover(cwd: &Path) -> Result<Self> {
        let probe = Git::new(cwd).output(["rev-parse", "--git-dir"])?;
        if !probe.success() {
            return Err(if probe.stderr.contains("not a git repository") {
                Error::NotARepository
            } else {
                Error::GitFailed {
                    command: "rev-parse --git-dir".into(),
                    status: format!("exit code {}", probe.code.unwrap_or(-1)),
                    stderr: probe.stderr.trim().into(),
                }
            });
        }

        let listing = Git::new(cwd).run(["worktree", "list", "--porcelain"])?;
        let worktrees = parse_porcelain(&listing)?;
        // Git documents that the main worktree is listed first.
        let main = worktrees
            .first()
            .ok_or_else(|| Error::Parse("`git worktree list` returned no worktrees".into()))?;
        if main.bare {
            return Err(Error::BareRepository);
        }
        let layout = Layout::for_main_worktree(&main.path)?;
        let git = Git::new(&main.path);
        Ok(Self {
            worktrees,
            layout,
            git,
        })
    }

    pub fn main(&self) -> &Worktree {
        &self.worktrees[0]
    }

    /// A [`Git`] anchored at the main worktree, for repo-wide commands.
    pub fn git(&self) -> &Git {
        &self.git
    }

    pub fn name(&self) -> String {
        self.main()
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn find_by_branch(&self, branch: &str) -> Option<&Worktree> {
        self.worktrees
            .iter()
            .find(|w| w.branch_name() == Some(branch))
    }

    pub fn is_main(&self, wt: &Worktree) -> bool {
        std::ptr::eq(wt, self.main())
    }
}
