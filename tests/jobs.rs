//! End-to-end tests for the job layer (`run`, `jobs`, `logs`, `stop`) and its
//! interaction with `remove`. Unix-only: jobs rely on `flock` and process groups.
//!
//! Timing policy: no test depends on a fixed sleep. Long-running jobs wait on a
//! "gate" file the test creates, and every wait is a deadline-based poll.
//! Tests only ever signal processes they spawned themselves (via BranchForge's
//! supervisors for jobs, or a `Victim` child they own).
#![cfg(unix)]

mod common;

use std::fs;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use serde_json::{Value, json};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// A harmless long-sleeping process owned by the test, used to prove BranchForge
/// never signals a PID just because metadata names it. Killed on drop.
struct Victim(Child);

impl Victim {
    fn spawn() -> Self {
        Victim(
            Command::new("sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
    fn still_running(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }
}

impl Drop for Victim {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Write a job directory by hand, as if a supervisor had left it behind.
fn craft_job(
    f: &Fixture,
    id: &str,
    branch: &str,
    worktree: &Path,
    command: &[&str],
    phase: Value,
    created_at_ms: u64,
) {
    let dir = f.job_dir(id);
    fs::create_dir_all(&dir).unwrap();
    for name in ["lock", "stdout.log", "stderr.log", "supervisor.log"] {
        fs::write(dir.join(name), "").unwrap();
    }
    let meta = json!({
        "version": 1,
        "id": id,
        "branch": branch,
        "worktree": worktree,
        "program": command[0],
        "args": &command[1..],
        "created_at_ms": created_at_ms,
        "phase": phase,
    });
    fs::write(
        dir.join("meta.json"),
        serde_json::to_vec_pretty(&meta).unwrap(),
    )
    .unwrap();
}

fn running_phase(pid: u32) -> Value {
    json!({"state": "running", "pid": pid, "supervisor_pid": pid, "started_at_ms": 1})
}

fn new_branch(f: &Fixture, branch: &str) {
    assert_ok(&f.bf(&["new", branch]));
}

// ------------------------------------------------------- launching & capture

#[test]
fn run_launches_a_command_and_records_success() {
    let f = Fixture::new();
    new_branch(&f, "feat");

    let text = assert_ok(&f.bf(&["run", "feat", "--", "echo", "hello"]));
    assert!(
        text.contains("Started job 1 on branch `feat`: echo hello"),
        "{text}"
    );
    assert!(
        text.contains("branchforge logs 1") && text.contains("branchforge stop 1"),
        "{text}"
    );

    f.wait_for_state("1", "succeeded");
    let jobs = assert_ok(&f.bf(&["jobs"]));
    let line = jobs.lines().find(|l| l.starts_with('1')).unwrap();
    assert!(
        line.contains("feat") && line.contains("echo hello") && line.contains('Z'),
        "{line}"
    );
    assert!(jobs.lines().next().unwrap().starts_with("ID"));

    assert_eq!(assert_ok(&f.bf(&["logs", "1", "--stdout"])), "hello\n");
    let both = assert_ok(&f.bf(&["logs", "1"]));
    assert!(both.contains("==> stdout <==\nhello\n"), "{both}");
    assert!(
        !both.contains("stderr"),
        "empty sections are skipped: {both}"
    );

    // Job data lives in the Git directory: invisible to `git status`, and the
    // managed worktree directory contains only the worktree.
    assert!(f.job_dir("1").join("meta.json").is_file());
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
    let status = assert_ok(&f.bf(&["status"]));
    assert!(!status.contains("Warning"), "{status}");
}

#[test]
fn nonzero_exit_is_recorded_as_failed_with_its_code() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    // `run` itself succeeds: the job was started. The job is what failed.
    f.run_job("feat", &["sh", "-c", "echo oops >&2; exit 3"]);
    f.wait_for_state("1", "failed");

    let jobs = assert_ok(&f.bf(&["jobs"]));
    let line = jobs.lines().find(|l| l.starts_with('1')).unwrap();
    assert!(
        line.split_whitespace().any(|w| w == "3"),
        "exit code shown: {line}"
    );
    assert!(
        line.contains("'echo oops >&2; exit 3'"),
        "command is quoted for display: {line}"
    );
    assert_eq!(assert_ok(&f.bf(&["logs", "1", "--stderr"])), "oops\n");
    assert_eq!(assert_ok(&f.bf(&["logs", "1", "--stdout"])), "");
}

#[test]
fn heavy_output_on_both_streams_does_not_deadlock() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    // 4 MB on each stream, interleaved: far beyond any pipe buffer.
    let script = "i=0; while [ $i -lt 40 ]; do \
        head -c 100000 /dev/zero | tr '\\0' a; \
        head -c 100000 /dev/zero | tr '\\0' b >&2; \
        i=$((i+1)); done";
    f.run_job("feat", &["sh", "-c", script]);
    f.wait_for_state("1", "succeeded");

    assert_eq!(
        fs::metadata(f.job_dir("1").join("stdout.log"))
            .unwrap()
            .len(),
        4_000_000
    );
    assert_eq!(
        fs::metadata(f.job_dir("1").join("stderr.log"))
            .unwrap()
            .len(),
        4_000_000
    );
    let out = f.bf(&["logs", "1", "--stdout"]);
    assert!(
        out.status.success()
            && out.stdout.len() == 4_000_000
            && out.stdout.iter().all(|&b| b == b'a')
    );
    let err = f.bf(&["logs", "1", "--stderr"]);
    assert!(err.stdout.len() == 4_000_000 && err.stdout.iter().all(|&b| b == b'b'));
}

#[test]
fn arguments_are_passed_verbatim_without_a_shell() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    f.run_job("feat", &["echo", "$HOME;|&*", "a b", "*", "$(whoami)"]);
    f.wait_for_state("1", "succeeded");
    assert_eq!(
        assert_ok(&f.bf(&["logs", "1", "--stdout"])),
        "$HOME;|&* a b * $(whoami)\n"
    );
}

#[test]
fn relative_programs_resolve_inside_the_worktree() {
    let f = Fixture::new();
    new_branch(&f, "feat/login");
    let wt = f.wt_path("feat%2Flogin");
    let script = wt.join("hello.sh");
    fs::write(&script, "#!/bin/sh\npwd\n").unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(&script)
        .status()
        .unwrap();

    f.run_job("feat/login", &["./hello.sh"]);
    f.wait_for_state("1", "succeeded");
    let out = assert_ok(&f.bf(&["logs", "1", "--stdout"]));
    assert_eq!(
        out.trim(),
        wt.to_str().unwrap(),
        "job ran with the worktree as its cwd"
    );
}

#[test]
fn missing_and_non_executable_programs_are_reported_and_recorded() {
    let f = Fixture::new();
    new_branch(&f, "feat");

    let err = assert_fails(&f.bf(&["run", "feat", "--", "definitely-not-a-real-program-xyz"]));
    assert!(
        err.contains("could not start") && err.contains("No such file"),
        "{err}"
    );
    f.wait_for_state("1", "start-failed");

    fs::write(f.wt_path("feat").join("noexec.sh"), "#!/bin/sh\necho hi\n").unwrap();
    let err = assert_fails(&f.bf(&["run", "feat", "--", "./noexec.sh"]));
    assert!(
        err.contains("could not start") && err.contains("Permission denied"),
        "{err}"
    );
    f.wait_for_state("2", "start-failed");

    // The failed attempts are inspectable, not silently dropped.
    assert_eq!(assert_ok(&f.bf(&["logs", "1"])), "");
    let jobs = assert_ok(&f.bf(&["jobs"]));
    assert_eq!(jobs.matches("start-failed").count(), 2, "{jobs}");
}

// ----------------------------------------------------- listing, logs, refresh

#[test]
fn jobs_and_logs_work_across_branches_and_from_any_worktree() {
    let f = Fixture::new();
    assert!(assert_ok(&f.bf(&["jobs"])).contains("No jobs yet"));
    new_branch(&f, "a");
    new_branch(&f, "b");

    f.run_job("a", &["echo", "from-a"]);
    f.run_job("b", &["sh", "-c", "echo from-b; echo b-err >&2"]);
    // `run` also works when invoked from inside a linked worktree.
    let text = assert_ok(&f.bf_in(&f.wt_path("a"), &["run", "a", "--", "true"]));
    assert!(text.contains("Started job 3"), "{text}");
    for id in ["1", "2", "3"] {
        f.wait_for_state(id, "succeeded");
    }

    let from_b = assert_ok(&f.bf_in(&f.wt_path("b"), &["jobs"]));
    let ids: Vec<_> = from_b
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>())
        .collect();
    assert_eq!(ids, [vec!["1", "a"], vec!["2", "b"], vec!["3", "a"]]);

