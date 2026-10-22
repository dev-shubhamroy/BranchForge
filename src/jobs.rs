//! Persistent job registry.
//!
//! Layout (inside the Git *common* directory, so it is shared by the main and
//! all linked worktrees, is never inside a worktree, and never shows up in
//! `git status`):
//!
//! ```text
//! <repo>/.git/branchforge/jobs/
//!   1/                 one directory per job; the name is the job id
//!     meta.json        metadata, replaced atomically (tmp file + rename)
//!     lock             held with flock by the job's supervisor while alive
//!     stdout.log       the program's stdout (appended, never truncated)
//!     stderr.log       the program's stderr
//!     supervisor.log   the supervisor's own diagnostics
//!     stop-requested   created by `stop`; contains the grace period in seconds
//! ```
//!
//! ## Who writes what
//!
//! Only the job's supervisor changes `meta.json` after creation, moving it
//! through `starting -> running -> finished`. Readers (`jobs`, `logs`, `stop`,
//! `remove`) never write metadata. A job's *effective* state is recomputed on
//! every read from the stored phase plus one liveness probe (is the supervisor
//! lock held?); see [`classify`]. Nothing about liveness is persisted, so
//! there is nothing to go stale.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::process;
use crate::repo::Repo;

pub const META_VERSION: u32 = 1;
/// Metadata is small; refuse to parse anything bigger than this.
const MAX_META_BYTES: u64 = 1024 * 1024;
/// How long a freshly created job may be "starting" without its supervisor
/// holding the lock yet, before we call it lost.
pub const STARTUP_GRACE_MS: u64 = 15_000;
/// Ids are canonical decimal numbers of at most this many digits.
const MAX_ID_DIGITS: usize = 9;

// ------------------------------------------------------------------ metadata

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobMeta {
    pub version: u32,
    pub id: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    pub created_at_ms: u64,
    pub phase: Phase,
}

/// What the supervisor last recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Running {
        pid: u32,
        supervisor_pid: u32,
        started_at_ms: u64,
    },
    Finished {
        outcome: Outcome,
        pid: Option<u32>,
        started_at_ms: Option<u64>,
        finished_at_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    /// Ended after a stop request (by whatever means).
    Stopped {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    /// The program could not be launched at all.
    StartFailed {
        message: String,
    },
}

impl JobMeta {
    /// Shell-style rendering for display, e.g. `sh -c 'echo hi; exit 3'`.
    /// Display only: the command is never re-parsed or run through a shell.
    pub fn command_text(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(quote_for_display)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Quote an argument if it contains anything but plainly safe characters.
fn quote_for_display(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

// --------------------------------------------------------------------- state

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Created; the supervisor has not reported the program as running yet.
    Starting,
    /// The supervisor is alive and its program was running when last recorded.
    Running,
    Succeeded,
    /// Exited non-zero or was killed by a signal nobody asked for.
    Failed,
    /// Ended after `stop`.
    Stopped,
    /// The program could not be launched.
    StartFailed,
    /// The metadata says it was running, but the supervisor is gone and no
    /// result was recorded. The program's fate is unknown.
    Lost,
    /// Metadata is missing or unreadable.
    Corrupt,
}

impl JobState {
    pub fn label(self) -> &'static str {
        match self {
            JobState::Starting => "starting",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Stopped => "stopped",
            JobState::StartFailed => "start-failed",
            JobState::Lost => "lost",
            JobState::Corrupt => "corrupt",
        }
    }

    /// States in which a supervisor is (or is about to be) alive.
    pub fn is_active(self) -> bool {
        matches!(self, JobState::Starting | JobState::Running)
    }
}

/// Effective state from persisted metadata plus one liveness observation.
///
/// * A recorded result is authoritative.
/// * `starting`/`running` are only believed while the supervisor lock is held
///   (a `starting` job also gets a short grace period before its supervisor
///   has taken the lock). Otherwise the job is `lost`.
pub fn classify(meta: &JobMeta, supervisor_alive: bool, now_ms: u64) -> JobState {
    match &meta.phase {
        Phase::Finished { outcome, .. } => match outcome {
            Outcome::Succeeded => JobState::Succeeded,
            Outcome::Failed { .. } => JobState::Failed,
            Outcome::Stopped { .. } => JobState::Stopped,
            Outcome::StartFailed { .. } => JobState::StartFailed,
        },
        Phase::Starting => {
            let young = now_ms.saturating_sub(meta.created_at_ms) < STARTUP_GRACE_MS;
            if supervisor_alive || young {
                JobState::Starting
            } else {
                JobState::Lost
            }
        }
        Phase::Running { .. } => {
            if supervisor_alive {
                JobState::Running
            } else {
                JobState::Lost
            }
        }
    }
}

// --------------------------------------------------------------------- paths

#[derive(Debug, Clone)]
pub struct JobPaths {
    pub dir: PathBuf,
    pub meta: PathBuf,
    pub lock: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub supervisor_log: PathBuf,
    pub stop_request: PathBuf,
}

impl JobPaths {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            meta: dir.join("meta.json"),
            lock: dir.join("lock"),
            stdout: dir.join("stdout.log"),
            stderr: dir.join("stderr.log"),
            supervisor_log: dir.join("supervisor.log"),
            stop_request: dir.join("stop-requested"),
            dir,
        }
    }
}

