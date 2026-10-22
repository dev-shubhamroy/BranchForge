use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{Error, Result};
use crate::jobs::JobStore;
use crate::repo::Repo;

/// Print a job's captured output.
///
/// With neither flag (or both), stdout then stderr are printed as labelled
/// sections. With exactly one flag, only that stream is printed, raw and
/// unlabelled, so it is safe to pipe. Logs are streamed from disk, never
/// loaded whole into memory, and remain readable even if the job's metadata
/// is corrupt.
pub fn run(repo: &Repo, id: &str, only_stdout: bool, only_stderr: bool) -> Result<()> {
    let store = JobStore::open_existing(repo)?.ok_or_else(|| Error::UnknownJob(id.to_string()))?;
    let record = store.load(id)?;

    let want_stdout = only_stdout || !only_stderr;
    let want_stderr = only_stderr || !only_stdout;
    let sectioned = want_stdout && want_stderr;

    let mut printed_any = false;
    let result = (|| -> io::Result<()> {
        if want_stdout {
            printed_any |= copy_log(&record.paths.stdout, sectioned.then_some("stdout"))?;
        }
        if want_stderr {
            printed_any |= copy_log(&record.paths.stderr, sectioned.then_some("stderr"))?;
        }
        io::stdout().flush()
    })();

    match result {
        Ok(()) => {}
        // `branchforge logs 1 | head` closing the pipe early is not an error.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
        Err(source) => {
            return Err(Error::Io {
                path: record.paths.dir.clone(),
                source,
            });
        }
    }
    if sectioned && !printed_any {
        eprintln!("(job {id} has produced no output)");
    }
    Ok(())
}

/// Stream one log file to stdout. Returns whether it had any content.
/// A missing file counts as empty.
fn copy_log(path: &Path, header: Option<&str>) -> io::Result<bool> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    let mut out = io::stdout().lock();
    if let Some(name) = header {
        writeln!(out, "==> {name} <==")?;
    }
    let copied = io::copy(&mut file, &mut out)?;
    if header.is_some() && copied > 0 && !ends_with_newline(path)? {
        writeln!(out)?;
    }
    Ok(copied > 0)
}

fn ends_with_newline(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}
