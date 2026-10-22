//! Platform layer for job supervision. This is the only module with `unsafe`
//! code or OS-specific APIs.
//!
//! Three primitives, all Unix-only (other platforms get stubs that return
//! [`Error::UnsupportedPlatform`]):
//!
//! * **Supervisor lock**: the supervisor holds an exclusive `flock` on a job's
//!   lock file for its whole life. "Is the job alive?" is answered by asking
//!   whether that lock is held, never by looking up a PID. The kernel drops the
//!   lock when the holder dies, so there is no stale state and no PID-reuse risk.
//! * **Process groups**: jobs run in their own group so one signal reaches the
//!   program and its children, and terminal signals (Ctrl-C) don't.
//! * **Group signalling**, guarded so it can never target PID 0/1 or "all
//!   processes" (`kill(-1)`).

#[cfg(unix)]
pub use unix::*;

#[cfg(not(unix))]
pub use unsupported::*;

use crate::error::{Error, Result};

/// Fail fast, before touching any state, on platforms where jobs cannot work.
/// Every job command calls this first so none of them can half-run.
pub fn ensure_supported() -> Result<()> {
    require_job_support(cfg!(unix))
}

fn require_job_support(supported: bool) -> Result<()> {
    if supported {
        Ok(())
    } else {
        Err(Error::UnsupportedPlatform(
            "file locking and process groups",
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Polite request to terminate (SIGTERM).
    Term,
    /// Unconditional kill (SIGKILL).
    Kill,
}

#[cfg(unix)]
mod unix {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    use super::Signal;
    use crate::error::{Error, Result};

    /// Exclusive lock held by a supervisor; released when dropped or when the
    /// process dies.
    #[derive(Debug)]
    pub struct SupervisorLock {
        _file: File,
    }

    impl SupervisorLock {
        /// Block until the exclusive lock on `path` is ours. Readers hold the
        /// lock only for microseconds while probing, so this returns promptly.
        pub fn acquire(path: &Path) -> Result<Self> {
            let io_err = |source| Error::Io {
                path: path.to_path_buf(),
                source,
            };
            let file = File::open(path).map_err(io_err)?;
            loop {
                // SAFETY: `file` is an open descriptor owned by this scope.
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if rc == 0 {
                    return Ok(Self { _file: file });
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(io_err(err));
                }
            }
        }
    }

    /// Repository-wide lock serialising the two operations that must not
    /// interleave: *registering a job* (`run`) and *deleting a worktree*
    /// (`remove`). Held for the duration of that critical section only.
    #[derive(Debug)]
    pub struct RegistryLock {
        _file: File,
    }

    impl RegistryLock {
        /// Create `path` if needed (mode 0600) and block until we hold it exclusively.
        pub fn acquire(path: &Path) -> Result<Self> {
            use std::os::unix::fs::OpenOptionsExt;
            let io_err = |source| Error::Io {
                path: path.to_path_buf(),
                source,
            };
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)
                .map_err(io_err)?;
            loop {
                // SAFETY: `file` is an open descriptor owned by this scope.
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if rc == 0 {
                    return Ok(Self { _file: file });
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(io_err(err));
                }
            }
        }
    }

    /// Is a supervisor currently holding the lock at `path`?
    /// A missing lock file means "no".
    pub fn supervisor_alive(path: &Path) -> Result<bool> {
        let io_err = |source| Error::Io {
            path: path.to_path_buf(),
            source,
        };
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_err(e)),
        };
        // SAFETY: `file` is an open descriptor owned by this scope. A shared,
        // non-blocking attempt fails exactly when an exclusive lock is held;
        // dropping `file` releases a lock we managed to take.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(false);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            Ok(true)
        } else {
            Err(io_err(err))
        }
    }

    /// Put the command in its own process group (so it is detached from the
    /// terminal's foreground group and can be signalled as a unit).
    pub fn own_process_group(cmd: &mut Command) {
        cmd.process_group(0);
    }

    /// The signal that terminated the process, if it died from one.
    pub fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }

    /// Signal every process in group `pgid`. Refuses PIDs that could address
    /// the whole system or init.
    pub fn signal_group(pgid: u32, signal: Signal) -> io::Result<()> {
        if pgid <= 1 || pgid > i32::MAX as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to signal process group {pgid}"),
            ));
        }
        let sig = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: `kill` takes plain integers. The negated, range-checked
        // `pgid` addresses exactly one process group.
        let rc = unsafe { libc::kill(-(pgid as i32), sig) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn lock_is_visible_while_held_and_gone_after_drop() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("lock");
            std::fs::write(&path, "").unwrap();

            assert!(!supervisor_alive(&path).unwrap());
            let guard = SupervisorLock::acquire(&path).unwrap();
            assert!(supervisor_alive(&path).unwrap());
            assert!(
                supervisor_alive(&path).unwrap(),
                "probing must not steal the lock"
            );
            drop(guard);
            assert!(!supervisor_alive(&path).unwrap());
        }

        #[test]
        fn registry_lock_excludes_other_holders_until_released() {
            use std::sync::mpsc;
            use std::time::Duration;

            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("registry.lock");
            let first = RegistryLock::acquire(&path).unwrap();

            let (tx, rx) = mpsc::channel();
            let contender = {
                let path = path.clone();
                std::thread::spawn(move || {
                    let _second = RegistryLock::acquire(&path).unwrap();
                    tx.send(()).unwrap();
                })
            };
            // While we hold it, the contender must stay blocked.
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
            drop(first);
            rx.recv_timeout(Duration::from_secs(10))
                .expect("the lock must be granted once released");
            contender.join().unwrap();
        }

        #[test]
        fn registry_lock_file_is_private() {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("registry.lock");
            let _lock = RegistryLock::acquire(&path).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        #[test]
        fn missing_lock_file_means_not_alive() {
            let tmp = tempfile::tempdir().unwrap();
            assert!(!supervisor_alive(&tmp.path().join("nope")).unwrap());
        }

        #[test]
        fn signal_group_refuses_dangerous_targets() {
            for bad in [0u32, 1, u32::MAX, i32::MAX as u32 + 1] {
                let err = signal_group(bad, Signal::Kill).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "pgid {bad}");
            }
        }
    }
}