// -------------------------------------------------------------------- record

/// One job as observed right now.
#[derive(Debug)]
pub struct JobRecord {
    pub id: String,
    pub paths: JobPaths,
    /// `Err(reason)` when the metadata is missing or unreadable.
    pub meta: std::result::Result<JobMeta, String>,
    /// Whether a supervisor holds the job's lock.
    pub alive: bool,
    pub state: JobState,
}

impl JobRecord {
    /// A placeholder for a job directory that could not be read at all.
    fn unreadable(id: String, dir: PathBuf, reason: String) -> Self {
        Self {
            id,
            paths: JobPaths::new(dir),
            meta: Err(reason),
            alive: false,
            state: JobState::Corrupt,
        }
    }

    pub fn meta(&self) -> Result<&JobMeta> {
        self.meta.as_ref().map_err(|reason| Error::CorruptJob {
            id: self.id.clone(),
            reason: reason.clone(),
        })
    }

    /// Would this job make it unsafe to delete `branch`'s worktree?
    /// Jobs with unreadable metadata but a live supervisor block *every*
    /// removal, because we cannot tell which worktree they use.
    pub fn blocks_removal_of(&self, branch: &str) -> bool {
        match &self.meta {
            Ok(meta) => meta.branch == branch && self.state.is_active(),
            Err(_) => self.alive,
        }
    }

    pub fn is_lost_for(&self, branch: &str) -> bool {
        matches!(&self.meta, Ok(meta) if meta.branch == branch) && self.state == JobState::Lost
    }

    /// Exit status for display: `0`, `3`, `signal 15`, or `-`.
    pub fn exit_text(&self) -> String {
        let Ok(meta) = &self.meta else {
            return "-".into();
        };
        let Phase::Finished { outcome, .. } = &meta.phase else {
            return "-".into();
        };
        let (code, signal) = match outcome {
            Outcome::Succeeded => (Some(0), None),
            Outcome::Failed { exit_code, signal } | Outcome::Stopped { exit_code, signal } => {
                (*exit_code, *signal)
            }
            Outcome::StartFailed { .. } => (None, None),
        };
        match (code, signal) {
            (Some(code), _) => code.to_string(),
            (None, Some(signal)) => format!("signal {signal}"),
            (None, None) => "-".into(),
        }
    }