    assert_eq!(assert_ok(&f.bf(&["logs", "1", "--stdout"])), "from-a\n");
    let two = assert_ok(&f.bf(&["logs", "2"]));
    assert!(
        two.contains("==> stdout <==\nfrom-b\n") && two.contains("==> stderr <==\nb-err\n"),
        "{two}"
    );
    assert!(!two.contains("from-a"));
}

#[test]
fn job_state_is_refreshed_from_disk_across_cli_invocations() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let gate = f.root.join("gate");
    let id = f.run_job(
        "feat",
        &["sh", "-c", GATE_SCRIPT, "sh", gate.to_str().unwrap()],
    );

    // Deterministically running: the job cannot finish until we create the gate.
    f.wait_for_state(&id, "running");
    assert_eq!(f.job_meta(&id)["phase"]["state"], "running");
    assert!(
        wait_until(PATIENCE, || {
            assert_ok(&f.bf(&["logs", &id, "--stdout"])).contains("started")
        }),
        "output so far is readable while the job runs"
    );

    fs::write(&gate, "").unwrap();
    f.wait_for_state(&id, "succeeded");
    assert_eq!(f.job_meta(&id)["phase"]["state"], "finished");
    assert_eq!(f.job_meta(&id)["phase"]["outcome"]["result"], "succeeded");
    assert_eq!(
        assert_ok(&f.bf(&["logs", &id, "--stdout"])),
        "started\ndone\n"
    );
}

