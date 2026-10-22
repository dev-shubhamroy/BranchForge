# BranchForge

A small Rust CLI that manages Git worktrees in a predictable layout, and only
ever removes worktrees it can prove it created.

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
  and removing the worktree your shell is currently inside.

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
├── repo.rs        `Repo::discover`: worktree list + layout, from any worktree
├── inspect.rs     builds `Entry` (worktree + kind + health) for list/status
├── render.rs      pure table/summary formatting
└── commands/      new.rs, list.rs, remove.rs, status.rs
tests/cli.rs       integration tests against temporary Git repositories
```

Data flows one way: `git` output -> parsers (`worktree`, `state`) -> `Repo` /
`inspect` -> `render` -> stdout. Parsers and rendering are pure functions, so
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
* **Out of scope for this milestone:** environment files/setup hooks, port
  allocation, process supervision, MCP integration, configuration files, and
  pruning stale worktrees.
