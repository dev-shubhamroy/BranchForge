use std::fs;
use std::path::Path;

use crate::error::{Error, Result};
use crate::inspect::read_state_with_ignored;
use crate::repo::Repo;

/// Remove the managed worktree for `branch`.
///
/// Every check below must pass before Git is asked to delete anything, and the
/// deletion itself is done by `git worktree remove` (never by `fs::remove_*`),
/// so Git's own registration check is a second line of defence.
pub fn run(repo: &Repo, branch: &str, force: bool, cwd: &Path) -> Result<()> {
    // 1. The branch must have a worktree, and it must not be the main one.
    let wt = repo
        .find_by_branch(branch)
        .ok_or_else(|| Error::WorktreeNotFound(branch.to_string()))?;
    if repo.is_main(wt) {
        return Err(Error::MainWorktree(branch.to_string()));
    }

    // 2. Prove BranchForge owns it: exact managed path for this branch,
    //    inside a plain (non-symlink) managed root. `--force` never bypasses this.
    //    The path check and the root check are separate calls so each failure
    //    gets its own specific error.
    if !repo.layout.owns_path(wt) {
        return Err(Error::NotManaged {
            branch: branch.to_string(),
            path: wt.path.clone(),
            expected: repo.layout.path_for(branch)?,
        });
    }
    repo.layout.check_root()?;

    // 3. Refuse states where removal is surprising or impossible.
    if let Some(reason) = &wt.locked {
        let reason = if reason.is_empty() {
            String::new()
        } else {
            format!(" ({reason})")
        };
        return Err(Error::Locked {
            path: wt.path.clone(),
            reason,
        });
    }
    if wt.prunable.is_some() || !wt.path.is_dir() {
        return Err(Error::WorktreeMissing(wt.path.clone()));
    }
    if let (Ok(here), Ok(target)) = (fs::canonicalize(cwd), fs::canonicalize(&wt.path))
        && here.starts_with(&target)
    {
        return Err(Error::InsideWorktree(wt.path.clone()));
    }

    // 4. Anything Git can't give back needs --force: uncommitted work (tracked
    //    changes or untracked files) *and* ignored files/directories such as
    //    `.env` or `node_modules/`, which `git worktree remove` deletes silently.
    let state = read_state_with_ignored(&wt.path)?;
    if !force {
        if state.is_dirty() {
            return Err(Error::DirtyWorktree {
                path: wt.path.clone(),
                summary: state.dirty_summary(),
            });
        }
        if state.ignored > 0 {
            return Err(Error::IgnoredContent {
                path: wt.path.clone(),
                count: state.ignored,
                examples: state.ignored_examples.join(", "),
            });
        }
    }

    // 5. Delegate deletion to Git. `--` guards against option-like paths.
    let mut args = vec!["worktree".as_ref(), "remove".as_ref()];
    if force {
        args.push("--force".as_ref());
    }
    args.push("--".as_ref());
    args.push(wt.path.as_os_str());
    repo.git().run(args)?;

    println!("Removed worktree {}", wt.path.display());
    if state.is_dirty() {
        println!("  discarded uncommitted changes: {}", state.dirty_summary());
    }
    if state.ignored > 0 {
        println!(
            "  deleted {} ignored item(s) (e.g. {})",
            state.ignored,
            state.ignored_examples.join(", ")
        );
    }
    println!(
        "  branch `{branch}` was kept; delete it with `git branch -d {branch}` if you no longer need it"
    );
    Ok(())
}