#[test]
fn repeated_and_concurrent_commands_are_safe() {
    let f = Fixture::new();
    new_branch(&f, "feat");

    // Many simultaneous `run`s must get distinct ids and never clobber each other.
    let ids: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|n| {
                let f = &f;
                scope.spawn(move || f.run_job("feat", &["sh", "-c", &format!("echo job-{n}")]))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut sorted: Vec<u32> = ids.iter().map(|i| i.parse().unwrap()).collect();
    sorted.sort_unstable();
    assert_eq!(sorted, (1..=8).collect::<Vec<u32>>(), "ids: {ids:?}");
    for id in &ids {
        f.wait_for_state(id, "succeeded");
    }
    // Each job's log holds exactly its own output.
    let mut outputs: Vec<String> = ids
        .iter()
        .map(|id| assert_ok(&f.bf(&["logs", id, "--stdout"])))
        .collect();
    outputs.sort();
    let expected: Vec<String> = (0..8).map(|n| format!("job-{n}\n")).collect();
    assert_eq!(outputs, expected);

    // Readers hammering the registry while a job runs and finishes.
    let gate = f.root.join("gate");
    let id = f.run_job(
        "feat",
        &["sh", "-c", GATE_SCRIPT, "sh", gate.to_str().unwrap()],
    );
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..5 {
                    assert_ok(&f.bf(&["jobs"]));
                    assert_ok(&f.bf(&["logs", &id]));
                }
            });
        }
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            fs::write(&gate, "").unwrap();
        });
    });
    f.wait_for_state(&id, "succeeded");
}

#[test]
fn existing_job_directories_are_never_overwritten() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    f.run_job("feat", &["true"]);
    // Someone (or a crash) left content where job 2 would go.
    fs::create_dir(f.job_dir("2")).unwrap();
    fs::write(f.job_dir("2").join("stdout.log"), "precious").unwrap();

    assert_eq!(f.run_job("feat", &["true"]), "3");
    assert_eq!(
        fs::read_to_string(f.job_dir("2").join("stdout.log")).unwrap(),
        "precious"
    );
    assert_eq!(f.job_state("2").as_deref(), Some("corrupt"));
}

// ------------------------------------------------------------------ stopping

#[test]
fn stop_terminates_a_long_running_job_and_its_process() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let id = f.run_job("feat", &["sleep", "60"]);
    f.wait_for_state(&id, "running");

    let pid = f.job_meta(&id)["phase"]["pid"].as_u64().unwrap() as u32;
    assert!(process_alive(pid));

    let text = assert_ok(&f.bf(&["stop", &id]));
    assert!(
        text.contains("Stopped job 1") && text.contains("SIGTERM"),
        "{text}"
    );
    assert_eq!(f.job_state(&id).as_deref(), Some("stopped"));
    assert_eq!(f.job_meta(&id)["phase"]["outcome"]["result"], "stopped");
    assert!(
        wait_until(PATIENCE, || !process_alive(pid)),
        "the program must really be gone"
    );

    // Stopping again is a clear error, not a second kill.
    let err = assert_fails(&f.bf(&["stop", &id]));
    assert!(
        err.contains("not running") && err.contains("stopped"),
        "{err}"
    );
}

#[test]
fn stop_escalates_to_sigkill_when_the_program_ignores_sigterm() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let ready = f.root.join("ready");
    // Ignore TERM (inherited by `sleep`), signal readiness, then block.
    let script = "trap '' TERM; echo ok > \"$1\"; sleep 60";
    let id = f.run_job("feat", &["sh", "-c", script, "sh", ready.to_str().unwrap()]);
    assert!(wait_until(PATIENCE, || ready.exists()));

    let text = assert_ok(&f.bf(&["stop", &id, "--grace", "1"]));
    assert!(
        text.contains("SIGKILL"),
        "SIGTERM was ignored, so SIGKILL must have been used: {text}"
    );
    assert_eq!(f.job_state(&id).as_deref(), Some("stopped"));
    let jobs = assert_ok(&f.bf(&["jobs"]));
    assert!(jobs.contains("signal 9"), "{jobs}");
}

#[test]
fn a_second_stop_with_a_shorter_grace_escalates_a_slow_stop() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let ready = f.root.join("ready");
    let script = "trap '' TERM; echo ok > \"$1\"; sleep 60";
    let id = f.run_job("feat", &["sh", "-c", script, "sh", ready.to_str().unwrap()]);
    assert!(wait_until(PATIENCE, || ready.exists()));

    std::thread::scope(|scope| {
        // First stop: SIGTERM is ignored and SIGKILL is a whole minute away.
        let slow = scope.spawn(|| f.bf(&["stop", &id, "--grace", "60"]));
        assert!(wait_until(PATIENCE, || f
            .job_dir(&id)
            .join("stop-requested")
            .exists()));
        assert_eq!(
            f.job_state(&id).as_deref(),
            Some("running"),
            "still alive, waiting out the grace"
        );

        // Second stop shortens the grace period to zero: kill now.
        let fast = assert_ok(&f.bf(&["stop", &id, "--grace", "0"]));
        assert!(fast.contains("SIGKILL"), "{fast}");
        let slow = slow.join().unwrap();
        assert!(
            String::from_utf8_lossy(&slow.stdout).contains("Stopped job"),
            "{}",
            stderr(&slow)
        );
    });
    assert_eq!(f.job_state(&id).as_deref(), Some("stopped"));
}