    /// When the program started (or, failing that, when the job was created).
    pub fn started_at_ms(&self) -> Option<u64> {
        let meta = self.meta.as_ref().ok()?;
        match &meta.phase {
            Phase::Running { started_at_ms, .. } => Some(*started_at_ms),
            Phase::Finished {
                started_at_ms: Some(ms),
                ..
            } => Some(*ms),
            _ => Some(meta.created_at_ms),
        }
    }
}

// --------------------------------------------------------------------- store

#[derive(Debug)]
pub struct JobStore {
    jobs_dir: PathBuf,
}

impl JobStore {
    /// A store rooted at an arbitrary directory (unit tests only).
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))] // its users are Unix-only tests
    pub(crate) fn at(jobs_dir: PathBuf) -> Self {
        Self { jobs_dir }
    }

    /// Open the registry if it exists. `Ok(None)` means no job was ever created.
    pub fn open_existing(repo: &Repo) -> Result<Option<Self>> {
        let root = data_root(repo)?;
        if !plain_dir_exists(&root)? {
            return Ok(None);
        }
        let jobs_dir = root.join("jobs");
        if !plain_dir_exists(&jobs_dir)? {
            return Ok(None);
        }
        Ok(Some(Self { jobs_dir }))
    }

    /// Open the registry, creating its directories (mode 0700) if needed.
    pub fn open_or_create(repo: &Repo) -> Result<Self> {
        let root = data_root(repo)?;
        ensure_private_dir(&root)?;
        let jobs_dir = root.join("jobs");
        ensure_private_dir(&jobs_dir)?;
        Ok(Self { jobs_dir })
    }

    /// Allocate the next id and publish a new job in the `starting` phase.
    ///
    /// The job is assembled in a hidden temp directory and renamed into place,
    /// so readers never see a half-built job and an existing job directory is
    /// never overwritten (a lost race for an id just moves on to the next one).
    pub fn create(
        &self,
        branch: &str,
        worktree: &Path,
        program: &str,
        args: &[String],
    ) -> Result<JobRecord> {
        let mut next = self.max_id()? + 1;
        loop {
            let id = next.to_string();
            let tmp = self
                .jobs_dir
                .join(format!(".tmp-{}-{}", std::process::id(), id));
            create_private_dir(&tmp).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;

            let paths = JobPaths::new(tmp.clone());
            let meta = JobMeta {
                version: META_VERSION,
                id: id.clone(),
                branch: branch.to_string(),
                worktree: worktree.to_path_buf(),
                program: program.to_string(),
                args: args.to_vec(),
                created_at_ms: now_ms(),
                phase: Phase::Starting,
            };
            let populated = (|| -> Result<()> {
                for file in [
                    &paths.lock,
                    &paths.stdout,
                    &paths.stderr,
                    &paths.supervisor_log,
                ] {
                    create_new_private(file)?;
                }
                write_meta(&paths, &meta)
            })();
            if let Err(e) = populated {
                // `tmp` is a directory we created a moment ago in our own registry.
                let _ = fs::remove_dir_all(&tmp);
                return Err(e);
            }

            let final_dir = self.jobs_dir.join(&id);
            match fs::rename(&tmp, &final_dir) {
                Ok(()) => return self.load(&id),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
                    ) =>
                {
                    // Someone else took this id first; discard our temp dir and retry.
                    let _ = fs::remove_dir_all(&tmp);
                    next += 1;
                }
                Err(source) => {
                    let _ = fs::remove_dir_all(&tmp);
                    return Err(Error::Io {
                        path: final_dir,
                        source,
                    });
                }
            }
        }
    }

    /// Load one job by id. The id is validated first, so it can never address
    /// anything outside the registry.
    pub fn load(&self, id: &str) -> Result<JobRecord> {
        validate_job_id(id)?;
        let dir = self.jobs_dir.join(id);
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(Error::CorruptJob {
                    id: id.to_string(),
                    reason: "job directory is a symlink or not a directory".into(),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::UnknownJob(id.to_string()));
            }
            Err(source) => return Err(Error::Io { path: dir, source }),
        }

        let paths = JobPaths::new(dir);
        let alive = process::supervisor_alive(&paths.lock)?;
        let meta = read_meta(&paths, id);
        let state = match &meta {
            Ok(m) => classify(m, alive, now_ms()),
            Err(_) => JobState::Corrupt,
        };
        Ok(JobRecord {
            id: id.to_string(),
            paths,
            meta,
            alive,
            state,
        })
    }

    /// All jobs, ordered by id. Entries that are not plain numeric directories
    /// (temp dirs, symlinks, stray files) are skipped and never followed.
    pub fn list(&self) -> Result<Vec<JobRecord>> {
        let mut records = Vec::new();
        for id in self.ids()? {
            match self.load(&id.to_string()) {
                Ok(record) => records.push(record),
                // Removed or replaced while we were listing.
                Err(Error::UnknownJob(_) | Error::CorruptJob { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(records)
    }

    /// Like [`JobStore::list`], but a job whose files cannot be read at all
    /// (for example a permission error) is shown as a `corrupt` row instead of
    /// failing the whole listing. For display only: policy decisions such as
    /// `remove` use the strict [`JobStore::list`], which fails closed.
    pub fn list_lenient(&self) -> Result<Vec<JobRecord>> {
        let mut records = Vec::new();
        for id in self.ids()? {
            let id = id.to_string();
            match self.load(&id) {
                Ok(record) => records.push(record),
                Err(Error::UnknownJob(_) | Error::CorruptJob { .. }) => {}
                Err(Error::Io { path, source }) => {
                    records.push(JobRecord::unreadable(
                        id.clone(),
                        self.jobs_dir.join(&id),
                        format!("cannot read {}: {source}", path.display()),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(records)
    }

    fn ids(&self) -> Result<Vec<u32>> {
        let io_err = |source| Error::Io {
            path: self.jobs_dir.clone(),
            source,
        };
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.jobs_dir).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            let is_real_dir = entry.file_type().map_err(io_err)?.is_dir();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_real_dir
                && validate_job_id(name).is_ok()
                && let Ok(n) = name.parse::<u32>()
            {
                ids.push(n);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    fn max_id(&self) -> Result<u32> {
        Ok(self.ids()?.last().copied().unwrap_or(0))
    }
}

/// Take the repository-wide registry lock (`<git-dir>/branchforge.lock`).
///
/// `run` holds it while it re-reads the worktree list and registers a job;
/// `remove` holds it from its first check until the worktree is deleted. Because
/// a registered job counts as active from the moment it is published, whichever
/// of the two gets the lock first, the other observes its effect:
///
/// * `run` first: its job exists, so `remove` refuses.
/// * `remove` first: the worktree is gone by the time `run` re-reads it, so
///   `run` refuses and registers nothing.
///
/// On platforms without jobs this is a no-op, so `remove` keeps working there.
pub fn registry_lock(repo: &Repo) -> Result<process::RegistryLock> {
    process::RegistryLock::acquire(&common_dir(repo)?.join("branchforge.lock"))
}

/// Removal policy for jobs. Fails with [`Error::ActiveJobs`] if any job for
/// `branch` has a live supervisor (or any job with unreadable metadata does,
/// since we cannot tell which worktree it uses). `--force` does not override
/// this: deleting a directory under a live process destroys its work.
///
/// Returns the ids of `lost` jobs for this branch (supervisor gone, outcome
/// unknown). Those do not block removal, but the caller should warn.
pub fn check_removal(repo: &Repo, branch: &str) -> Result<Vec<String>> {
    let Some(store) = JobStore::open_existing(repo)? else {
        return Ok(Vec::new());
    };
    let records = store.list()?;
    let blocking: Vec<&str> = records
        .iter()
        .filter(|r| r.blocks_removal_of(branch))
        .map(|r| r.id.as_str())
        .collect();
    if !blocking.is_empty() {
        return Err(Error::ActiveJobs {
            branch: branch.to_string(),
            ids: blocking.join(", "),
        });
    }
    Ok(records
        .iter()
        .filter(|r| r.is_lost_for(branch))
        .map(|r| r.id.clone())
        .collect())
}

/// Job ids are canonical positive decimal numbers: no sign, no leading zero,
/// no separators. That makes path traversal impossible by construction and
/// gives each job exactly one spelling.
pub fn validate_job_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID_DIGITS
        && id.bytes().all(|b| b.is_ascii_digit())
        && !id.starts_with('0');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidJobId(id.to_string()))
    }
}

/// Atomically replace a job's metadata (write a temp file, then rename it).
pub fn write_meta(paths: &JobPaths, meta: &JobMeta) -> Result<()> {
    let json = serde_json::to_vec_pretty(meta).map_err(Error::Metadata)?;
    let tmp = paths
        .dir
        .join(format!("meta.json.tmp-{}", std::process::id()));
    let io_err = |source| Error::Io {
        path: paths.meta.clone(),
        source,
    };
    let mut file = private_open_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(io_err)?;
    file.write_all(&json).map_err(io_err)?;
    file.sync_all().map_err(io_err)?;
    drop(file);
    fs::rename(&tmp, &paths.meta).map_err(io_err)
}

fn read_meta(paths: &JobPaths, id: &str) -> std::result::Result<JobMeta, String> {
    let file = match File::open(&paths.meta) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err("metadata file is missing".into());
        }
        Err(e) => return Err(format!("cannot read metadata: {e}")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_META_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read metadata: {e}"))?;
    if bytes.len() as u64 > MAX_META_BYTES {
        return Err("metadata file is unreasonably large".into());
    }
    let meta: JobMeta =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid metadata: {e}"))?;
    if meta.version != META_VERSION {
        return Err(format!(
            "unsupported metadata version {} (this build understands {META_VERSION})",
            meta.version
        ));
    }
    if meta.id != id {
        return Err(format!("metadata belongs to job `{}`, not `{id}`", meta.id));
    }
    Ok(meta)
}

// ------------------------------------------------------------------- helpers

fn common_dir(repo: &Repo) -> Result<PathBuf> {
    let out = repo.git().run(["rev-parse", "--git-common-dir"])?;
    let dir = PathBuf::from(out.trim_end_matches(['\n', '\r']));
    Ok(if dir.is_absolute() {
        dir
    } else {
        repo.main().path.join(dir)
    })
}

fn data_root(repo: &Repo) -> Result<PathBuf> {
    Ok(common_dir(repo)?.join("branchforge"))
}

/// Does `path` exist as a real directory? Errors on symlinks and files.
fn plain_dir_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => Ok(true),
        Ok(_) => Err(Error::BadDataDir(path.to_path_buf())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    if plain_dir_exists(path)? {
        return Ok(());
    }
    match create_private_dir(path) {
        Ok(()) => Ok(()),
        // Lost a race with another BranchForge process: re-validate what is there.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => plain_dir_exists(path).map(|_| ()),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn private_open_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Create a new empty private file; fails if it already exists.
fn create_new_private(path: &Path) -> Result<()> {
    private_open_options()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `2026-10-22 12:00:01Z` (UTC; no time-zone database needed).
pub fn format_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(phase: Phase) -> JobMeta {
        JobMeta {
            version: META_VERSION,
            id: "7".into(),
            branch: "feat".into(),
            worktree: PathBuf::from("/w/feat"),
            program: "echo".into(),
            args: vec!["a b".into(), "--flag".into()],
            created_at_ms: 1_000_000,
            phase,
        }
    }

    fn finished(outcome: Outcome) -> Phase {
        Phase::Finished {
            outcome,
            pid: Some(10),
            started_at_ms: Some(1_000_100),
            finished_at_ms: 1_000_200,
        }
    }

    const RUNNING: Phase = Phase::Running {
        pid: 10,
        supervisor_pid: 9,
        started_at_ms: 1_000_100,
    };

    #[test]
    fn recorded_results_are_authoritative_regardless_of_liveness() {
        let now = 1_000_000 + STARTUP_GRACE_MS * 10;
        for alive in [true, false] {
            let cases = [
                (Outcome::Succeeded, JobState::Succeeded),
                (
                    Outcome::Failed {
                        exit_code: Some(3),
                        signal: None,
                    },
                    JobState::Failed,
                ),
                (
                    Outcome::Stopped {
                        exit_code: None,
                        signal: Some(15),
                    },
                    JobState::Stopped,
                ),
                (
                    Outcome::StartFailed {
                        message: "x".into(),
                    },
                    JobState::StartFailed,
                ),
            ];
            for (outcome, expected) in cases {
                assert_eq!(classify(&meta(finished(outcome)), alive, now), expected);
            }
        }
    }

    #[test]
    fn running_is_only_believed_while_the_supervisor_lock_is_held() {
        let m = meta(RUNNING);
        assert_eq!(classify(&m, true, 5_000_000), JobState::Running);
        assert_eq!(classify(&m, false, 5_000_000), JobState::Lost);
        // Even a "recent" running job is lost the moment its supervisor is gone.
        assert_eq!(classify(&m, false, 1_000_001), JobState::Lost);
    }

    #[test]
    fn starting_gets_a_grace_period_then_becomes_lost() {
        let m = meta(Phase::Starting);
        assert_eq!(classify(&m, false, 1_000_000 + 1), JobState::Starting);
        assert_eq!(
            classify(&m, true, 1_000_000 + STARTUP_GRACE_MS * 5),
            JobState::Starting
        );
        assert_eq!(
            classify(&m, false, 1_000_000 + STARTUP_GRACE_MS),
            JobState::Lost
        );
        // A clock that went backwards must not panic or underflow.
        assert_eq!(classify(&m, false, 5), JobState::Starting);
    }

    #[test]
    fn only_starting_and_running_are_active() {
        for (state, active) in [
            (JobState::Starting, true),
            (JobState::Running, true),
            (JobState::Succeeded, false),
            (JobState::Failed, false),
            (JobState::Stopped, false),
            (JobState::StartFailed, false),
            (JobState::Lost, false),
            (JobState::Corrupt, false),
        ] {
            assert_eq!(state.is_active(), active, "{state:?}");
        }
    }

    #[test]
    fn job_ids_are_canonical_numbers_only() {
        for good in ["1", "42", "999999999"] {
            assert!(validate_job_id(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "0",
            "01",
            "-1",
            "+1",
            "1.5",
            "1e3",
            " 1",
            "1 ",
            "abc",
            "../1",
            "1/../1",
            "1/",
            "/1",
            "..",
            ".",
            "1\0",
            "١٢",
            "1234567890",
        ] {
            assert!(
                matches!(validate_job_id(bad), Err(Error::InvalidJobId(_))),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn command_text_quotes_awkward_arguments_for_display() {
        let mut m = meta(Phase::Starting);
        m.program = "sh".into();
        m.args = vec![
            "-c".into(),
            "echo 'hi'; exit 3".into(),
            String::new(),
            "plain/path-1.txt".into(),
        ];
        assert_eq!(
            m.command_text(),
            r#"sh -c 'echo '\''hi'\''; exit 3' '' plain/path-1.txt"#
        );
    }

    #[test]
    fn metadata_round_trips_through_json_including_awkward_arguments() {
        let mut m = meta(finished(Outcome::Failed {
            exit_code: Some(2),
            signal: None,
        }));
        m.args = vec![
            "line1\nline2".into(),
            "quote\"s".into(),
            "ünï".into(),
            String::new(),
        ];
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<JobMeta>(&json).unwrap(), m);
    }

    #[test]
    fn phase_json_shape_is_stable() {
        let json = serde_json::to_value(meta(RUNNING)).unwrap();
        assert_eq!(json["phase"]["state"], "running");
        assert_eq!(json["phase"]["pid"], 10);
        let json = serde_json::to_value(meta(finished(Outcome::Succeeded))).unwrap();
        assert_eq!(json["phase"]["state"], "finished");
        assert_eq!(json["phase"]["outcome"]["result"], "succeeded");
    }

    #[test]
    fn exit_text_covers_codes_signals_and_unfinished_jobs() {
        let record = |phase| JobRecord {
            id: "7".into(),
            paths: JobPaths::new(PathBuf::from("/x")),
            meta: Ok(meta(phase)),
            alive: false,
            state: JobState::Failed,
        };
        assert_eq!(record(finished(Outcome::Succeeded)).exit_text(), "0");
        assert_eq!(
            record(finished(Outcome::Failed {
                exit_code: Some(3),
                signal: None
            }))
            .exit_text(),
            "3"
        );
        assert_eq!(
            record(finished(Outcome::Stopped {
                exit_code: None,
                signal: Some(15)
            }))
            .exit_text(),
            "signal 15"
        );
        assert_eq!(
            record(finished(Outcome::StartFailed {
                message: "x".into()
            }))
            .exit_text(),
            "-"
        );
        assert_eq!(record(RUNNING).exit_text(), "-");
    }

    #[test]
    fn removal_blocking_rules() {
        let rec = |meta: std::result::Result<JobMeta, String>, alive, state| JobRecord {
            id: "1".into(),
            paths: JobPaths::new(PathBuf::from("/x")),
            meta,
            alive,
            state,
        };
        let running = rec(Ok(meta(RUNNING)), true, JobState::Running);
        assert!(running.blocks_removal_of("feat"));
        assert!(
            !running.blocks_removal_of("other"),
            "other branches are unaffected"
        );

        let lost = rec(Ok(meta(RUNNING)), false, JobState::Lost);
        assert!(!lost.blocks_removal_of("feat"));
        assert!(lost.is_lost_for("feat") && !lost.is_lost_for("other"));

        let done = rec(
            Ok(meta(finished(Outcome::Succeeded))),
            false,
            JobState::Succeeded,
        );
        assert!(!done.blocks_removal_of("feat"));

        // Unreadable metadata: only a live supervisor blocks, but then it blocks everything.
        assert!(rec(Err("bad".into()), true, JobState::Corrupt).blocks_removal_of("anything"));
        assert!(!rec(Err("bad".into()), false, JobState::Corrupt).blocks_removal_of("anything"));
    }

    #[test]
    fn utc_formatting_matches_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00Z");
        assert_eq!(format_utc(1_000_000_000_000), "2001-09-09 01:46:40Z");
        assert_eq!(format_utc(1_700_000_000_000), "2023-11-14 22:13:20Z");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29 00:00:00Z"); // leap day
        assert_eq!(format_utc(4_102_444_799_999), "2099-12-31 23:59:59Z");
    }

    #[test]
    fn store_allocates_unique_ids_and_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JobStore {
            jobs_dir: tmp.path().to_path_buf(),
        };
        let wt = Path::new("/w/feat");
        let a = store.create("feat", wt, "echo", &[]).unwrap();
        let b = store.create("feat", wt, "echo", &[]).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("1", "2"));
        assert_eq!(a.state, JobState::Starting);

        // A pre-existing directory with the next id (and content) is left alone.
        fs::create_dir(tmp.path().join("3")).unwrap();
        fs::write(tmp.path().join("3/precious"), "keep").unwrap();
        let c = store.create("feat", wt, "echo", &[]).unwrap();
        assert_eq!(c.id, "4");
        assert_eq!(
            fs::read_to_string(tmp.path().join("3/precious")).unwrap(),
            "keep"
        );

        let ids: Vec<_> = store.list().unwrap().into_iter().map(|r| r.id).collect();
        // "3" has no metadata -> listed as corrupt rather than hidden.
        assert_eq!(ids, ["1", "2", "3", "4"]);
        assert_eq!(store.load("3").unwrap().state, JobState::Corrupt);
    }

    #[test]
    fn list_skips_temp_dirs_stray_files_and_non_canonical_names() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JobStore {
            jobs_dir: tmp.path().to_path_buf(),
        };
        store.create("feat", Path::new("/w"), "echo", &[]).unwrap();
        fs::create_dir(tmp.path().join(".tmp-1-9")).unwrap();
        fs::create_dir(tmp.path().join("007")).unwrap();
        fs::create_dir(tmp.path().join("notes")).unwrap();
        fs::write(tmp.path().join("5"), "a file named like an id").unwrap();
        let ids: Vec<_> = store.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["1"]);
    }

    #[test]
    fn corrupt_metadata_variants_are_all_reported_not_fatal() {
        fn good(id: &str) -> String {
            serde_json::to_string(&JobMeta {
                id: id.into(),
                ..meta(RUNNING)
            })
            .unwrap()
        }
        let tmp = tempfile::tempdir().unwrap();
        let store = JobStore {
            jobs_dir: tmp.path().to_path_buf(),
        };
        type MakeMeta = fn(&str) -> String;
        let cases: [(&str, MakeMeta); 6] = [
            ("garbage", |_| "not json at all".into()),
            ("truncated", |id| {
                let g = good(id);
                g[..g.len() / 2].to_string()
            }),
            ("empty", |_| String::new()),
            ("wrong version", |id| {
                good(id).replace("\"version\":1", "\"version\":99")
            }),
            ("id of another job", |_| good("8")),
            ("wrong field type", |id| {
                good(id).replace("\"pid\":10", "\"pid\":\"ten\"")
            }),
        ];
        for (i, (name, make)) in cases.into_iter().enumerate() {
            let id = (i + 1).to_string();
            let paths = JobPaths::new(tmp.path().join(&id));
            fs::create_dir(&paths.dir).unwrap();
            fs::write(&paths.meta, make(&id)).unwrap();
            let rec = store.load(&id).unwrap();
            assert_eq!(rec.state, JobState::Corrupt, "{name}: {:?}", rec.meta);
            assert!(!rec.state.is_active());
        }
        // Sanity: the same helper produces metadata that does load.
        let paths = JobPaths::new(tmp.path().join("7"));
        fs::create_dir(&paths.dir).unwrap();
        fs::write(&paths.meta, good("7")).unwrap();
        assert_eq!(store.load("7").unwrap().state, JobState::Lost);

        // Missing metadata file entirely.
        fs::create_dir(tmp.path().join("9")).unwrap();
        let rec = store.load("9").unwrap();
        assert_eq!(rec.state, JobState::Corrupt);
        assert!(matches!(rec.meta(), Err(Error::CorruptJob { .. })));
    }

    #[test]
    fn oversized_metadata_is_rejected_without_reading_it_all() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JobStore {
            jobs_dir: tmp.path().to_path_buf(),
        };
        let paths = JobPaths::new(tmp.path().join("1"));
        fs::create_dir(&paths.dir).unwrap();
        fs::write(&paths.meta, vec![b' '; (MAX_META_BYTES + 10) as usize]).unwrap();
        let rec = store.load("1").unwrap();
        assert_eq!(rec.state, JobState::Corrupt);
        assert!(rec.meta.unwrap_err().contains("large"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_job_directories_are_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let store = JobStore {
            jobs_dir: tmp.path().to_path_buf(),
        };
        std::os::unix::fs::symlink(elsewhere.path(), tmp.path().join("1")).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(matches!(store.load("1"), Err(Error::CorruptJob { .. })));
    }
}
