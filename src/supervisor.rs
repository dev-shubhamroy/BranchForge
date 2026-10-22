//! The per-job supervisor.
//!
//! `branchforge run` does not run the program itself. It creates the job record
//! and starts a detached copy of this binary (`branchforge __supervise <id>`),
//! which:
//!
//! 1. takes the job's exclusive lock (this is what makes the job count as alive),
//! 2. re-validates that the recorded worktree is still a managed one,
//! 3. spawns the program in its own process group with stdout/stderr appended
//!    to the job's log files (the kernel writes the files; nothing is buffered
//!    in memory and nothing can deadlock on a full pipe),
//! 4. polls for exit and for a stop request, and
//! 5. records the result and exits.
//!
//! The supervisor owns the child it spawned. It only ever signals the process
//! group of that still-unreaped child, so it can never hit a recycled PID, and
//! the CLI never signals anything: `stop` just drops a request file.

use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::jobs::{JobMeta, JobPaths, JobRecord, JobStore, Outcome, Phase, now_ms, write_meta};
use crate::process::{self, Signal};
use crate::repo::Repo;

pub const DEFAULT_STOP_GRACE_SECS: u64 = 5;
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Start a detached supervisor for `record` (used by `run`).
pub fn spawn(repo: &Repo, record: &JobRecord) -> Result<Child> {
    let exe = std::env::current_exe().map_err(|source| Error::Io {
        path: "current executable".into(),
        source,
    })?;
    let log = OpenOptions::new()
        .append(true)
        .open(&record.paths.supervisor_log)
        .map_err(|source| Error::Io {
            path: record.paths.supervisor_log.clone(),
            source,
        })?;
    let mut cmd = Command::new(&exe);
    cmd.arg("__supervise")
        .arg(&record.id)
        .current_dir(&repo.main().path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    process::own_process_group(&mut cmd);
    cmd.spawn()
        .map_err(|source| Error::Io { path: exe, source })
}

/// Entry point of `branchforge __supervise <id>`.
pub fn run(repo: &Repo, id: &str) -> Result<()> {
    let store = JobStore::open_existing(repo)?.ok_or_else(|| Error::UnknownJob(id.to_string()))?;
    let paths = store.load(id)?.paths;

    // Held until this function returns. Blocks if another supervisor for the
    // same job is somehow running, then the phase check below refuses to rerun.
    let _lock = process::SupervisorLock::acquire(&paths.lock)?;

    let mut meta = store.load(id)?.meta()?.clone();
    if !matches!(meta.phase, Phase::Starting) {
        return Err(Error::JobAlreadyStarted { id: id.to_string() });
    }

    // The metadata is only a record. Before running anything, make sure the
    // worktree it names is still exactly the managed worktree for its branch.
    let worktree = match repo.require_managed(&meta.branch) {
        Ok(wt) if wt.path == meta.worktree && wt.path.is_dir() => wt.path.clone(),
        Ok(_) => {
            return finish_start_failed(
                &paths,
                &mut meta,
                "the recorded worktree is not the managed worktree for this branch".into(),
            );
        }
        Err(e) => return finish_start_failed(&paths, &mut meta, e.to_string()),
    };

    let mut cmd = Command::new(&meta.program);
    cmd.args(&meta.args)
        .current_dir(&worktree)
        .stdin(Stdio::null());
    for (target, is_stdout) in [(&paths.stdout, true), (&paths.stderr, false)] {
        // Append, never truncate: existing log content is never overwritten.
        match OpenOptions::new().append(true).open(target) {
            Ok(file) if is_stdout => cmd.stdout(file),
            Ok(file) => cmd.stderr(file),
            Err(e) => {
                return finish_start_failed(
                    &paths,
                    &mut meta,
                    format!("cannot open log file {}: {e}", target.display()),
                );
            }
        };
    }
    process::own_process_group(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return finish_start_failed(&paths, &mut meta, e.to_string()),
    };
    drop(cmd); // release the supervisor's copies of the log handles

    let pid = child.id();
    let started_at_ms = now_ms();
    meta.phase = Phase::Running {
        pid,
        supervisor_pid: std::process::id(),
        started_at_ms,
    };
    if let Err(e) = write_meta(&paths, &meta) {
        // Never leave a program running that nothing has recorded. Then, best
        // effort, record why the job never ran (this write may fail too, e.g. if
        // the disk is full; `run` then falls back to `record_early_exit`).
        let _ = process::signal_group(pid, Signal::Kill);
        let _ = child.wait();
        let _ = finish_start_failed(
            &paths,
            &mut meta,
            format!("the program was started but could not be recorded, so it was killed: {e}"),
        );
        return Err(e);
    }

    let (status, stop_requested) = supervise(&mut child, &paths)?;

    let signal = process::exit_signal(&status);
    let exit_code = status.code();
    let outcome = if stop_requested {
        Outcome::Stopped { exit_code, signal }
    } else if status.success() {
        Outcome::Succeeded
    } else {
        Outcome::Failed { exit_code, signal }
    };
    meta.phase = Phase::Finished {
        outcome,
        pid: Some(pid),
        started_at_ms: Some(started_at_ms),
        finished_at_ms: now_ms(),
    };
    write_meta(&paths, &meta)
}

/// Wait for `child`, honouring stop requests. Returns its exit status and
/// whether a stop was requested.
fn supervise(child: &mut Child, paths: &JobPaths) -> Result<(std::process::ExitStatus, bool)> {
    let pid = child.id();
    // (when the first request was seen, current grace period, SIGKILL sent)
    let mut stop: Option<(Instant, Duration, bool)> = None;

    loop {
        let waited = child.try_wait().map_err(|source| Error::Io {
            path: paths.dir.clone(),
            source,
        })?;
        if let Some(status) = waited {
            return Ok((status, stop.is_some()));
        }

        // If the registry directory vanished (repo deleted, job dir removed),
        // nothing can observe or stop this job any more: don't leave it running.
        if !paths.dir.is_dir() {
            let _ = process::signal_group(pid, Signal::Kill);
            let status = child.wait().map_err(|source| Error::Io {
                path: paths.dir.clone(),
                source,
            })?;
            return Ok((status, true));
        }

        match (&mut stop, read_stop_request(&paths.stop_request)) {
            (None, Some(grace)) => {
                // Ask nicely first; ignore errors (the group may already be gone).
                let _ = process::signal_group(pid, Signal::Term);
                stop = Some((Instant::now(), grace, false));
            }
            // A later request may shorten the grace period (escalation).
            (Some((_, current, _)), Some(grace)) => *current = grace,
            _ => {}
        }
        if let Some((since, grace, killed)) = &mut stop
            && !*killed
            && since.elapsed() >= *grace
        {
            let _ = process::signal_group(pid, Signal::Kill);
            *killed = true;
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

/// `Some(grace)` if a stop has been requested. The file holds the grace period
/// in whole seconds; a missing or unparsable value falls back to the default.
fn read_stop_request(path: &Path) -> Option<Duration> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => String::new(),
    };
    let secs = text.trim().parse().unwrap_or(DEFAULT_STOP_GRACE_SECS);
    Some(Duration::from_secs(secs))
}

/// Called by `run` after it saw the supervisor process exit.
///
/// If the supervisor left the job in `starting` (it failed before it could
/// record anything) and nothing else holds the job's lock, record `start-failed`
/// right away, instead of leaving a phantom job that would show as `starting`
/// and block `remove` until it decays to `lost` 15 seconds later.
///
/// Returns whether the job was updated. A supervisor that was *killed by a
/// signal* is deliberately not handled here: it might have died after spawning
/// the program, so the job's fate is genuinely unknown and it is left to become
/// `lost`. A job past `starting` is never touched.
pub fn record_early_exit(
    store: &JobStore,
    id: &str,
    exit: std::process::ExitStatus,
    details: &str,
) -> Result<bool> {
    if process::exit_signal(&exit).is_some() {
        return Ok(false);
    }
    let record = store.load(id)?;
    let Ok(meta) = &record.meta else {
        return Ok(false);
    };
    if record.alive || !matches!(meta.phase, Phase::Starting) {
        return Ok(false);
    }
    let code = exit
        .code()
        .map_or("unknown".to_string(), |code| code.to_string());
    let mut meta = meta.clone();
    finish_start_failed(
        &record.paths,
        &mut meta,
        format!("the supervisor exited (status {code}) before it started the program{details}"),
    )?;
    Ok(true)
}

fn finish_start_failed(paths: &JobPaths, meta: &mut JobMeta, message: String) -> Result<()> {
    meta.phase = Phase::Finished {
        outcome: Outcome::StartFailed { message },
        pid: None,
        started_at_ms: None,
        finished_at_ms: now_ms(),
    };
    write_meta(paths, meta)
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;
    use std::process::{Command, ExitStatus};

    use super::*;
    use crate::jobs::JobState;

    fn exited_with(code: i32) -> ExitStatus {
        Command::new("sh")
            .args(["-c", &format!("exit {code}")])
            .status()
            .unwrap()
    }

    /// An exit status for a process that was killed by a signal.
    fn killed_by_signal() -> ExitStatus {
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        child.kill().unwrap();
        child.wait().unwrap()
    }

    fn new_job(dir: &Path) -> (JobStore, String) {
        let store = JobStore::at(dir.to_path_buf());
        let record = store
            .create("feat", Path::new("/w/feat"), "true", &[])
            .unwrap();
        (store, record.id)
    }

    #[test]
    fn a_supervisor_that_exits_while_starting_marks_the_job_start_failed_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, id) = new_job(tmp.path());
        assert_eq!(store.load(&id).unwrap().state, JobState::Starting);

        let changed = record_early_exit(&store, &id, exited_with(1), ": boom").unwrap();
        assert!(changed);
        let after = store.load(&id).unwrap();
        assert_eq!(
            after.state,
            JobState::StartFailed,
            "no 15-second wait in `starting`"
        );
        let Phase::Finished {
            outcome: Outcome::StartFailed { message },
            ..
        } = &after.meta().unwrap().phase
        else {
            panic!("expected a recorded start failure");
        };
        assert!(
            message.contains("status 1") && message.contains(": boom"),
            "{message}"
        );
    }

    #[test]
    fn a_supervisor_killed_by_a_signal_is_not_assumed_to_have_failed_to_start() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, id) = new_job(tmp.path());
        // It may have died after spawning the program, so the fate is unknown:
        // leave the record alone and let it decay to `lost`.
        assert!(!record_early_exit(&store, &id, killed_by_signal(), "").unwrap());
        assert!(matches!(
            store.load(&id).unwrap().meta().unwrap().phase,
            Phase::Starting
        ));
    }

    #[test]
    fn jobs_past_starting_or_with_a_live_supervisor_are_never_touched() {
        let tmp = tempfile::tempdir().unwrap();

        // Live supervisor (lock held): not ours to overwrite.
        let (store, id) = new_job(tmp.path());
        let lock = process::SupervisorLock::acquire(&store.load(&id).unwrap().paths.lock).unwrap();
        assert!(!record_early_exit(&store, &id, exited_with(1), "").unwrap());
        drop(lock);

        // Already running / already finished.
        for phase in [
            Phase::Running {
                pid: 5,
                supervisor_pid: 4,
                started_at_ms: 1,
            },
            Phase::Finished {
                outcome: Outcome::Succeeded,
                pid: Some(5),
                started_at_ms: Some(1),
                finished_at_ms: 2,
            },
        ] {
            std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
            let (store, id) = new_job(&tmp.path().join("sub"));
            let record = store.load(&id).unwrap();
            let mut meta = record.meta().unwrap().clone();
            meta.phase = phase.clone();
            write_meta(&record.paths, &meta).unwrap();
            assert!(!record_early_exit(&store, &id, exited_with(1), "").unwrap());
            assert_eq!(store.load(&id).unwrap().meta().unwrap().phase, phase);
            std::fs::remove_dir_all(tmp.path().join("sub")).unwrap();
        }
    }

    #[test]
    fn corrupt_metadata_is_left_for_the_user_to_inspect() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, id) = new_job(tmp.path());
        std::fs::write(store.load(&id).unwrap().paths.meta, "garbage").unwrap();
        assert!(!record_early_exit(&store, &id, exited_with(1), "").unwrap());
        assert_eq!(store.load(&id).unwrap().state, JobState::Corrupt);
    }
}