#[test]
fn stop_kills_the_whole_process_group() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let pidfile = f.root.join("grandchild.pid");
    // A background grandchild that outlives its parent unless the group is signalled.
    let script = "sleep 60 & echo $! > \"$1\"; wait";
    let id = f.run_job(
        "feat",
        &["sh", "-c", script, "sh", pidfile.to_str().unwrap()],
    );
    assert!(wait_until(PATIENCE, || {
        fs::read_to_string(&pidfile)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }));
    let grandchild: u32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process_alive(grandchild));

    assert_ok(&f.bf(&["stop", &id]));
    assert!(
        wait_until(PATIENCE, || !process_alive(grandchild)),
        "grandchild must die with the group"
    );
}

#[test]
fn stop_rejects_unknown_finished_and_invalid_targets() {
    let f = Fixture::new();
    let err = assert_fails(&f.bf(&["stop", "1"]));
    assert!(err.contains("no job with id `1`"), "{err}");

    new_branch(&f, "feat");
    f.run_job("feat", &["true"]);
    f.wait_for_state("1", "succeeded");
    let err = assert_fails(&f.bf(&["stop", "1"]));
    assert!(
        err.contains("not running") && err.contains("succeeded"),
        "{err}"
    );
    assert!(
        !f.job_dir("1").join("stop-requested").exists(),
        "no request for a finished job"
    );
}

// ------------------------------------------------------ ids & invalid targets

#[test]
fn unknown_and_malformed_job_ids_are_rejected_without_touching_the_filesystem() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    f.run_job("feat", &["true"]);
    f.wait_for_state("1", "succeeded");
    let before: Vec<_> = fs::read_dir(f.jobs_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    for unknown in ["2", "99", "123456789"] {
        for cmd in ["logs", "stop"] {
            let err = assert_fails(&f.bf(&[cmd, unknown]));
            assert!(err.contains("no job with id"), "{cmd} {unknown}: {err}");
        }
    }
    for malformed in [
        "../1",
        "1/../1",
        "../../etc/passwd",
        "01",
        "abc",
        "1.0",
        "",
        " 1",
        "1234567890",
        "-1",
    ] {
        for cmd in ["logs", "stop"] {
            let out = f.bf(&[cmd, "--", malformed]);
            let err = assert_fails(&out);
            assert!(err.contains("invalid job id"), "{cmd} {malformed:?}: {err}");
        }
    }
    let after: Vec<_> = fs::read_dir(f.jobs_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
    assert!(!f.repo.join(".git/branchforge/etc").exists());
}

#[test]
fn run_refuses_unsafe_or_unknown_targets_and_creates_no_job() {
    let f = Fixture::new();
    new_branch(&f, "feat");

    // Unknown / invalid / main.
    assert!(assert_fails(&f.bf(&["run", "nope", "--", "true"])).contains("no worktree has branch"));
    for bad in ["../x", "a..b", "a/../b", "/abs", "x\ty"] {
        let err = assert_fails(&f.bf(&["run", bad, "--", "true"]));
        assert!(
            err.contains("invalid branch name") || err.contains("no worktree"),
            "{bad:?}: {err}"
        );
    }
    assert!(assert_fails(&f.bf(&["run", "main", "--", "true"])).contains("main worktree"));

    // A worktree BranchForge did not create.
    let ext = f.root.join("hand-made");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "manual",
            ext.to_str().unwrap(),
        ],
    );
    assert!(
        assert_fails(&f.bf(&["run", "manual", "--", "true"]))
            .contains("not managed by BranchForge")
    );

    // Inside the managed directory but with the wrong name.
    let odd = f.wt_path("odd-name");
    git(
        &f.repo,
        &["worktree", "add", "-q", "-b", "real", odd.to_str().unwrap()],
    );
    assert!(
        assert_fails(&f.bf(&["run", "real", "--", "true"])).contains("not managed by BranchForge")
    );

    // Worktree directory replaced by a symlink.
    new_branch(&f, "linked");
    let real = f.root.join("real-dir");
    fs::rename(f.wt_path("linked"), &real).unwrap();
    std::os::unix::fs::symlink(&real, f.wt_path("linked")).unwrap();
    assert!(
        assert_fails(&f.bf(&["run", "linked", "--", "touch", "pwned"]))
            .contains("not managed by BranchForge")
    );
    assert!(!real.join("pwned").exists());

    // Missing directory.
    new_branch(&f, "gone");
    fs::remove_dir_all(f.wt_path("gone")).unwrap();
    assert!(assert_fails(&f.bf(&["run", "gone", "--", "true"])).contains("missing on disk"));

    // Empty program.
    assert!(assert_fails(&f.bf(&["run", "feat", "--", ""])).contains("must not be empty"));
    // Missing `--`/program is a usage error from clap.
    assert!(!f.bf(&["run", "feat"]).status.success());

    assert!(
        !f.jobs_dir().exists(),
        "no rejected request may create a job"
    );
}

