# BranchForge

A small Rust CLI that manages Git worktrees in a predictable layout, only ever
removes worktrees it can prove it created, and can run long-lived commands in
those worktrees as supervised background jobs.

A Git worktree lets you check out several branches at once, each in its own
directory, sharing one repository. BranchForge gives every branch its own
worktree in a sibling directory so parallel tasks never fight over one working
copy.

```
~/code/
├── app/                        <- your repository (main worktree)
├── app.worktrees/              <- managed by BranchForge
│   ├── feature%2Flogin/        <- branch `feature/login`
│   └── hotfix/                 <- branch `hotfix`
```

## Requirements

* Rust 1.88+ (declared as `rust-version` in Cargo.toml; edition 2024 plus let-chains). Developed and tested on 1.90 only; older toolchains were not tested.
* Git 2.28+ on your `PATH` (the integration tests use `git init -b`)
* Worktree management (`new`, `list`, `status`, `remove`) is cross-platform in
  intent but has only been **run** on macOS. **Jobs (`run`, `jobs`, `logs`,
  `stop`) need a Unix-like system** (they use `flock` and process groups).
  What is and is not verified for other platforms:
  * *Compile-time (verified):* the whole crate, tests included, type-checks for
    `x86_64-pc-windows-gnu`.
  * *Runtime (NOT verified):* nothing has been executed on Windows or Linux. By
    design every job command first calls `process::ensure_supported()` and, on a
    platform without job support, fails with "running jobs is only supported on
    Unix-like systems" **before touching any state**, and `remove` keeps working
    there (its registry lock is a no-op). On Unix the tests cover the decision
    logic and which commands it applies to; the failing branch itself has never
    been executed.

## Installation

```bash
git clone <this repository> && cd BranchForge
cargo install --path .        # installs `branchforge` into ~/.cargo/bin
# or just build it:
cargo build --release         # binary at target/release/branchforge
```

## Usage

Run commands from inside the repository or from any of its worktrees.

```bash
# Create a worktree (and the branch, from HEAD) for `feature/login`
branchforge new feature/login

# Start a NEW branch from a specific commit-ish
branchforge new hotfix --base v1.2.0

# Check out an EXISTING branch in a worktree (no --base allowed)
branchforge new existing-branch

# Show every worktree: branch, kind (main/managed/external), state, sync, path
branchforge list

# Repo summary, managed worktrees, and warnings about stray directories
branchforge status

# Remove a managed worktree. Refuses if it contains uncommitted changes
# (including untracked files) OR Git-ignored content such as .env or node_modules/
branchforge remove feature/login

# PERMANENTLY delete the worktree including uncommitted changes and ignored files
branchforge remove feature/login --force

# Run any program as a supervised background job inside a managed worktree
branchforge run feature/login -- cargo test --release

# List jobs, read their output, stop them
branchforge jobs
branchforge logs 1
branchforge stop 1
```

### What `remove` and `--force` do

* Without `--force`, `remove` refuses to delete a worktree that has uncommitted
  changes **or** Git-ignored content, and tells you which paths are at risk.
* `--force` can **permanently delete** both uncommitted changes and ignored
  files, including local files such as `.env` and directories such as
  `node_modules/`. They are not stored in Git and cannot be recovered. Copy
  anything you need first.
* `--force` does **not** weaken any other protection. These still apply:
  ownership and path validation (the worktree must be exactly where BranchForge
  would have created it), symlink checks (on the managed directory and on the
  worktree itself), the main worktree, locked worktrees, missing directories,
  removing the worktree your shell is currently inside, and **running jobs**
  (see below).

The branch is **kept** when its worktree is removed; delete it yourself with
`git branch -d <branch>`.

Example `list` output:

```
BRANCH         KIND     STATE                SYNC  PATH
main           main     clean                -     /home/me/code/app
feature/login  managed  clean                -     /home/me/code/app.worktrees/feature%2Flogin
hotfix         managed  dirty (1 untracked)  -     /home/me/code/app.worktrees/hotfix
```

`SYNC` shows `+ahead/-behind vs upstream` when the branch tracks an upstream.
Exit code is `0` on success and `1` on any error (message on stderr).

## Running and supervising jobs