#[cfg(not(unix))]
mod unsupported {
    use std::io;
    use std::path::Path;
    use std::process::Command;

    use super::Signal;
    use crate::error::{Error, Result};

    #[derive(Debug)]
    pub struct SupervisorLock;

    impl SupervisorLock {
        pub fn acquire(_path: &Path) -> Result<Self> {
            Err(Error::UnsupportedPlatform(
                "file locking and process groups",
            ))
        }
    }

    /// No jobs can exist here, so there is nothing to serialise. This must be a
    /// no-op (not an error) so `remove` keeps working on these platforms.
    #[derive(Debug)]
    pub struct RegistryLock;

    impl RegistryLock {
        pub fn acquire(_path: &Path) -> Result<Self> {
            Ok(Self)
        }
    }

    pub fn supervisor_alive(_path: &Path) -> Result<bool> {
        Err(Error::UnsupportedPlatform(
            "file locking and process groups",
        ))
    }

    pub fn own_process_group(_cmd: &mut Command) {}

    pub fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
        None
    }

    pub fn signal_group(_pgid: u32, _signal: Signal) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process groups are not supported on this platform",
        ))
    }
}

#[cfg(test)]
mod platform_tests {
    use super::*;

    #[test]
    fn unsupported_platforms_get_a_clear_typed_error() {
        let err = require_job_support(false).unwrap_err();
        assert!(matches!(err, Error::UnsupportedPlatform(_)));
        let message = err.to_string();
        assert!(
            message.contains("only supported on Unix-like systems"),
            "{message}"
        );
        assert!(require_job_support(true).is_ok());
    }

    #[test]
    fn this_platform_reports_its_real_support() {
        assert_eq!(ensure_supported().is_ok(), cfg!(unix));
    }
}