#[test]
fn run_refuses_a_symlinked_managed_directory() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let moved = f.root.join("moved");
    fs::rename(f.managed_root(), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, f.managed_root()).unwrap();

    let err = assert_fails(&f.bf(&["run", "feat", "--", "touch", "pwned"]));
    assert!(err.contains("not a plain directory"), "{err}");
    assert!(!moved.join("feat/pwned").exists());
    assert!(!f.jobs_dir().exists());
}

#[test]
fn symlinked_job_data_directories_are_refused_and_never_written_through() {
    for link_jobs_only in [false, true] {
        let f = Fixture::new();
        new_branch(&f, "feat");
        let elsewhere = f.root.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let branchforge = f.repo.join(".git/branchforge");
        if link_jobs_only {
            fs::create_dir(&branchforge).unwrap();
            std::os::unix::fs::symlink(&elsewhere, branchforge.join("jobs")).unwrap();
        } else {
            std::os::unix::fs::symlink(&elsewhere, &branchforge).unwrap();
        }

        for args in [
            &["run", "feat", "--", "true"][..],
            &["jobs"],
            &["logs", "1"],
            &["stop", "1"],
        ] {
            let err = assert_fails(&f.bf(args));
            assert!(err.contains("not a plain directory"), "{args:?}: {err}");
        }
        // Removal fails closed: it cannot prove no job is running.
        let err = assert_fails(&f.bf(&["remove", "feat"]));
        assert!(err.contains("not a plain directory"), "{err}");
        assert!(f.wt_path("feat").is_dir());
        assert_eq!(
            fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "nothing written through the symlink"
        );
    }
}

// ------------------------------------------------------ corrupted metadata

#[test]
fn corrupted_metadata_is_reported_and_never_breaks_other_jobs() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    f.run_job("feat", &["echo", "hello"]);
    f.run_job("feat", &["echo", "other"]);
    f.wait_for_state("1", "succeeded");
    f.wait_for_state("2", "succeeded");
    let meta = f.job_dir("1").join("meta.json");
    let good = fs::read_to_string(&meta).unwrap();

    let variants: Vec<(&str, Option<String>)> = vec![
        ("garbage", Some("{{{ definitely not json".into())),
        ("empty", Some(String::new())),
        ("truncated", Some(good[..good.len() / 2].to_string())),
        (
            "wrong version",
            Some(good.replace("\"version\": 1", "\"version\": 99")),
        ),
        (
            "another job's id",
            Some(good.replace("\"id\": \"1\"", "\"id\": \"2\"")),
        ),
        ("deleted", None),
    ];
    for (name, content) in variants {
        match content {
            Some(text) => fs::write(&meta, text).unwrap(),
            None => fs::remove_file(&meta).unwrap(),
        }
        let jobs = assert_ok(&f.bf(&["jobs"])); // never fatal
        let row1 = jobs.lines().find(|l| l.starts_with('1')).unwrap();
        assert!(row1.contains("corrupt"), "{name}: {row1}");
        let row2 = jobs.lines().find(|l| l.starts_with('2')).unwrap();
        assert!(
            row2.contains("succeeded"),
            "{name}: other jobs are unaffected: {row2}"
        );

        // Logs stay readable; stop refuses with the reason.
        assert_eq!(
            assert_ok(&f.bf(&["logs", "1", "--stdout"])),
            "hello\n",
            "{name}"
        );
        let err = assert_fails(&f.bf(&["stop", "1"]));
        assert!(err.contains("unreadable metadata"), "{name}: {err}");
        // A dead job with bad metadata does not block removal of anything.
        assert_ok(&f.bf(&["status"]));
    }
}

#[test]
fn corrupt_metadata_with_a_live_supervisor_blocks_every_removal() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    new_branch(&f, "other");
    let gate = f.root.join("gate");
    let id = f.run_job(
        "feat",
        &["sh", "-c", GATE_SCRIPT, "sh", gate.to_str().unwrap()],
    );
    f.wait_for_state(&id, "running");

    // We can no longer tell which worktree this job uses, but it is alive.
    fs::write(f.job_dir(&id).join("meta.json"), "garbage").unwrap();
    assert_eq!(f.job_state(&id).as_deref(), Some("corrupt"));
    let err = assert_fails(&f.bf(&["remove", "other", "--force"]));
    assert!(
        err.contains("active job") && err.contains(&format!("[{id}]")),
        "{err}"
    );
    assert!(f.wt_path("other").is_dir());

    // The supervisor rewrites valid metadata when the job ends; then it unblocks.
    fs::write(&gate, "").unwrap();
    f.wait_for_state(&id, "succeeded");
    assert_ok(&f.bf(&["remove", "other"]));
}