BranchForge can launch **any executable** (a test run, a build, a script, a
coding agent's CLI) inside a managed worktree and keep track of it after the
terminal is gone. It does not know or care what the program is: no API key, no
network, no AI-specific integration. This layer is plain process management.

### Commands

| Command | What it does |
|---|---|
| `branchforge run <branch> -- <program> [args...]` | Start `program` in the managed worktree for `<branch>` and return immediately. Everything after `--` is passed to the program as separate arguments; **no shell is involved**, so `*`, `$VAR`, `;` and `\|` reach the program literally. A relative program such as `./script.sh` is resolved inside the worktree. |
| `branchforge jobs` | Table of every job: `ID BRANCH STATE STARTED EXIT COMMAND` (times are UTC). |
| `branchforge logs <id> [--stdout] [--stderr]` | Print captured output. With no flag (or both) you get labelled `==> stdout <==` / `==> stderr <==` sections; with one flag you get that stream raw, ready to pipe. Works while the job is still running. |
| `branchforge stop <id> [--grace SECONDS]` | Ask the job to terminate and wait for the result (details below). |

```text
$ branchforge run feature/login -- sh -c 'echo built; echo warning >&2; exit 3'
Started job 1 on branch `feature/login`: sh -c 'echo built; echo warning >&2; exit 3'
  state: running
  logs:  branchforge logs 1
  stop:  branchforge stop 1

$ branchforge jobs
ID  BRANCH         STATE   STARTED               EXIT  COMMAND
1   feature/login  failed  2026-10-22 09:22:31Z  3     sh -c 'echo built; echo warning >&2; exit 3'

$ branchforge logs 1 --stderr
warning
```

`run` exits `0` once the job has *started*; whether the job later succeeds is
what `jobs` reports. If the program cannot be launched at all (missing file,
no execute permission), `run` fails with the OS error and the attempt is still
recorded as a `start-failed` job.

### Job ids, storage and logs

* Ids are small positive numbers (`1`, `2`, `3`...), allocated per repository.
  They are validated strictly (digits only, no leading zero, at most 9 digits)
  before they are ever used as a path, so `../1`, `01` or `1/..` are rejected.
* Everything is stored in the repository's Git directory, **not** in a worktree
  and not in `<repo>.worktrees/` (which holds only worktrees):

  ```text
  <repo>/.git/branchforge/jobs/<id>/
    meta.json        command, branch, worktree, timestamps, last recorded phase
    lock             held by the job's supervisor while it is alive
    stdout.log       the program's stdout
    stderr.log       the program's stderr
    supervisor.log   the supervisor's own diagnostics
    stop-requested   present once `stop` has been asked for
  <repo>/.git/branchforge.lock   repository-wide lock taken by `run` and `remove`
  ```

  Because the data lives in the Git directory, `git status` stays clean, all
  worktrees of the repository share one registry, and **removing a worktree
  never deletes its jobs' logs**.
* stdout and stderr are redirected by the operating system straight into the
  two log files. Nothing is held in memory and a chatty program can never block
  on a full pipe. Logs are opened in append mode and job directories are created
  atomically, so existing logs and metadata are never overwritten or reused.
  Directories are `0700` and files `0600`, since logs may contain secrets.
* There is **no log rotation or pruning**: logs grow as large as the program's
  output and jobs stay listed until you delete their directory by hand.

### Job lifecycle and status semantics

| State | Meaning |
|---|---|
| `starting` | Job created; its supervisor has not yet reported the program as running. If the supervisor process exits without recording anything, `run` marks the job `start-failed` immediately. |
| `running` | The supervisor is **alive** and the program was running when last recorded. |
| `succeeded` | The program exited with code 0. |
| `failed` | It exited non-zero, or was killed by a signal nobody asked for. `EXIT` shows the code or `signal N`. |
| `stopped` | It ended after `branchforge stop`. |
| `start-failed` | The program could not be launched. |
| `lost` | The record says it was running but its supervisor is gone and no result was written (the supervisor was killed, or the machine rebooted). **The program's real fate is unknown.** |
| `corrupt` | `meta.json` is missing, unreadable, from another job, or an unsupported version. `logs` still works. |

**How state is determined.** Each job has a tiny *supervisor* process (the same
binary, started as a hidden `__supervise` subcommand, detached from your
terminal in its own process group). It is the only writer of `meta.json`: it
records `starting`, then `running`, then the final result, each time by writing
a temporary file and renaming it into place so readers never see a half-written
file. It also holds an exclusive `flock` on the job's `lock` file for as long as
it lives.

Every read (`jobs`, `logs`, `stop`, `remove`) recomputes the state from the
recorded phase plus one question: *is that lock currently held?* Nothing about
liveness is stored, so there is nothing to go stale. A job is therefore
`running` only while a supervisor really holds the lock; **a PID existing, or
being written in a file, never makes a job look alive.** The kernel releases the
lock when its holder dies for any reason, which also removes the PID-reuse
problem. A job that is `starting` gets 15 seconds' grace before an absent
supervisor makes it `lost`. The one case that waits out that grace is a
supervisor **killed by a signal** while starting: it may have died after
launching the program, so BranchForge does not claim the start failed. (A
supervisor that merely *errors out* is recorded as `start-failed` at once.)

### Stopping a job

`branchforge stop <id>` never signals a process itself. It drops a
`stop-requested` file (containing the grace period, default 5 s) in the job
directory and waits. The job's supervisor, which owns the program it spawned,
notices the file and sends **SIGTERM to the program's process group** (so
children and grandchildren are included). If the program is still running after
the grace period it sends **SIGKILL**. `--grace 0` kills immediately, and a
second `stop` with a shorter `--grace` escalates a stop that is taking too long.
`stop` reports whether the job ended as `stopped` (including *how*: `SIGTERM`,
`SIGKILL`, or an exit code), or that it had already finished on its own, or that
the supervisor died while the stop was pending.

