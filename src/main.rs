use std::error::Error as _;
use std::process::ExitCode;

use clap::Parser;

use branchforge::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match branchforge::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            let mut source = err.source();
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}