// -------------------------------------------------- removal and stale entries

#[test]
fn remove_is_blocked_while_a_job_runs_even_with_force() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    new_branch(&f, "other");
    let gate = f.root.join("gate");
    let id = f.run_job(
        "feat",
        &["sh", "-c", GATE_SCRIPT, "sh", gate.to_str().unwrap()],
    );
    f.wait_for_state(&id, "running");

    for args in [&["remove", "feat"][..], &["remove", "feat", "--force"]] {
        let err = assert_fails(&f.bf(args));
        assert!(
            err.contains("active job(s) [1]") && err.contains("branchforge stop"),
            "{err}"
        );
        assert!(err.contains("--force does not override"), "{err}");
    }
    assert!(f.wt_path("feat").is_dir(), "the worktree must be untouched");
    assert_eq!(f.job_state(&id).as_deref(), Some("running"));

    // Other branches are unaffected.
    assert_ok(&f.bf(&["remove", "other"]));

    // Once the job ends (here: on its own), removal proceeds as usual.
    fs::write(&gate, "").unwrap();
    f.wait_for_state(&id, "succeeded");
    assert_ok(&f.bf(&["remove", "feat"]));
    assert!(!f.wt_path("feat").exists());
    // The job's logs outlive the worktree they ran in.
    assert_eq!(
        assert_ok(&f.bf(&["logs", &id, "--stdout"])),
        "started\ndone\n"
    );
}

#[test]
fn remove_succeeds_after_stopping_the_job() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let id = f.run_job("feat", &["sleep", "60"]);
    f.wait_for_state(&id, "running");
    assert_fails(&f.bf(&["remove", "feat"]));
    assert_ok(&f.bf(&["stop", &id]));
    assert_ok(&f.bf(&["remove", "feat"]));
    assert!(!f.wt_path("feat").exists());
}

#[test]
fn existing_remove_protections_still_apply_alongside_jobs() {
    let f = Fixture::new();
    fs::write(f.repo.join(".gitignore"), ".env\n").unwrap();
    git(&f.repo, &["add", ".gitignore"]);
    git(&f.repo, &["commit", "-q", "-m", "ignore"]);
    new_branch(&f, "feat");
    f.run_job("feat", &["true"]);
    f.wait_for_state("1", "succeeded");

    fs::write(f.wt_path("feat").join(".env"), "secret").unwrap();
    let err = assert_fails(&f.bf(&["remove", "feat"]));
    assert!(
        err.contains("git-ignored"),
        "ignored-file protection intact: {err}"
    );
    assert_ok(&f.bf(&["remove", "feat", "--force"]));
}

#[test]
fn lost_jobs_are_never_reported_running_and_never_cause_foreign_signals() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let mut victim = Victim::spawn();

    // Metadata claims job 1 is running with the victim's PID, and job 2 even
    // names *this test process*. No supervisor holds either job's lock.
    craft_job(
        &f,
        "1",
        "feat",
        &f.wt_path("feat"),
        &["sleep", "60"],
        running_phase(victim.pid()),
        1,
    );
    craft_job(
        &f,
        "2",
        "feat",
        &f.wt_path("feat"),
        &["sleep", "60"],
        running_phase(std::process::id()),
        1,
    );

    let jobs = assert_ok(&f.bf(&["jobs"]));
    for id in ["1", "2"] {
        let row = jobs.lines().find(|l| l.starts_with(id)).unwrap();
        assert!(
            row.contains("lost") && !row.contains("running"),
            "a live PID is not proof of a running job: {row}"
        );
    }

    for id in ["1", "2"] {
        let err = assert_fails(&f.bf(&["stop", id]));
        assert!(err.contains("not running") && err.contains("lost"), "{err}");
        assert!(err.contains("cannot verify or signal"), "{err}");
        assert!(!f.job_dir(id).join("stop-requested").exists());
    }
    assert!(
        victim.still_running(),
        "BranchForge must not signal a PID taken from metadata"
    );

    // Lost jobs don't block removal, but the user is warned.
    let out = assert_ok(&f.bf(&["remove", "feat"]));
    assert!(
        out.contains("warning") && out.contains("lost") && out.contains("1, 2"),
        "{out}"
    );
    assert!(!f.wt_path("feat").exists());
    assert!(victim.still_running());

    fs::remove_dir_all(f.jobs_dir()).unwrap(); // crafted fixtures: tidy up
}

#[test]
fn a_fresh_starting_job_is_starting_but_an_old_one_is_lost() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    craft_job(
        &f,
        "1",
        "feat",
        &f.wt_path("feat"),
        &["true"],
        json!({"state": "starting"}),
        now_ms(),
    );
    craft_job(
        &f,
        "2",
        "feat",
        &f.wt_path("feat"),
        &["true"],
        json!({"state": "starting"}),
        1,
    );
    assert_eq!(f.job_state("1").as_deref(), Some("starting"));
    assert_eq!(f.job_state("2").as_deref(), Some("lost"));
    // A starting job protects its worktree; a lost one does not.
    let err = assert_fails(&f.bf(&["remove", "feat"]));
    assert!(err.contains("active job(s) [1]"), "{err}");
    fs::remove_dir_all(f.jobs_dir()).unwrap();
}

