use std::fs;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::jobs::{JobPaths, JobState, JobStore, Outcome, Phase};
use crate::repo::Repo;

/// Extra time, beyond the grace period, to wait for the supervisor to record
/// the result before reporting that the stop has not completed.
const WAIT_SLACK: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Ask a job's supervisor to terminate its program, then wait for the result.
///
/// The CLI never signals a process. It drops a request file; the supervisor
/// (which owns the program and holds the job lock) sends SIGTERM to the
/// program's process group, and SIGKILL if it is still running after
/// `grace_secs`. Because only a live supervisor can act on the request, a
/// stale or forged PID in the metadata can never cause an unrelated process to
/// be signalled.
pub fn run(repo: &Repo, id: &str, grace_secs: u64) -> Result<()> {
    let store = JobStore::open_existing(repo)?.ok_or_else(|| Error::UnknownJob(id.to_string()))?;
    let record = store.load(id)?;

    match record.state {
        JobState::Starting | JobState::Running => {}
        JobState::Corrupt => {
            return Err(Error::CorruptJob {
                id: id.to_string(),
                reason: record.meta.err().unwrap_or_default(),
            });
        }
        other => {
            return Err(Error::JobNotRunning {
                id: id.to_string(),
                state: describe_not_running(other),
            });
        }
    }

    request_stop(&record.paths, grace_secs)?;

    let wait = Duration::from_secs(grace_secs) + WAIT_SLACK;
    let started = Instant::now();
    loop {
        let current = store.load(id)?;
        match current.state {
            JobState::Starting | JobState::Running => {}
            JobState::Stopped => {
                println!("Stopped job {id}: {}", how_it_ended(&current.meta()?.phase));
                return Ok(());
            }
            JobState::Succeeded | JobState::Failed | JobState::StartFailed => {
                println!(
                    "Job {id} had already finished on its own ({}, exit {}); nothing was stopped.",
                    current.state.label(),
                    current.exit_text()
                );
                return Ok(());
            }
            JobState::Lost | JobState::Corrupt => {
                return Err(Error::JobNotRunning {
                    id: id.to_string(),
                    state: format!(
                        "{}: its supervisor died while the stop was pending",
                        current.state.label()
                    ),
                });
            }
        }
        if started.elapsed() >= wait {
            return Err(Error::StopTimedOut {
                id: id.to_string(),
                secs: wait.as_secs(),
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn describe_not_running(state: JobState) -> String {
    match state {
        JobState::Lost => {
            "lost: its supervisor is gone, so BranchForge cannot verify or signal its process; \
check for a leftover process yourself"
                .into()
        }
        other => other.label().into(),
    }
}

/// Write the request atomically (temp file + rename). A repeated `stop`
/// replaces the file, which is how a later request can shorten the grace
/// period to force a kill.
fn request_stop(paths: &JobPaths, grace_secs: u64) -> Result<()> {
    let io_err = |source| Error::Io {
        path: paths.stop_request.clone(),
        source,
    };
    let tmp = paths
        .dir
        .join(format!("stop-requested.tmp-{}", std::process::id()));
    fs::write(&tmp, grace_secs.to_string()).map_err(io_err)?;
    fs::rename(&tmp, &paths.stop_request).map_err(io_err)
}

fn how_it_ended(phase: &Phase) -> String {
    let Phase::Finished {
        outcome: Outcome::Stopped { exit_code, signal },
        ..
    } = phase
    else {
        return "stopped".into();
    };
    match (exit_code, signal) {
        (_, Some(signal)) => format!("terminated by {}", signal_name(*signal)),
        (Some(code), None) => format!("exited with code {code} after the stop request"),
        (None, None) => "stopped".into(),
    }
}

fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        9 => "SIGKILL".into(),
        15 => "SIGTERM".into(),
        other => format!("signal {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_how_a_stopped_job_ended() {
        let finished = |exit_code, signal| Phase::Finished {
            outcome: Outcome::Stopped { exit_code, signal },
            pid: None,
            started_at_ms: None,
            finished_at_ms: 0,
        };
        assert_eq!(
            how_it_ended(&finished(None, Some(15))),
            "terminated by SIGTERM"
        );
        assert_eq!(
            how_it_ended(&finished(None, Some(9))),
            "terminated by SIGKILL"
        );
        assert_eq!(
            how_it_ended(&finished(None, Some(31))),
            "terminated by signal 31"
        );
        assert_eq!(
            how_it_ended(&finished(Some(0), None)),
            "exited with code 0 after the stop request"
        );
        assert_eq!(how_it_ended(&Phase::Starting), "stopped");
    }

    #[test]
    fn stop_request_is_written_atomically_and_can_be_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = JobPaths::new(tmp.path().to_path_buf());
        request_stop(&paths, 7).unwrap();
        assert_eq!(fs::read_to_string(&paths.stop_request).unwrap(), "7");
        request_stop(&paths, 0).unwrap();
        assert_eq!(fs::read_to_string(&paths.stop_request).unwrap(), "0");
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(leftovers, ["stop-requested"], "no temp files left behind");
    }
}