Consequences worth knowing:

* Only a **live supervisor** can act on a stop request, and it only ever signals
  the process group of a child it spawned and has not yet reaped, so a stale or
  forged PID in the metadata can never cause an unrelated process to be
  signalled. `stop` on a `lost` job is refused, with a note to look for a
  leftover process yourself.
* If a job's directory is deleted while it runs, its supervisor notices and
  kills the program rather than leaving something nobody can observe.
* Stragglers that moved themselves into a different process group (for example
  a daemonised grandchild) are outside the group that `stop` signals.

### Jobs and `remove`

`branchforge remove <branch>` **refuses while any job for that branch is
`starting` or `running`, and `--force` does not override this**: deleting a
directory under a live process would destroy its work. The error lists the job
ids; `branchforge stop <id>` them first. Two further rules:

* A `lost` job does **not** block removal (nothing can verify it is alive), but
  `remove` prints a warning naming it.
* A job with `corrupt` metadata whose supervisor is still alive blocks removal
  of *every* worktree, because BranchForge cannot tell which one it uses.
* If the job registry cannot be read (for example its directory is a symlink),
  `remove` fails closed.

**The `run`/`remove` race is closed by a lock, not just documented.** `run` and
`remove` both take a repository-wide lock (`.git/branchforge.lock`). `remove`
holds it from its first check until the worktree is deleted; `run` holds it
while it re-reads the worktree list and publishes the job record. A published
job counts as active immediately (`starting`), so whichever gets the lock
first, the other sees the result: if `run` wins, `remove` refuses; if `remove`
wins, `run` finds the worktree gone and registers nothing. Two tests hold the
lock by hand and interleave the other operation to prove both orderings.

What this does *not* cover: programs you start yourself in a worktree outside
BranchForge, a job whose supervisor was killed while `starting` and has aged
into `lost` (see above), and platforms where the lock is a no-op because jobs
are unsupported.

### Where jobs may run

Only in a worktree BranchForge manages: the branch must have a worktree at
exactly its managed path, not the main worktree, not a symlink, inside a plain
(non-symlink) managed directory, and the directory must exist. Branch names go
through the same validation as `new`. The supervisor **re-checks all of this
immediately before running anything**, so edited or stale metadata cannot make
it run a program in some other directory.

## Safety model

