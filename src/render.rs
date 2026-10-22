//! Plain-text table rendering. Pure functions (no I/O) so they are unit-testable.

use crate::inspect::{Entry, Health};

/// `clean`, `dirty (2 modified, 1 untracked)`, `missing`, plus a `locked` flag.
pub fn describe_state(entry: &Entry) -> String {
    let base = match &entry.health {
        Health::Ready(s) if s.is_dirty() => format!("dirty ({})", s.dirty_summary()),
        Health::Ready(_) => "clean".to_string(),
        Health::Missing => "missing".to_string(),
        Health::Unreadable(_) => "unreadable".to_string(),
    };
    match &entry.worktree.locked {
        Some(_) => format!("{base}, locked"),
        None => base,
    }
}

/// `+2/-1 vs origin/main`, or `-` when there is no upstream.
pub fn describe_sync(entry: &Entry) -> String {
    match &entry.health {
        Health::Ready(s) => match &s.upstream {
            Some(up) => format!("+{}/-{} vs {up}", s.ahead, s.behind),
            None => "-".to_string(),
        },
        _ => "-".to_string(),
    }
}

fn describe_branch(entry: &Entry) -> String {
    let wt = &entry.worktree;
    match wt.branch_name() {
        Some(name) => name.to_string(),
        None => format!("(detached {})", wt.short_head()),
    }
}

/// Render entries as an aligned table. The last column (path) is not padded.
pub fn render_table(entries: &[Entry]) -> String {
    let header = ["BRANCH", "KIND", "STATE", "SYNC", "PATH"];
    let mut rows: Vec<[String; 5]> = vec![header.map(String::from)];
    for e in entries {
        rows.push([
            describe_branch(e),
            e.kind.label().to_string(),
            describe_state(e),
            describe_sync(e),
            e.worktree.path.display().to_string(),
        ]);
    }

    let mut widths = [0usize; 5];
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }

    let mut out = String::new();
    for row in &rows {
        let last = row.len() - 1;
        for (i, cell) in row.iter().enumerate() {
            if i == last {
                out.push_str(cell);
            } else {
                out.push_str(cell);
                let pad = widths[i] - cell.chars().count() + 2;
                out.extend(std::iter::repeat_n(' ', pad));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::Kind;
    use crate::state::WorktreeState;
    use crate::worktree::parse_porcelain;

    fn entry(porcelain: &str, kind: Kind, health: Health) -> Entry {
        Entry {
            worktree: parse_porcelain(porcelain).unwrap().remove(0),
            kind,
            health,
        }
    }

    #[test]
    fn describes_clean_dirty_missing_and_locked() {
        let clean = entry(
            "worktree /a\nbranch refs/heads/x\n",
            Kind::Main,
            Health::Ready(WorktreeState::default()),
        );
        assert_eq!(describe_state(&clean), "clean");

        let dirty_state = WorktreeState {
            unstaged: 2,
            untracked: 1,
            ..Default::default()
        };
        let dirty = entry(
            "worktree /a\nbranch refs/heads/x\nlocked\n",
            Kind::Managed,
            Health::Ready(dirty_state),
        );
        assert_eq!(
            describe_state(&dirty),
            "dirty (2 modified, 1 untracked), locked"
        );

        let missing = entry(
            "worktree /a\nbranch refs/heads/x\n",
            Kind::External,
            Health::Missing,
        );
        assert_eq!(describe_state(&missing), "missing");
    }

    #[test]
    fn describes_sync_with_and_without_upstream() {
        let with = WorktreeState {
            upstream: Some("origin/x".into()),
            ahead: 2,
            behind: 1,
            ..Default::default()
        };
        let e = entry("worktree /a\n", Kind::Main, Health::Ready(with));
        assert_eq!(describe_sync(&e), "+2/-1 vs origin/x");

        let e = entry(
            "worktree /a\n",
            Kind::Main,
            Health::Ready(WorktreeState::default()),
        );
        assert_eq!(describe_sync(&e), "-");
    }

    #[test]
    fn table_is_aligned_and_handles_detached_heads() {
        let a = entry(
            "worktree /repo\nHEAD 1234567890\nbranch refs/heads/main\n",
            Kind::Main,
            Health::Ready(WorktreeState::default()),
        );
        let b = entry(
            "worktree /repo.worktrees/x\nHEAD abcdefabcdef\ndetached\n",
            Kind::External,
            Health::Ready(WorktreeState::default()),
        );
        let table = render_table(&[a, b]);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[2].starts_with("(detached abcdefa)"));
        // Every row's KIND column starts at the same offset.
        let col = lines[0].find("KIND").unwrap();
        assert!(lines[1][col..].starts_with("main"));
        assert!(lines[2][col..].starts_with("external"));
    }
}
