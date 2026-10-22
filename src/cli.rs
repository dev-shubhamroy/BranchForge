//! Command-line interface definition (clap derive).

use clap::{Parser, Subcommand};

use crate::supervisor::DEFAULT_STOP_GRACE_SECS;

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

        /// Remove even if the worktree has uncommitted changes or ignored files
        /// (permanently deletes them). Never overrides ownership, symlink,
        /// main-worktree, locked, missing-directory, current-directory or
        /// running-job protections.
        #[arg(short, long)]
        force: bool,
    },

    /// Run a program as a supervised background job in a managed worktree
    #[command(after_help = "Example: branchforge run feature/login -- cargo test --release")]
    Run {
        /// Branch whose managed worktree the program runs in
        branch: String,

        /// The program and its arguments, after `--` (no shell is involved)
        #[arg(last = true, required = true, value_name = "PROGRAM_AND_ARGS")]
        command: Vec<String>,
    },

    /// List jobs with id, branch, state, start time and exit status
    Jobs,

    /// Show a job's captured output
    Logs {
        /// Job id from `branchforge jobs`
        job_id: String,

        /// Print only stdout, raw (no section headers)
        #[arg(long)]
        stdout: bool,

        /// Print only stderr, raw (no section headers)
        #[arg(long)]
        stderr: bool,
    },

    /// Stop a running job (SIGTERM, then SIGKILL after the grace period)
    Stop {
        /// Job id from `branchforge jobs`
        job_id: String,

        /// Seconds to wait after SIGTERM before sending SIGKILL (0 = kill at once)
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_STOP_GRACE_SECS)]
        grace: u64,
    },

    /// Internal: the supervisor process for one job. Not for direct use.
    #[command(name = "__supervise", hide = true)]
    Supervise {
        /// Job id to supervise
        job_id: String,
    },
}

impl Commands {
    /// Commands that need file locking and process groups (the job layer).
    pub fn needs_job_support(&self) -> bool {
        matches!(
            self,
            Commands::Run { .. }
                | Commands::Jobs
                | Commands::Logs { .. }
                | Commands::Stop { .. }
                | Commands::Supervise { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Commands {
        let mut full = vec!["branchforge"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).unwrap().command
    }

    #[test]
    fn exactly_the_job_commands_need_job_support() {
        for (args, needs) in [
            (&["run", "b", "--", "x"][..], true),
            (&["jobs"], true),
            (&["logs", "1"], true),
            (&["stop", "1"], true),
            (&["__supervise", "1"], true),
            (&["new", "b"], false),
            (&["list"], false),
            (&["status"], false),
            (&["remove", "b"], false),
        ] {
            assert_eq!(parse(args).needs_job_support(), needs, "{args:?}");
        }
    }

    #[test]
    fn run_requires_the_double_dash_and_a_program() {
        assert!(Cli::try_parse_from(["branchforge", "run", "b"]).is_err());
        assert!(Cli::try_parse_from(["branchforge", "run", "b", "x"]).is_err());
        let Commands::Run { command, .. } = parse(&["run", "b", "--", "ls", "-la", "--color"])
        else {
            panic!("expected run");
        };
        assert_eq!(
            command,
            ["ls", "-la", "--color"],
            "flags after `--` belong to the program"
        );
    }
}
