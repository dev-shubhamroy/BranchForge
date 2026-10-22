//! Parsing of `git worktree list --porcelain`.
//!
//! Porcelain output is a series of blocks separated by blank lines:
//!
//! ```text
//! worktree /path/to/main
//! HEAD 0123456789abcdef...
//! branch refs/heads/main
//!
//! worktree /path/to/linked
//! HEAD fedcba9876543210...
//! detached
//! locked optional reason
//! ```

use std::path::PathBuf;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Full ref, e.g. `refs/heads/main`.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    /// `Some(reason)` when locked; the reason may be empty.
    pub locked: Option<String>,
    /// `Some(reason)` when git considers the entry stale (e.g. directory gone).
    pub prunable: Option<String>,
}

impl Worktree {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            head: None,
            branch: None,
            bare: false,
            detached: false,
            locked: None,
            prunable: None,
        }
    }

    /// Branch name without the `refs/heads/` prefix.
    pub fn branch_name(&self) -> Option<&str> {
        self.branch.as_deref()?.strip_prefix("refs/heads/")
    }

    /// First 7 characters of the HEAD commit, or `-` if unknown.
    pub fn short_head(&self) -> &str {
        match &self.head {
            Some(h) => &h[..h.len().min(7)],
            None => "-",
        }
    }
}

/// Parse the full output of `git worktree list --porcelain`.
///
/// Unknown attribute lines are ignored so newer Git versions don't break us.
/// Limitation: paths containing newlines are not supported (we deliberately
/// avoid `-z` so Git versions older than 2.36 still work).
pub fn parse_porcelain(text: &str) -> Result<Vec<Worktree>> {
    let mut worktrees = Vec::new();
    let mut current: Option<Worktree> = None;

    for line in text.lines() {
        if line.is_empty() {
            worktrees.extend(current.take());
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));

        if key == "worktree" {
            worktrees.extend(current.take());
            if value.is_empty() {
                return Err(Error::Parse("`worktree` line with an empty path".into()));
            }
            current = Some(Worktree::new(PathBuf::from(value)));
            continue;
        }

        let wt = current
            .as_mut()
            .ok_or_else(|| Error::Parse(format!("`{key}` appeared before any `worktree` line")))?;
        match key {
            "HEAD" => wt.head = Some(value.to_string()),
            "branch" => wt.branch = Some(value.to_string()),
            "bare" => wt.bare = true,
            "detached" => wt.detached = true,
            "locked" => wt.locked = Some(value.to_string()),
            "prunable" => wt.prunable = Some(value.to_string()),
            _ => {}
        }
    }
    worktrees.extend(current);
    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
worktree /home/u/proj
HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
branch refs/heads/main

worktree /home/u/proj.worktrees/feature%2Flogin
HEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
branch refs/heads/feature/login
locked in use on usb drive

worktree /home/u/scratch dir
HEAD cccccccccccccccccccccccccccccccccccccccc
detached
prunable gitdir file points to non-existent location
";

    #[test]
    fn parses_multiple_blocks() {
        let wts = parse_porcelain(SAMPLE).unwrap();
        assert_eq!(wts.len(), 3);

        assert_eq!(wts[0].path, PathBuf::from("/home/u/proj"));
        assert_eq!(wts[0].branch_name(), Some("main"));
        assert_eq!(wts[0].short_head(), "aaaaaaa");
        assert!(wts[0].locked.is_none());

        assert_eq!(wts[1].branch_name(), Some("feature/login"));
        assert_eq!(wts[1].locked.as_deref(), Some("in use on usb drive"));

        // Paths with spaces survive intact; detached has no branch.
        assert_eq!(wts[2].path, PathBuf::from("/home/u/scratch dir"));
        assert!(wts[2].detached);
        assert_eq!(wts[2].branch_name(), None);
        assert!(wts[2].prunable.is_some());
    }

    #[test]
    fn locked_without_reason_is_still_locked() {
        let wts = parse_porcelain("worktree /a\nHEAD abc\nbranch refs/heads/x\nlocked\n").unwrap();
        assert_eq!(wts[0].locked.as_deref(), Some(""));
    }

    #[test]
    fn bare_repository_is_flagged() {
        let wts = parse_porcelain("worktree /srv/repo.git\nbare\n").unwrap();
        assert!(wts[0].bare);
        assert_eq!(wts[0].head, None);
        assert_eq!(wts[0].short_head(), "-");
    }

    #[test]
    fn tolerates_missing_trailing_blank_line_and_unknown_keys() {
        let wts = parse_porcelain("worktree /a\nfuture-key value\nHEAD abc\n").unwrap();
        assert_eq!(wts.len(), 1);
        assert_eq!(wts[0].head.as_deref(), Some("abc"));
    }

    #[test]
    fn empty_output_is_no_worktrees() {
        assert!(parse_porcelain("").unwrap().is_empty());
    }

    #[test]
    fn attribute_before_worktree_is_an_error() {
        assert!(matches!(
            parse_porcelain("HEAD abc\n"),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn empty_worktree_path_is_an_error() {
        assert!(matches!(
            parse_porcelain("worktree \n"),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn non_heads_ref_has_no_branch_name() {
        let wts = parse_porcelain("worktree /a\nbranch refs/remotes/o/x\n").unwrap();
        assert_eq!(wts[0].branch_name(), None);
    }
}