| Risk | Protection |
|---|---|
| Path traversal (`../evil`, `a/../b`) | Names are validated, then **encoded** into a single safe path component; `/` becomes `%2F`, so no separator can survive. |
| Ambiguous mapping (`a/b` vs `a-b`) | Encoding is injective (`%` is escaped too), so distinct branches get distinct directories. |
| Overwriting existing data | `new` refuses if the target path exists in any form (file, directory, dangling symlink). |
| Deleting the wrong directory | `remove` only proceeds if the worktree path is **exactly** `<repo>.worktrees/<encoded branch>`, the managed root is a plain directory (not a symlink), the entry isn't a symlink, and Git lists it as a worktree. `--force` never bypasses this. |
| Losing uncommitted work | Dirty means any staged/unstaged change **or untracked file**. `remove` refuses unless `--force`, and reports what was discarded. (`--force` is the only way past this check and the ignored-file check below.) |
| Losing git-ignored files (`.env`, `node_modules/`, build output) | Git deletes these silently, so `remove` asks Git for ignored entries (`status --ignored`) and refuses unless `--force`, naming example paths. `list`/`status` still call a worktree `clean` if only ignored files remain. |
| Other refusals | Main worktree, locked worktrees, missing directories, and removing the worktree you are standing in. |
| Running programs in the wrong place | `run` requires a managed, non-symlinked, existing worktree; the supervisor re-validates it right before spawning. |
| Signalling an unrelated process | The CLI never signals. Only a supervisor holding the job lock signals the process group of its own unreaped child; group ids 0 and 1 are refused in code. |
| Believing a stale PID | Liveness is "is the supervisor lock held?", never "does this PID exist?". |
| Deleting a worktree under a running job | `remove` refuses while a job is active, even with `--force`; unreadable registries fail closed; a registry lock serialises `run` against `remove` so a job cannot slip in between the check and the deletion. |
| Job id / data-directory tricks | Ids are canonical numbers; symlinked data directories and job directories are refused and never followed. |
| Argument injection | Git is run via `std::process::Command` with arguments passed separately (no shell); `--` precedes user-controlled values; option-like names/bases are rejected. |
| Deletion mechanism | BranchForge never calls `fs::remove_*`; deletion is `git worktree remove`, which itself only touches registered worktrees. |

## Architecture

```
src/
├── main.rs        parse args, call lib, print errors (+ cause chain), set exit code
├── lib.rs         `run(cli)`: discover repo, dispatch to a command
├── cli.rs         clap derive definitions (Cli, Commands)
├── error.rs       typed `Error` enum (thiserror) and `Result` alias
├── git.rs         `Git`: safe subprocess wrapper (run / output / succeeds)
├── worktree.rs    `Worktree` + parser for `git worktree list --porcelain`
├── state.rs       `WorktreeState` + parser for `git status --porcelain=v2 --branch`
├── layout.rs      managed-directory rules: branch->path encoding, `is_managed`
├── repo.rs        `Repo::discover`; `require_managed` ownership check shared by remove/run
├── inspect.rs     builds `Entry` (worktree + kind + health) for list/status
├── process.rs     the only OS-specific/`unsafe` code: flock, process groups, group signals
├── jobs.rs        job registry: metadata types, state classification, atomic store
├── supervisor.rs  the detached per-job supervisor (`__supervise`)
├── render.rs      pure table/summary formatting
└── commands/      new, list, status, remove, run, jobs, logs, stop
tests/cli.rs       integration tests for worktree management
tests/jobs.rs      integration tests for jobs (Unix)
tests/common/      shared fixture: temp Git repos, isolated Git config, wait helpers
```

Data flows one way: `git` output -> parsers (`worktree`, `state`) -> `Repo` /
`inspect` -> `render` -> stdout. The job layer sits beside it: `commands::run`
creates a record in `jobs`, starts a `supervisor`, and everything else only
reads the registry. Parsers and rendering are pure functions, so
they are unit-tested without touching Git. Everything else is covered by
integration tests that run the real binary in temp repos (with the developer's
global Git config disabled for reproducibility).

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Limitations

* **Local branches only.** `new foo` does not set up tracking from `origin/foo`;
  use `--base origin/foo` to start from a remote branch.
* **Branch is never deleted** on `remove`, and `remove` does not check whether
  the branch is merged or pushed.
* **Not managed = untouchable.** Worktrees created with plain `git worktree add`
  (or by older tools) show as `external` and are never removed. Mismatched
  names inside `<repo>.worktrees/` are treated the same way.
* **Ignored content counts entries, not files:** Git reports a wholly ignored
  directory (e.g. `node_modules/`) as one entry. Anything ignored, including
  files ignored by your global gitignore (such as `.DS_Store` inside a
  worktree), makes `remove` require `--force`. Empty ignored directories are
  not reported by Git and are not considered.
