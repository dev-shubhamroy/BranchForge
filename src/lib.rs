//! BranchForge: a small CLI for managing Git worktrees safely.
//!
//! Layering (each layer only depends on the ones above it in this list):
//! `error` -> `git` (subprocess) -> `worktree`/`state` (parsers) -> `layout`
//! (path rules) -> `repo`/`inspect` (assembled view) -> `render` -> `commands`
//! -> `cli`/`run`.

pub mod cli;
pub mod commands;
pub mod error;
pub mod git;
pub mod inspect;
pub mod layout;
pub mod render;
pub mod repo;
pub mod state;
pub mod worktree;

use cli::{Cli, Commands};
use error::{Error, Result};
use repo::Repo;

/// Execute a parsed command line against the repository containing the
/// current directory.
pub fn run(cli: Cli) -> Result<()> {
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
    }
}
