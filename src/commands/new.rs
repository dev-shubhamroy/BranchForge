use std::fs;

use crate::error::{Error, Result};
use crate::repo::Repo;

pub fn run(repo: &Repo, branch: &str, base: Option<&str>) -> Result<()> {
    let git = repo.git();

    // 1. Validate everything before touching the filesystem.
    let target = repo.layout.path_for(branch)?; // syntax checks + safe path
    let full_ref = format!("refs/heads/{branch}");
    if !git.succeeds(["check-ref-format", full_ref.as_str()])? {
        return Err(Error::InvalidBranchName {
            name: branch.to_string(),
            reason: "rejected by `git check-ref-format`".to_string(),
        });
    }

    if let Some(existing) = repo.find_by_branch(branch) {
        return Err(Error::BranchCheckedOut {
            branch: branch.to_string(),
            path: existing.path.clone(),
        });
    }

    let branch_exists = git.succeeds(["show-ref", "--verify", "--quiet", full_ref.as_str()])?;
    if branch_exists && base.is_some() {
        return Err(Error::BaseWithExistingBranch(branch.to_string()));
    }

    let start_point = base.unwrap_or("HEAD");
    if !branch_exists {
        if start_point.starts_with('-') {
            return Err(Error::InvalidBase(start_point.to_string()));
        }
        let spec = format!("{start_point}^{{commit}}");
        let resolves = git.succeeds(["rev-parse", "--verify", "--quiet", spec.as_str()])?;
        if !resolves {
            // `HEAD` is a symbolic ref to a branch that has no commit yet.
            let unborn_head =
                start_point == "HEAD" && git.succeeds(["symbolic-ref", "--quiet", "HEAD"])?;
            return Err(if unborn_head {
                Error::NoCommits
            } else {
                Error::InvalidBase(start_point.to_string())
            });
        }
    }

    // 2. Prepare the managed root and make sure we never overwrite anything.
    repo.layout.ensure_root()?;
    // symlink_metadata also catches dangling symlinks, which `exists()` would miss.
    if fs::symlink_metadata(&target).is_ok() {
        return Err(Error::PathExists(target));
    }

    // 3. Let Git do the real work. `--` stops option parsing, so a branch or
    //    path can never be mistaken for a flag.
    if branch_exists {
        git.run([
            "worktree".as_ref(),
            "add".as_ref(),
            "--".as_ref(),
            target.as_os_str(),
            branch.as_ref(),
        ])?;
    } else {
        git.run([
            "worktree".as_ref(),
            "add".as_ref(),
            "-b".as_ref(),
            branch.as_ref(),
            "--".as_ref(),
            target.as_os_str(),
            start_point.as_ref(),
        ])?;
    }

    if branch_exists {
        println!("Created worktree for existing branch `{branch}`");
    } else {
        println!("Created worktree and new branch `{branch}` (from `{start_point}`)");
    }
    println!("  path: {}", target.display());
    Ok(())
}