// ---------------------------------------------- supervisor trust boundaries

#[test]
fn the_supervisor_revalidates_the_recorded_worktree_before_running_anything() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let outside = f.root.join("outside");
    fs::create_dir(&outside).unwrap();
    let marker = f.root.join("marker");

    // Tampered metadata: right branch, but a worktree path outside the managed layout.
    craft_job(
        &f,
        "1",
        "feat",
        &outside,
        &["touch", marker.to_str().unwrap()],
        json!({"state": "starting"}),
        now_ms(),
    );
    assert_ok(&f.bf(&["__supervise", "1"]));
    assert!(
        !marker.exists(),
        "the program must not run in an unmanaged directory"
    );
    f.wait_for_state("1", "start-failed");

    // Branch with no worktree at all.
    craft_job(
        &f,
        "2",
        "ghost",
        &f.wt_path("ghost"),
        &["touch", marker.to_str().unwrap()],
        json!({"state": "starting"}),
        now_ms(),
    );
    assert_ok(&f.bf(&["__supervise", "2"]));
    assert!(!marker.exists());
    f.wait_for_state("2", "start-failed");

    // A job that already started or finished is never run a second time.
    let counter = f.root.join("counter");
    let id = f.run_job(
        "feat",
        &[
            "sh",
            "-c",
            "echo x >> \"$1\"",
            "sh",
            counter.to_str().unwrap(),
        ],
    );
    f.wait_for_state(&id, "succeeded");
    let err = assert_fails(&f.bf(&["__supervise", &id]));
    assert!(err.contains("already started or finished"), "{err}");
    assert_eq!(fs::read_to_string(&counter).unwrap(), "x\n");
}

#[test]
fn deleting_a_jobs_directory_makes_its_supervisor_kill_the_program() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let pidfile = f.root.join("pid");
    let script = "echo $$ > \"$1\"; sleep 60";
    let id = f.run_job(
        "feat",
        &["sh", "-c", script, "sh", pidfile.to_str().unwrap()],
    );
    assert!(wait_until(PATIENCE, || {
        fs::read_to_string(&pidfile)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }));
    let pid: u32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process_alive(pid));

    // Nothing can observe or stop this job any more; it must not linger.
    fs::remove_dir_all(f.job_dir(&id)).unwrap();
    assert!(wait_until(PATIENCE, || !process_alive(pid)));
}

// ------------------------------------------- run/remove race (registry lock)

/// Take the repository-wide registry lock from the test, exactly as `run` and
/// `remove` do, so the test can decide what happens "in between".
fn hold_registry_lock(f: &Fixture) -> fs::File {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.repo.join(".git/branchforge.lock"))
        .unwrap();
    // SAFETY: valid descriptor owned by `file`.
    assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
    file
}

fn is_blocked(child: &mut Child) -> bool {
    // True if the process is still running after a short observation window.
    !wait_until(Duration::from_millis(300), || {
        child.try_wait().unwrap().is_some()
    })
}

#[test]
fn remove_checks_for_jobs_only_after_taking_the_registry_lock() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let lock = hold_registry_lock(&f);

    // `remove` starts while "a run" is mid-registration. It must wait for the
    // lock instead of checking (and deleting) first.
    let mut remove = f.spawn_bf(&["remove", "feat", "--force"]);
    assert!(
        is_blocked(&mut remove),
        "remove must wait for the registry lock"
    );
    assert!(
        f.wt_path("feat").is_dir(),
        "nothing may be deleted while it waits"
    );

    // The "run" publishes its job, then releases the lock.
    craft_job(
        &f,
        "1",
        "feat",
        &f.wt_path("feat"),
        &["true"],
        json!({"state": "starting"}),
        now_ms(),
    );
    drop(lock);

    let out = remove.wait_with_output().unwrap();
    let err = assert_fails(&out);
    assert!(
        err.contains("active job(s) [1]"),
        "remove saw the job registered while it waited: {err}"
    );
    assert!(f.wt_path("feat").is_dir(), "the worktree must survive");
    fs::remove_dir_all(f.jobs_dir()).unwrap();
}

#[test]
fn run_rereads_the_worktree_list_after_taking_the_registry_lock() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let marker = f.root.join("marker");
    let lock = hold_registry_lock(&f);

    // `run` starts and validates against the world as it was...
    let mut run = f.spawn_bf(&["run", "feat", "--", "touch", marker.to_str().unwrap()]);
    assert!(is_blocked(&mut run), "run must wait for the registry lock");

    // ...but a `remove` finishes first (holding the lock, deleting the worktree).
    git(
        &f.repo,
        &["worktree", "remove", f.wt_path("feat").to_str().unwrap()],
    );
    drop(lock);

    let out = run.wait_with_output().unwrap();
    let err = assert_fails(&out);
    assert!(
        err.contains("no worktree has branch"),
        "run must notice the worktree is gone: {err}"
    );
    assert!(
        !f.jobs_dir().exists(),
        "no job may be registered for a deleted worktree"
    );
    assert!(!marker.exists(), "no program may have run");
}

