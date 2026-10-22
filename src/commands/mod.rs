//! One module per subcommand. Each exposes a `run` function that takes an
//! already-discovered [`Repo`](crate::repo::Repo) and prints to stdout.

pub mod list;
pub mod new;
pub mod remove;
pub mod status;
