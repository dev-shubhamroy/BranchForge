//! One module per subcommand. Each exposes a `run` function that takes an
//! already-discovered [`Repo`](crate::repo::Repo) and prints to stdout.

pub mod jobs;
pub mod list;
pub mod logs;
pub mod new;
pub mod remove;
pub mod run;
pub mod status;
pub mod stop;