#[test]
fn run_and_remove_hammering_the_same_branch_never_leave_a_job_in_a_deleted_worktree() {
    // Statistical complement to the deterministic tests above: whatever the
    // interleaving, a job is never active while its worktree is gone.
    for round in 0..6 {
        let f = Fixture::new();
        new_branch(&f, "feat");
        let (run_out, remove_out) = std::thread::scope(|scope| {
            let run = scope.spawn(|| f.bf(&["run", "feat", "--", "sleep", "30"]));
            let remove = scope.spawn(|| f.bf(&["remove", "feat"]));
            (run.join().unwrap(), remove.join().unwrap())
        });
        let worktree_exists = f.wt_path("feat").is_dir();
        let job_active = f
            .job_state("1")
            .is_some_and(|state| state == "running" || state == "starting");
        assert!(
            !job_active || worktree_exists,
            "round {round}: an active job lost its worktree\nrun: {:?}\nremove: {:?}",
            String::from_utf8_lossy(&run_out.stderr),
            String::from_utf8_lossy(&remove_out.stderr),
        );
        // Exactly one of them won.
        assert!(
            run_out.status.success() != remove_out.status.success(),
            "round {round}: run={} remove={}",
            run_out.status,
            remove_out.status
        );
        if job_active {
            assert_ok(&f.bf(&["stop", "1", "--grace", "0"]));
        }
    }
}

// --------------------------------------------- lifecycle: stop vs. natural exit

#[test]
fn a_program_that_exits_by_itself_after_stop_was_delivered_is_recorded_as_stopped() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let gate = f.root.join("gate");
    // Notes the SIGTERM but carries on until the gate appears, then exits 0 on its own.
    let script = "trap 'echo got-term' TERM; while [ ! -e \"$1\" ]; do sleep 0.05; done; echo exiting-by-itself";
    let id = f.run_job("feat", &["sh", "-c", script, "sh", gate.to_str().unwrap()]);
    f.wait_for_state(&id, "running");

    std::thread::scope(|scope| {
        let stopper = scope.spawn(|| f.bf(&["stop", &id, "--grace", "60"]));
        // Wait until the supervisor has really delivered SIGTERM.
        assert!(wait_until(PATIENCE, || {
            assert_ok(&f.bf(&["logs", &id, "--stdout"])).contains("got-term")
        }));
        assert_eq!(f.job_state(&id).as_deref(), Some("running"));
        fs::write(&gate, "").unwrap(); // the program now exits normally, code 0

        let out = stopper.join().unwrap();
        let text = assert_ok(&out);
        assert!(
            text.contains("Stopped job") && text.contains("exited with code 0"),
            "{text}"
        );
    });
    // A stop was requested and delivered first, so this is `stopped`, not `succeeded`.
    assert_eq!(f.job_state(&id).as_deref(), Some("stopped"));
    assert_eq!(f.job_meta(&id)["phase"]["outcome"]["exit_code"], 0);
}

#[test]
fn stop_on_a_job_that_finished_before_the_request_is_acted_on_reports_it_honestly() {
    let f = Fixture::new();
    new_branch(&f, "feat");
    let id = f.run_job("feat", &["true"]);
    f.wait_for_state(&id, "succeeded");
    // A stop request after the fact must not rewrite history.
    let err = assert_fails(&f.bf(&["stop", &id]));
    assert!(err.contains("succeeded"), "{err}");
    assert_eq!(f.job_state(&id).as_deref(), Some("succeeded"));
}

// ----------------------------------------------- registry robustness (listing)

#[test]
fn one_unreadable_job_does_not_break_listing_but_still_blocks_removal() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    new_branch(&f, "feat");
    new_branch(&f, "other");
    f.run_job("feat", &["true"]);
    f.run_job("feat", &["true"]);
    f.wait_for_state("1", "succeeded");
    f.wait_for_state("2", "succeeded");

    let lock = f.job_dir("1").join("lock");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o000)).unwrap();
    let restore = || fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    if fs::File::open(&lock).is_ok() {
        restore();
        eprintln!("skipping: file permissions are not enforced for this user (root?)");
        return;
    }

    // Display is lenient: the bad job is a visible `corrupt` row, the rest still list.
    let jobs = assert_ok(&f.bf(&["jobs"]));
    let row1 = jobs.lines().find(|l| l.starts_with('1')).unwrap();
    assert!(
        row1.contains("corrupt") && row1.contains("cannot read"),
        "{row1}"
    );
    assert!(
        jobs.lines()
            .any(|l| l.starts_with('2') && l.contains("succeeded")),
        "{jobs}"
    );

    // Policy is strict: removal cannot prove the job is not running, so it refuses.
    let err = assert_fails(&f.bf(&["remove", "other"]));
    assert!(err.contains("I/O error"), "{err}");
    assert!(f.wt_path("other").is_dir());

    restore();
    assert_ok(&f.bf(&["remove", "other"]));
}
