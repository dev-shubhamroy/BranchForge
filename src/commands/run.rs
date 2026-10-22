use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::jobs::{self, JobState, JobStore, Outcome, Phase, now_ms, write_meta};
use crate::repo::Repo;
use crate::supervisor;

/// How long `run` waits for the supervisor to report that the program started.
const START_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Launch `command` (program followed by its arguments) in the managed
/// worktree for `branch`, as a supervised background job.
pub fn run(repo: &Repo, branch: &str, command: &[String]) -> Result<()> {
    // Cheap validation first (traversal, flags, control characters, empty program).
    repo.layout.path_for(branch)?;
    let (program, args) = match command.split_first() {
        Some((program, args)) if !program.is_empty() => (program, args),
        _ => return Err(Error::EmptyCommand),
    };

    // Critical section, serialised with `remove` by the registry lock:
    //  * re-read the worktree list, because a `remove` that held the lock before
    //    us may have deleted the worktree since `repo` was discovered;
    //  * require a managed, non-symlinked, existing worktree (never the main one
    //    or one BranchForge didn't create);
    //  * publish the job record. From that moment the job counts as active, so a
    //    `remove` that gets the lock after us refuses.
    // The lock is released before the supervisor is spawned; the already
    // published `starting` record is what protects the worktree from then on.
    let (store, record) = {
        let _registry = jobs::registry_lock(repo)?;
        let fresh = Repo::discover(&repo.main().path)?;
        let wt = fresh.require_managed(branch)?;
        if wt.prunable.is_some() || !wt.path.is_dir() {
            return Err(Error::WorktreeMissing(wt.path.clone()));
        }
        let store = JobStore::open_or_create(&fresh)?;
        let record = store.create(branch, &wt.path, program, args)?;
        (store, record)
    };
    let id = record.id.clone();
    let mut supervisor_process = match supervisor::spawn(repo, &record) {
        Ok(child) => child,
        Err(e) => {
            // No supervisor exists to ever finish this job, and nothing else
            // writes its metadata, so record the failure here instead of
            // leaving a job that would sit in `starting` until it decays to `lost`.
            if let Ok(meta) = record.meta() {
                let mut meta = meta.clone();
                meta.phase = Phase::Finished {
                    outcome: Outcome::StartFailed {
                        message: format!("could not start the supervisor: {e}"),
                    },
                    pid: None,
                    started_at_ms: None,
                    finished_at_ms: now_ms(),
                };
                let _ = write_meta(&record.paths, &meta);
            }
            return Err(e);
        }
    };

    // Wait only until the supervisor reports the outcome of *starting* the
    // program, never for the program itself.
    let started = Instant::now();
    loop {
        let current = store.load(&id)?;
        match current.state {
            JobState::StartFailed => {
                let message = match &current.meta()?.phase {
                    Phase::Finished {
                        outcome: Outcome::StartFailed { message },
                        ..
                    } => message.clone(),
                    _ => "unknown error".into(),
                };
                return Err(Error::JobStartFailed {
                    job_id: id,
                    program: program.clone(),
                    message,
                });
            }
            JobState::Corrupt => {
                return Err(Error::CorruptJob {
                    id,
                    reason: current.meta.err().unwrap_or_default(),
                });
            }
            JobState::Starting | JobState::Lost => {}
            JobState::Running | JobState::Succeeded | JobState::Failed | JobState::Stopped => {
                print_started(&id, branch, &current.meta()?.command_text(), current.state);
                return Ok(());
            }
        }

        // If the supervisor process has already exited without reporting, record
        // `start-failed` now rather than leaving the job `starting`.
        let supervisor_exit = supervisor_process.try_wait().ok().flatten();
        if let Some(exit) = supervisor_exit {
            let details = supervisor_log_excerpt(&current.paths.supervisor_log);
            if supervisor::record_early_exit(&store, &id, exit, &details)? {
                continue; // the next load reports the recorded failure
            }
        }
        if supervisor_exit.is_some() || started.elapsed() >= START_TIMEOUT {
            // One last look: it may have finished between our load and the check.
            let last = store.load(&id)?;
            if !matches!(last.state, JobState::Starting | JobState::Lost) {
                continue;
            }
            return Err(Error::JobStartTimedOut {
                job_id: id,
                secs: started.elapsed().as_secs(),
                details: supervisor_log_excerpt(&last.paths.supervisor_log),
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn print_started(id: &str, branch: &str, command: &str, state: JobState) {
    println!("Started job {id} on branch `{branch}`: {command}");
    if state == JobState::Running {
        println!("  state: running");
    } else {
        println!("  state: {} (already finished)", state.label());
    }
    println!("  logs:  branchforge logs {id}");
    println!("  stop:  branchforge stop {id}");
}

/// First couple of KiB of the supervisor's own log, for error messages.
fn supervisor_log_excerpt(path: &Path) -> String {
    let mut text = String::new();
    if let Ok(file) = fs::File::open(path) {
        let _ = file.take(2048).read_to_string(&mut text);
    }
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}
