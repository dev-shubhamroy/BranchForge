//! A thin, safe wrapper around the `git` executable.
//!
//! Design rules:
//! * Arguments are passed as separate `OsStr`s to [`Command`]; there is never a
//!   shell and never string interpolation into a command line, so branch names
//!   or paths cannot inject extra arguments or shell syntax.
//! * Every call runs as `git -C <dir> ...`, so behaviour never depends on the
//!   process's current directory.
//! * stdin is closed and prompts are disabled so a misbehaving repo can't hang
//!   the tool waiting for input.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// Environment variables that would silently override `git -C <dir>`.
const OVERRIDING_ENV: [&str; 4] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
];

/// Captured result of one git invocation.
#[derive(Debug)]
pub struct GitOutput {
    /// Exit code, or `None` if the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Human-readable argument list, used in error messages.
    command: String,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    fn into_failure(self) -> Error {
        Error::GitFailed {
            command: self.command,
            status: match self.code {
                Some(code) => format!("exit code {code}"),
                None => "terminated by signal".to_string(),
            },
            stderr: self.stderr.trim().to_string(),
        }
    }
}

/// Runs git commands against one directory.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
}

impl Git {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Run git and return the captured output regardless of exit status.
    /// Fails only if git can't be started or its stdout isn't UTF-8.
    pub fn output<I, S>(&self, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|a| a.as_ref().to_os_string())
            .collect();
        let command = args
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");

        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.dir)
            .args(&args)
            .env("GIT_TERMINAL_PROMPT", "0")
            // Read-only commands such as `status` must not take the index lock.
            .env("GIT_OPTIONAL_LOCKS", "0")
            // Stable (English) messages regardless of the user's locale.
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        for var in OVERRIDING_ENV {
            cmd.env_remove(var);
        }

        let out = cmd.output().map_err(Error::GitSpawn)?;
        let stdout = String::from_utf8(out.stdout).map_err(|_| Error::NonUtf8Output {
            command: command.clone(),
        })?;
        Ok(GitOutput {
            code: out.status.code(),
            stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            command,
        })
    }

    /// Run git, require exit code 0, and return stdout.
    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.output(args)?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(out.into_failure())
        }
    }

    /// For yes/no plumbing commands (`show-ref --quiet`, `check-ref-format`, ...):
    /// exit 0 means `true`, exit 1 means `false`, anything else is a real error.
    pub fn succeeds<I, S>(&self, args: I) -> Result<bool>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.output(args)?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(out.into_failure()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