* **Unsafe managed directory.** If `<repo>.worktrees` is a symlink or not a
  directory, `status` prints an `UNSAFE` note and still lists worktrees, but
  nothing under it is reported as `managed` (it shows as `external`), `new`
  refuses to use it, and `remove` refuses with an explicit error, even with
  `--force`. BranchForge never follows or deletes the symlink.
* **`.DS_Store`:** a regular file with exactly that name in the managed
  directory is not reported as a stray entry. Nothing else is hidden.
* **Bare repositories are unsupported.**
* **Worktree paths containing newlines** are not supported (the parser uses the
  line-based porcelain format so older Git versions work).
* **Non-UTF-8 output** from Git (e.g. non-UTF-8 paths) is reported as an error.
* **Windows is untested.** Reserved device names (`CON`, `NUL`, ...) are not
  special-cased, and the symlink-related tests only run on Unix.
* **Case-insensitive filesystems:** `Feature` and `feature` map to directories
  differing only in case; the second `new` is refused as "already exists"
  rather than silently sharing a directory.
* **Race conditions:** checks and the deletion are not atomic; another process
  modifying the worktree between the dirty check and removal is only protected
  by Git's own checks.
* **Jobs: platform.** Unix only (`flock`, process groups). On Windows they report
  "unsupported"; this has been type-checked, never run.
* **Jobs: a dead supervisor orphans its program.** If a supervisor is killed
  (for example `kill -9`, OOM) the job becomes `lost` and its program, which
  keeps running, can no longer be stopped by BranchForge. Supervisors run in
  their own process group: SIGINT and SIGHUP delivered to the terminal's
  foreground group (what Ctrl-C and a terminal hang-up send) were verified not
  to reach them. A real closed terminal window was not tested, and a reboot or
  `kill` of the supervisor itself does end the job's supervision.
* **Jobs: unreadable entries.** A job directory that cannot be read at all (for
  example a permission error) appears in `jobs` as a `corrupt` row without
  hiding the others, but `remove` treats it as "cannot prove nothing is running"
  and refuses (fails closed). Metadata (including the program and arguments) is
  trusted as written by you: only the worktree is re-validated before running.
* **Jobs: signalling scope.** `stop` signals one process group. Descendants that
  changed group are not reached.
* **Jobs: no `logs --follow`, log size limit, rotation, pruning or filtering by
  branch.** Output goes to your terminal unfiltered, including any control
  sequences the program printed.
* **Jobs: environment.** The program inherits the environment of the `run`
  command; PATH lookup uses that environment, not the worktree's.
* **Jobs: local clock.** Timestamps are UTC; a clock set backwards only affects
  the 15-second startup grace.
* **Out of scope for this milestone:** environment files/setup hooks, port
  allocation, MCP integration, a dashboard, containers, agent-specific
  integrations, configuration files, and pruning stale worktrees or old jobs.

## Design notes for discussion

* **Why a supervisor process instead of just detaching the child?** After the CLI
  exits, someone must `wait()` for the program to learn its exit status, tell a
  stop apart from a crash, and own the right to signal it. A tiny dedicated
  process per job does exactly that, and nothing else.
* **Why `flock` for liveness instead of a PID file?** A PID can be reused, and a
  file can outlive its writer. A lock held by the kernel on behalf of a live
  process cannot be forged by stale data and disappears when the process does.
* **Why does `stop` write a file instead of sending a signal?** The CLI would
  have to trust a PID from disk. With a request file, only the process that
  *owns* the child ever signals, and only its own unreaped child's group.
* **Why files for logs instead of pipes?** The kernel does the copying; there is
  no reader thread, no buffering policy and no deadlock when stdout and stderr
  are both busy. The trade-off is no built-in rotation.
* **Why derive state on read instead of persisting `lost`?** Persisting it would
  add a second writer and a race. Recomputing from one authoritative record plus
  one liveness probe keeps a single writer and nothing that can go stale.
* **Why keep job data in `.git/`?** It must survive worktree removal, must not
  appear in `git status`, and must not break the "`<repo>.worktrees/` holds only
  worktrees" invariant that the safety checks rely on.
