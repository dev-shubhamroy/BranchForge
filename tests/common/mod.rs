//! Shared helpers for the integration tests: run the real `branchforge` binary
//! against temporary Git repositories, with Git isolated from the developer's
//! global/system configuration.
#![allow(dead_code)] // each test crate uses a different subset

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

pub fn isolate(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
}

/// Run git in `dir`, panicking with stderr on failure. Returns trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = isolate(Command::new("git").arg("-C").arg(dir).args(args))
        .output()
        .expect("git should be installed");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

pub struct Fixture {
    _tmp: TempDir,
    /// Canonicalized temp root (macOS tmp dirs are behind a symlink).
    pub root: PathBuf,
    pub repo: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "initial"]);
        Self {
            _tmp: tmp,
            root,
            repo,
        }
    }

    pub fn managed_root(&self) -> PathBuf {
        self.root.join("repo.worktrees")
    }

    pub fn wt_path(&self, encoded: &str) -> PathBuf {
        self.managed_root().join(encoded)
    }

    pub fn bf_in(&self, cwd: &Path, args: &[&str]) -> Output {
        isolate(
            Command::new(env!("CARGO_BIN_EXE_branchforge"))
                .current_dir(cwd)
                .args(args),
        )
        .output()
        .unwrap()
    }

    /// Start `branchforge` without waiting for it (stdout/stderr are piped).
    pub fn spawn_bf(&self, args: &[&str]) -> std::process::Child {
        isolate(
            Command::new(env!("CARGO_BIN_EXE_branchforge"))
                .current_dir(&self.repo)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()),
        )
        .spawn()
        .unwrap()
    }

    pub fn bf(&self, args: &[&str]) -> Output {
        self.bf_in(&self.repo, args)
    }
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
#[track_caller]
pub fn assert_ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "expected success, stderr: {}",
        stderr(out)
    );
    stdout(out)
}
#[track_caller]
pub fn assert_fails(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "expected failure, stdout: {}",
        stdout(out)
    );
    stderr(out)
}

// ------------------------------------------------------------ job helpers

use std::time::{Duration, Instant};

/// Poll `cond` every 20ms until it is true or `timeout` passes. Deadline-based,
/// so tests never depend on a fixed sleep being "long enough".
pub fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Generous upper bound for things that normally take milliseconds.
pub const PATIENCE: Duration = Duration::from_secs(30);

impl Fixture {
    /// Where BranchForge keeps job data for this repository.
    pub fn jobs_dir(&self) -> PathBuf {
        self.repo.join(".git/branchforge/jobs")
    }

    pub fn job_dir(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(id)
    }

    /// Parsed `meta.json` of a job.
    pub fn job_meta(&self, id: &str) -> serde_json::Value {
        let text = fs::read_to_string(self.job_dir(id).join("meta.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// The state column of `branchforge jobs` for `id`, if the job is listed.
    pub fn job_state(&self, id: &str) -> Option<String> {
        let out = self.bf(&["jobs"]);
        stdout(&out)
            .lines()
            .find(|l| l.split_whitespace().next() == Some(id))
            .and_then(|l| l.split_whitespace().nth(2).map(str::to_string))
    }

    pub fn wait_for_state(&self, id: &str, state: &str) {
        let reached = wait_until(PATIENCE, || self.job_state(id).as_deref() == Some(state));
        assert!(
            reached,
            "job {id} never reached `{state}` (last seen: {:?})\n{}",
            self.job_state(id),
            stdout(&self.bf(&["jobs"]))
        );
    }

    /// Run `program args...` as a job on `branch`, returning the new job id.
    pub fn run_job(&self, branch: &str, command: &[&str]) -> String {
        let mut args = vec!["run", branch, "--"];
        args.extend_from_slice(command);
        let text = assert_ok(&self.bf(&args));
        text.lines()
            .next()
            .and_then(|l| l.strip_prefix("Started job "))
            .and_then(|l| l.split_whitespace().next())
            .unwrap_or_else(|| panic!("unexpected `run` output: {text}"))
            .to_string()
    }
}

/// A script argument list for a job that stays running until `gate` exists.
pub const GATE_SCRIPT: &str = "echo started; while [ ! -e \"$1\" ]; do sleep 0.05; done; echo done";

pub fn process_alive(pid: u32) -> bool {
    // Signal 0 only checks existence/permission; it delivers nothing.
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// If a test fails while a job is running, ask its supervisor to kill it so no
/// `sleep 60` outlives the test run. (Supervisors also kill their program when
/// their job directory disappears.)
impl Drop for Fixture {
    fn drop(&mut self) {
        let Ok(entries) = fs::read_dir(self.jobs_dir()) else {
            return;
        };
        let mut pending = Vec::new();
        for entry in entries.flatten() {
            let dir = entry.path();
            let meta = dir.join("meta.json");
            if meta.is_file()
                && !fs::read_to_string(&meta)
                    .unwrap_or_default()
                    .contains("\"finished\"")
            {
                let _ = fs::write(dir.join("stop-requested"), "0");
                pending.push(meta);
            }
        }
        wait_until(Duration::from_secs(10), || {
            pending.iter().all(|meta| {
                fs::read_to_string(meta)
                    .map(|s| s.contains("\"finished\""))
                    .unwrap_or(true)
            })
        });
    }
}
