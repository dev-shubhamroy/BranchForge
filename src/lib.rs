//! BranchForge: a small CLI for managing Git worktrees safely.
//!
//! Layering (each layer only depends on the ones above it in this list):
//! `error` -> `git` (subprocess) -> `worktree`/`state` (parsers) -> `layout`
//! (path rules) -> `repo`/`inspect` (assembled view) -> `process` (OS
//! primitives) -> `jobs` (registry) -> `supervisor` -> `render` -> `commands`
//! -> `cli`/`run`.

pub mod cli;
pub mod commands;
pub mod error;
pub mod git;
pub mod inspect;
pub mod jobs;
pub mod layout;
pub mod process;
pub mod render;
pub mod repo;
pub mod state;
pub mod supervisor;
pub mod worktree;

use cli::{Cli, Commands};
use error::{Error, Result};
use repo::Repo;

/// Execute a parsed command line against the repository containing the
/// current directory.
pub fn run(cli: Cli) -> Result<()> {
    // Before any I/O: job commands on an unsupported platform must fail cleanly
    // and leave no state behind (not even a half-created job directory).
    if cli.command.needs_job_support() {
        process::ensure_supported()?;
    }
    let cwd = std::env::current_dir().map_err(|source| Error::Io {
        path: ".".into(),
        source,
    })?;
    let repo = Repo::discover(&cwd)?;

    match cli.command {
        Commands::New { branch, base } => commands::new::run(&repo, &branch, base.as_deref()),
        Commands::List => commands::list::run(&repo),
        Commands::Status => commands::status::run(&repo),
        Commands::Remove { branch, force } => commands::remove::run(&repo, &branch, force, &cwd),
        Commands::Run { branch, command } => commands::run::run(&repo, &branch, &command),
        Commands::Jobs => commands::jobs::run(&repo),
        Commands::Logs {
            job_id,
            stdout,
            stderr,
        } => commands::logs::run(&repo, &job_id, stdout, stderr),
        Commands::Stop { job_id, grace } => commands::stop::run(&repo, &job_id, grace),
        Commands::Supervise { job_id } => supervisor::run(&repo, &job_id),
    }
}
