use crate::error::Result;
use crate::jobs::{JobRecord, JobStore, format_utc};
use crate::render::render_columns;
use crate::repo::Repo;

/// Longest command text shown in the table.
const MAX_COMMAND_CHARS: usize = 60;

pub fn run(repo: &Repo) -> Result<()> {
    let records = match JobStore::open_existing(repo)? {
        Some(store) => store.list_lenient()?,
        None => Vec::new(),
    };
    if records.is_empty() {
        println!("No jobs yet. Start one with `branchforge run <branch> -- <program> [args...]`.");
        return Ok(());
    }

    let mut rows = vec![
        ["ID", "BRANCH", "STATE", "STARTED", "EXIT", "COMMAND"]
            .map(String::from)
            .to_vec(),
    ];
    rows.extend(records.iter().map(row));
    print!("{}", render_columns(&rows));
    Ok(())
}

fn row(record: &JobRecord) -> Vec<String> {
    match &record.meta {
        Ok(meta) => vec![
            record.id.clone(),
            meta.branch.clone(),
            record.state.label().to_string(),
            record
                .started_at_ms()
                .map(format_utc)
                .unwrap_or_else(|| "-".into()),
            record.exit_text(),
            truncate(&meta.command_text(), MAX_COMMAND_CHARS),
        ],
        Err(reason) => vec![
            record.id.clone(),
            "-".into(),
            record.state.label().to_string(),
            "-".into(),
            "-".into(),
            truncate(&format!("<{reason}>"), MAX_COMMAND_CHARS),
        ],
    }
}

/// Shorten to `max` characters, collapsing newlines so a hostile or odd
/// command line can't break the table.
fn truncate(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.chars().count() <= max {
        flat
    } else {
        let mut short: String = flat.chars().take(max - 1).collect();
        short.push('…');
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_shortens_and_flattens_control_characters() {
        assert_eq!(truncate("echo hi", 60), "echo hi");
        assert_eq!(truncate("a\nb\tc", 60), "a b c");
        let long = "x".repeat(100);
        let short = truncate(&long, 10);
        assert_eq!(short.chars().count(), 10);
        assert!(short.ends_with('…'));
        // Multi-byte characters are cut on character boundaries, not bytes.
        assert_eq!(truncate("ééééé", 3).chars().count(), 3);
    }
}
