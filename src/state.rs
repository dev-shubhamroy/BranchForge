//! Parsing of `git status --porcelain=v2 --branch`.
//!
//! Relevant line shapes (see `git help status`, "Porcelain Format Version 2"):
//!
//! ```text
//! # branch.upstream origin/main
//! # branch.ab +2 -1
//! 1 <XY> ...           ordinary changed entry
//! 2 <XY> ...           renamed/copied entry
//! u <XY> ...           unmerged (conflicted) entry
//! ? <path>             untracked
//! ! <path>             ignored (only present when `--ignored` was requested)
//! ```
//!
//! With `--ignored` (traditional mode) Git lists an ignored *file* as
//! `! path/file` and a wholly ignored *directory* as one collapsed
//! `! path/dir/` entry, so counting `!` lines counts "things that would be
//! lost", not individual files.
//!
//! `XY` is two characters: `X` = staged status, `Y` = unstaged status, and `.`
//! means "unchanged" in that column.

use crate::error::{Error, Result};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorktreeState {
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Entries with a staged change.
    pub staged: u32,
    /// Entries with an unstaged change in tracked files.
    pub unstaged: u32,
    pub conflicted: u32,
    pub untracked: u32,
    /// Ignored entries (files or collapsed directories). Always 0 unless the
    /// status was read with `--ignored`; deliberately *not* part of `is_dirty`.
    pub ignored: u32,
    /// The first few ignored paths, for error messages.
    pub ignored_examples: Vec<String>,
}

/// How many example ignored paths to keep for messages.
const MAX_IGNORED_EXAMPLES: usize = 3;

impl WorktreeState {
    /// "Dirty" matches what `git worktree remove` refuses to delete without
    /// `--force`: any tracked modification *or* untracked file. Ignored files
    /// are intentionally excluded so `list`/`status` keep their normal meaning;
    /// `remove` checks [`WorktreeState::ignored`] separately.
    pub fn is_dirty(&self) -> bool {
        self.staged + self.unstaged + self.conflicted + self.untracked > 0
    }

    /// E.g. `2 staged, 1 modified, 3 untracked`; empty string when clean.
    pub fn dirty_summary(&self) -> String {
        let parts = [
            (self.conflicted, "conflicted"),
            (self.staged, "staged"),
            (self.unstaged, "modified"),
            (self.untracked, "untracked"),
        ];
        parts
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, label)| format!("{n} {label}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn parse_status_v2(text: &str) -> Result<WorktreeState> {
    let mut state = WorktreeState::default();

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            if let Some(upstream) = rest.strip_prefix("branch.upstream ") {
                state.upstream = Some(upstream.to_string());
            } else if let Some(ab) = rest.strip_prefix("branch.ab ") {
                (state.ahead, state.behind) = parse_ahead_behind(ab)?;
            }
            continue;
        }

        match line.split_at_checked(1) {
            Some(("1" | "2", rest)) => {
                let xy = rest.trim_start().get(..2).ok_or_else(|| {
                    Error::Parse(format!("status entry without an XY field: `{line}`"))
                })?;
                let mut chars = xy.chars();
                let (x, y) = (chars.next().unwrap_or('.'), chars.next().unwrap_or('.'));
                if x != '.' {
                    state.staged += 1;
                }
                if y != '.' {
                    state.unstaged += 1;
                }
            }
            Some(("u", _)) => state.conflicted += 1,
            Some(("?", _)) => state.untracked += 1,
            Some(("!", rest)) => {
                state.ignored += 1;
                if state.ignored_examples.len() < MAX_IGNORED_EXAMPLES {
                    state.ignored_examples.push(rest.trim_start().to_string());
                }
            }
            _ => {} // unknown lines
        }
    }
    Ok(state)
}

fn parse_ahead_behind(s: &str) -> Result<(u32, u32)> {
    let bad = || Error::Parse(format!("bad branch.ab value `{s}`"));
    let (ahead, behind) = s.split_once(' ').ok_or_else(bad)?;
    let ahead = ahead
        .strip_prefix('+')
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;
    let behind = behind
        .strip_prefix('-')
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;
    Ok((ahead, behind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_repo_without_upstream() {
        let s = parse_status_v2("# branch.oid abc\n# branch.head main\n").unwrap();
        assert_eq!(s, WorktreeState::default());
        assert!(!s.is_dirty());
        assert_eq!(s.dirty_summary(), "");
    }

    #[test]
    fn upstream_and_ahead_behind() {
        let s = parse_status_v2(
            "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +3 -2\n",
        )
        .unwrap();
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (3, 2));
        assert!(!s.is_dirty());
    }

    #[test]
    fn counts_each_kind_of_change() {
        let text = "\
# branch.head main
1 M. N... 100644 100644 100644 aaa bbb staged.txt
1 .M N... 100644 100644 100644 aaa bbb unstaged.txt
1 MM N... 100644 100644 100644 aaa bbb both.txt
2 R. N... 100644 100644 100644 aaa bbb R100 new.txt\told.txt
u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt
? untracked one.txt
? other.txt
! ignored.log
";
        let s = parse_status_v2(text).unwrap();
        assert_eq!(s.staged, 3); // staged.txt, both.txt, rename
        assert_eq!(s.unstaged, 2); // unstaged.txt, both.txt
        assert_eq!(s.conflicted, 1);
        assert_eq!(s.untracked, 2);
        assert!(s.is_dirty());
        assert_eq!(
            s.dirty_summary(),
            "1 conflicted, 3 staged, 2 modified, 2 untracked"
        );
    }

    #[test]
    fn untracked_only_is_dirty() {
        let s = parse_status_v2("? new.txt\n").unwrap();
        assert!(s.is_dirty());
        assert_eq!(s.dirty_summary(), "1 untracked");
    }

    #[test]
    fn ignored_files_and_collapsed_directories_are_counted_but_not_dirty() {
        // Shapes taken from real `git status --porcelain=v2 --ignored` output.
        let text = "\
# branch.head feat
! .env
! dist/out.js
! node_modules/
! untracked/.env
";
        let s = parse_status_v2(text).unwrap();
        assert_eq!(s.ignored, 4);
        assert_eq!(
            s.ignored_examples,
            vec![".env", "dist/out.js", "node_modules/"]
        );
        assert!(
            !s.is_dirty(),
            "ignored content must not change dirty semantics"
        );
        assert_eq!(s.dirty_summary(), "");
    }

    #[test]
    fn ignored_is_zero_when_status_was_not_asked_for_ignored() {
        let s = parse_status_v2("# branch.head main\n? new.txt\n").unwrap();
        assert_eq!(s.ignored, 0);
        assert!(s.ignored_examples.is_empty());
    }

    #[test]
    fn ignored_alongside_real_changes_keeps_both() {
        let s =
            parse_status_v2("1 .M N... 100644 100644 100644 a b f.txt\n? u.txt\n! .env\n").unwrap();
        assert!(s.is_dirty());
        assert_eq!((s.unstaged, s.untracked, s.ignored), (1, 1, 1));
    }

    #[test]
    fn malformed_ahead_behind_is_an_error() {
        assert!(parse_status_v2("# branch.ab 3 2\n").is_err());
        assert!(parse_status_v2("# branch.ab +x -y\n").is_err());
    }

    #[test]
    fn entry_without_xy_is_an_error() {
        assert!(parse_status_v2("1 \n").is_err());
    }
}
