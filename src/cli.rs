//! Command-line interface definition (clap derive).

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "branchforge",
    version,
    about = "Manage Git worktrees in a predictable, safe layout",
    long_about = "BranchForge creates one Git worktree per branch in a sibling directory \
named `<repo>.worktrees`, and only ever removes worktrees it can prove it manages."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Create a worktree for a branch (creating the branch if needed)
    New {
        /// Branch to check out in the new worktree
        branch: String,

        /// Commit-ish to start a *new* branch from (default: HEAD)
        #[arg(short, long, value_name = "REF")]
        base: Option<String>,
    },

    /// List all worktrees with branch, kind, state and path
    List,

    /// Summarize the repository and its managed worktrees
    Status,

    /// Remove a managed worktree (the branch itself is kept)
    Remove {
        /// Branch whose worktree should be removed
        branch: String,

        /// Remove even if the worktree has uncommitted changes (discards them)
        #[arg(short, long)]
        force: bool,
    },
}
