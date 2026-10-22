//! End-to-end tests: run the real `branchforge` binary against temporary Git
//! repositories. Git is isolated from the developer's global/system config.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn isolate(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
}

/// Run git in `dir`, panicking with stderr on failure. Returns trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
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

struct Fixture {
    _tmp: TempDir,
    /// Canonicalized temp root (macOS tmp dirs are behind a symlink).
    root: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
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

    fn managed_root(&self) -> PathBuf {
        self.root.join("repo.worktrees")
    }

    fn wt_path(&self, encoded: &str) -> PathBuf {
        self.managed_root().join(encoded)
    }

    fn bf_in(&self, cwd: &Path, args: &[&str]) -> Output {
        isolate(
            Command::new(env!("CARGO_BIN_EXE_branchforge"))
                .current_dir(cwd)
                .args(args),
        )
        .output()
        .unwrap()
    }

    fn bf(&self, args: &[&str]) -> Output {
        self.bf_in(&self.repo, args)
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
#[track_caller]
fn assert_ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "expected success, stderr: {}",
        stderr(out)
    );
    stdout(out)
}
#[track_caller]
fn assert_fails(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "expected failure, stdout: {}",
        stdout(out)
    );
    stderr(out)
}

// ---------------------------------------------------------------- new

#[test]
fn new_creates_branch_and_worktree_in_sibling_directory() {
    let f = Fixture::new();
    let out = f.bf(&["new", "feature"]);
    let text = assert_ok(&out);

    let path = f.wt_path("feature");
    assert!(path.join("README.md").is_file());
    assert!(text.contains(path.to_str().unwrap()), "stdout was: {text}");
    assert_eq!(
        git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feature"
    );
    assert_eq!(
        git(&f.repo, &["rev-parse", "HEAD"]),
        git(&path, &["rev-parse", "HEAD"]),
        "new branch should start at HEAD"
    );
}

#[test]
fn new_with_base_starts_from_that_ref() {
    let f = Fixture::new();
    let first = git(&f.repo, &["rev-parse", "HEAD"]);
    git(&f.repo, &["tag", "v1"]);
    fs::write(f.repo.join("second.txt"), "2\n").unwrap();
    git(&f.repo, &["add", "second.txt"]);
    git(&f.repo, &["commit", "-q", "-m", "second"]);

    assert_ok(&f.bf(&["new", "hotfix", "--base", "v1"]));
    let path = f.wt_path("hotfix");
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), first);
    assert!(!path.join("second.txt").exists());
}

#[test]
fn new_rejects_unknown_base_and_creates_nothing() {
    let f = Fixture::new();
    let err = assert_fails(&f.bf(&["new", "x", "--base", "does-not-exist"]));
    assert!(err.contains("not a valid commit-ish"), "stderr: {err}");
    assert!(!f.managed_root().exists());
    assert!(git(&f.repo, &["branch", "--list", "x"]).is_empty());
}

#[test]
fn new_rejects_option_like_base() {
    let f = Fixture::new();
    let err = assert_fails(&f.bf(&["new", "x", "--base=--output=/tmp/pwned"]));
    assert!(err.contains("not a valid commit-ish"), "stderr: {err}");
}

#[test]
fn new_reuses_existing_branch_but_refuses_base_with_it() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "existing"]);

    let err = assert_fails(&f.bf(&["new", "existing", "--base", "main"]));
    assert!(err.contains("already exists"), "stderr: {err}");
    assert!(!f.wt_path("existing").exists());

    let text = assert_ok(&f.bf(&["new", "existing"]));
    assert!(text.contains("existing branch"), "stdout: {text}");
    assert!(f.wt_path("existing").is_dir());
}

#[test]
fn new_refuses_branch_already_checked_out() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "feature"]));
    let err = assert_fails(&f.bf(&["new", "feature"]));
    assert!(err.contains("already checked out"), "stderr: {err}");

    let err = assert_fails(&f.bf(&["new", "main"]));
    assert!(err.contains("already checked out"), "stderr: {err}");
}

#[test]
fn slashes_and_lookalike_names_map_to_distinct_directories() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "feature/login"]));
    assert_ok(&f.bf(&["new", "feature-login"]));
    assert_ok(&f.bf(&["new", "feature%2Flogin"]));

    assert!(f.wt_path("feature%2Flogin").is_dir());
    assert!(f.wt_path("feature-login").is_dir());
    assert!(f.wt_path("feature%252Flogin").is_dir());
    assert_eq!(
        git(
            &f.wt_path("feature%2Flogin"),
            &["rev-parse", "--abbrev-ref", "HEAD"]
        ),
        "feature/login"
    );
    assert_eq!(
        git(
            &f.wt_path("feature-login"),
            &["rev-parse", "--abbrev-ref", "HEAD"]
        ),
        "feature-login"
    );
}

#[test]
fn new_rejects_dangerous_branch_names_without_creating_anything() {
    let f = Fixture::new();
    for bad in [
        "../evil",
        "a/../../evil",
        "..",
        "a..b",
        "/abs",
        "trailing/",
        "with space",
        "x.lock",
        "a~b",
        "a:b",
    ] {
        let out = f.bf(&["new", bad]);
        assert!(!out.status.success(), "{bad:?} should be rejected");
    }
    // Option-like names are refused by our validation (clap needs `--` to accept them).
    let out = f.bf(&["new", "--", "-dash"]);
    assert!(!out.status.success());

    assert!(
        !f.managed_root().exists(),
        "nothing should have been created"
    );
    assert!(!f.root.join("evil").exists());
    assert!(!f.root.join("repo.worktrees").exists());
}

#[test]
fn new_refuses_to_overwrite_an_existing_path() {
    let f = Fixture::new();
    fs::create_dir_all(f.wt_path("feature")).unwrap();
    fs::write(f.wt_path("feature").join("precious.txt"), "keep me").unwrap();

    let err = assert_fails(&f.bf(&["new", "feature"]));
    assert!(err.contains("already exists"), "stderr: {err}");
    assert_eq!(
        fs::read_to_string(f.wt_path("feature").join("precious.txt")).unwrap(),
        "keep me"
    );
    assert!(git(&f.repo, &["branch", "--list", "feature"]).is_empty());
}

#[cfg(unix)]
#[test]
fn new_refuses_a_symlinked_managed_root() {
    let f = Fixture::new();
    let elsewhere = f.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, f.managed_root()).unwrap();

    let err = assert_fails(&f.bf(&["new", "feature"]));
    assert!(err.contains("not a plain directory"), "stderr: {err}");
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn commands_fail_cleanly_outside_a_repository() {
    let f = Fixture::new();
    let outside = f.root.join("not-a-repo");
    fs::create_dir(&outside).unwrap();
    for args in [&["list"][..], &["status"], &["new", "x"], &["remove", "x"]] {
        let err = assert_fails(&f.bf_in(&outside, args));
        assert!(
            err.contains("not inside a Git repository"),
            "{args:?}: {err}"
        );
    }
}

// ---------------------------------------------------------------- list / status

#[test]
fn list_shows_branch_kind_state_and_path() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "clean-one"]));
    assert_ok(&f.bf(&["new", "dirty-one"]));
    fs::write(f.wt_path("dirty-one").join("scratch.txt"), "x").unwrap();
    fs::write(f.wt_path("dirty-one").join("README.md"), "changed\n").unwrap();

    let text = assert_ok(&f.bf(&["list"]));
    let line_for = |branch: &str| {
        text.lines()
            .find(|l| l.starts_with(branch))
            .unwrap_or_else(|| panic!("no line for {branch} in:\n{text}"))
            .to_string()
    };

    let main = line_for("main");
    assert!(
        main.contains("main") && main.contains("clean") && main.contains(f.repo.to_str().unwrap())
    );
    let clean = line_for("clean-one");
    assert!(
        clean.contains("managed") && clean.contains("clean"),
        "{clean}"
    );
    let dirty = line_for("dirty-one");
    assert!(
        dirty.contains("managed") && dirty.contains("1 modified") && dirty.contains("1 untracked"),
        "{dirty}"
    );
    assert!(text.lines().next().unwrap().starts_with("BRANCH"));
}

#[test]
fn list_marks_worktrees_outside_the_managed_dir_as_external() {
    let f = Fixture::new();
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

    let text = assert_ok(&f.bf(&["list"]));
    let line = text.lines().find(|l| l.starts_with("manual")).unwrap();
    assert!(line.contains("external"), "{line}");
}

#[test]
fn list_reports_missing_directories_without_failing() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "gone"]));
    fs::remove_dir_all(f.wt_path("gone")).unwrap();

    let text = assert_ok(&f.bf(&["list"]));
    let line = text.lines().find(|l| l.starts_with("gone")).unwrap();
    assert!(line.contains("missing"), "{line}");
}

#[test]
fn status_summarizes_repo_and_managed_worktrees() {
    let f = Fixture::new();
    let before = assert_ok(&f.bf(&["status"]));
    assert!(before.contains("Repository   repo"), "{before}");
    assert!(before.contains("not created yet"), "{before}");
    assert!(before.contains("No managed worktrees"), "{before}");

    assert_ok(&f.bf(&["new", "a"]));
    assert_ok(&f.bf(&["new", "b"]));
    fs::write(f.wt_path("b").join("new.txt"), "x").unwrap();

    let after = assert_ok(&f.bf(&["status"]));
    assert!(
        after.contains("3 total: 1 main, 2 managed, 0 external"),
        "{after}"
    );
    assert!(after.contains("1 dirty, 0 missing, 0 locked"), "{after}");
    assert!(after.contains("Managed worktrees:"), "{after}");
    assert!(
        after.contains(f.managed_root().to_str().unwrap()),
        "{after}"
    );
}

#[test]
fn status_works_from_inside_a_linked_worktree() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "a"]));
    let text = assert_ok(&f.bf_in(&f.wt_path("a"), &["status"]));
    assert!(
        text.contains(&format!("Root         {}", f.repo.display())),
        "{text}"
    );
    assert!(text.contains("1 managed"), "{text}");
}

#[test]
fn status_reports_but_never_touches_unregistered_entries() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "a"]));
    let stray = f.wt_path("leftover");
    fs::create_dir(&stray).unwrap();
    fs::write(stray.join("file.txt"), "data").unwrap();

    let text = assert_ok(&f.bf(&["status"]));
    assert!(text.contains(stray.to_str().unwrap()), "{text}");
    assert!(text.contains("never deletes"), "{text}");
    assert!(stray.join("file.txt").is_file());
}

// ---------------------------------------------------------------- remove

#[test]
fn remove_deletes_clean_managed_worktree_and_keeps_branch() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "feature/x"]));
    let path = f.wt_path("feature%2Fx");
    assert!(path.is_dir());

    let text = assert_ok(&f.bf(&["remove", "feature/x"]));
    assert!(text.contains("Removed worktree"), "{text}");
    assert!(!path.exists());
    assert!(
        !git(&f.repo, &["branch", "--list", "feature/x"]).is_empty(),
        "branch must be kept"
    );
    assert!(!git(&f.repo, &["worktree", "list", "--porcelain"]).contains("feature%2Fx"));
}

#[test]
fn remove_refuses_dirty_worktree_unless_forced() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "work"]));
    let path = f.wt_path("work");
    fs::write(path.join("unsaved.txt"), "important").unwrap();

    let err = assert_fails(&f.bf(&["remove", "work"]));
    assert!(
        err.contains("uncommitted changes")
            && err.contains("1 untracked")
            && err.contains("--force"),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(path.join("unsaved.txt")).unwrap(),
        "important"
    );

    let text = assert_ok(&f.bf(&["remove", "work", "--force"]));
    assert!(
        text.contains("discarded") && text.contains("1 untracked"),
        "{text}"
    );
    assert!(!path.exists());
}

#[test]
fn remove_refuses_modified_tracked_files_too() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "work"]));
    fs::write(f.wt_path("work").join("README.md"), "edited\n").unwrap();
    let err = assert_fails(&f.bf(&["remove", "work"]));
    assert!(err.contains("1 modified"), "{err}");
    assert!(f.wt_path("work").is_dir());
}

#[test]
fn remove_never_touches_non_managed_worktrees_even_with_force() {
    let f = Fixture::new();
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

    for args in [&["remove", "manual"][..], &["remove", "manual", "--force"]] {
        let err = assert_fails(&f.bf(args));
        assert!(err.contains("not managed by BranchForge"), "{err}");
        assert!(ext.join("README.md").is_file());
    }
}

#[test]
fn remove_refuses_a_worktree_in_the_managed_dir_with_a_mismatched_name() {
    let f = Fixture::new();
    fs::create_dir(f.managed_root()).unwrap();
    let odd = f.wt_path("totally-different-name");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "actual-branch",
            odd.to_str().unwrap(),
        ],
    );

    let err = assert_fails(&f.bf(&["remove", "actual-branch", "--force"]));
    assert!(err.contains("not managed by BranchForge"), "{err}");
    assert!(odd.is_dir());
}

#[test]
fn remove_never_removes_the_main_worktree() {
    let f = Fixture::new();
    for args in [&["remove", "main"][..], &["remove", "main", "--force"]] {
        let err = assert_fails(&f.bf(args));
        assert!(err.contains("main worktree"), "{err}");
    }
    assert!(f.repo.join("README.md").is_file());
}

#[test]
fn remove_unknown_branch_is_an_error() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "no-worktree"]);
    for name in ["nope", "no-worktree"] {
        let err = assert_fails(&f.bf(&["remove", name]));
        assert!(err.contains("no worktree has branch"), "{err}");
    }
}

#[test]
fn remove_refuses_locked_worktree() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "busy"]));
    git(
        &f.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "on usb",
            f.wt_path("busy").to_str().unwrap(),
        ],
    );

    let err = assert_fails(&f.bf(&["remove", "busy", "--force"]));
    assert!(err.contains("locked") && err.contains("on usb"), "{err}");
    assert!(f.wt_path("busy").is_dir());
}

#[test]
fn remove_refuses_when_current_directory_is_inside_the_worktree() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "here"]));
    let sub = f.wt_path("here").join("nested");
    fs::create_dir(&sub).unwrap();

    let err = assert_fails(&f.bf_in(&sub, &["remove", "here", "--force"]));
    assert!(err.contains("current directory is inside"), "{err}");
    assert!(sub.is_dir());
}

#[test]
fn remove_refuses_when_directory_is_missing() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "gone"]));
    fs::remove_dir_all(f.wt_path("gone")).unwrap();
    let err = assert_fails(&f.bf(&["remove", "gone"]));
    assert!(err.contains("missing on disk"), "{err}");
}

#[test]
fn remove_works_when_invoked_from_another_linked_worktree() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "a"]));
    assert_ok(&f.bf(&["new", "b"]));
    assert_ok(&f.bf_in(&f.wt_path("a"), &["remove", "b"]));
    assert!(!f.wt_path("b").exists());
    assert!(f.wt_path("a").is_dir());
}

// ------------------------------------------- remove: ignored content

/// Commit a `.gitignore` on `main` so every new worktree inherits it.
fn commit_gitignore(f: &Fixture, patterns: &str) {
    fs::write(f.repo.join(".gitignore"), patterns).unwrap();
    git(&f.repo, &["add", ".gitignore"]);
    git(&f.repo, &["commit", "-q", "-m", "add .gitignore"]);
}

#[test]
fn remove_refuses_ignored_file_without_force_then_force_deletes_it() {
    let f = Fixture::new();
    commit_gitignore(&f, ".env\n");
    assert_ok(&f.bf(&["new", "feat"]));
    let env = f.wt_path("feat").join(".env");
    fs::write(&env, "API_KEY=secret").unwrap();

    // Ignored files are invisible to the normal dirty check...
    let list = assert_ok(&f.bf(&["list"]));
    let line = list.lines().find(|l| l.starts_with("feat")).unwrap();
    assert!(
        line.contains("clean"),
        "list semantics must not change: {line}"
    );
    assert!(assert_ok(&f.bf(&["status"])).contains("0 dirty"));

    // ...but `remove` must still refuse, name the file, and leave it intact.
    let err = assert_fails(&f.bf(&["remove", "feat"]));
    assert!(
        err.contains("git-ignored") && err.contains(".env") && err.contains("--force"),
        "{err}"
    );
    assert_eq!(fs::read_to_string(&env).unwrap(), "API_KEY=secret");
    assert!(git(&f.repo, &["worktree", "list", "--porcelain"]).contains("feat"));

    let text = assert_ok(&f.bf(&["remove", "feat", "--force"]));
    assert!(
        text.contains("deleted 1 ignored item(s)") && text.contains(".env"),
        "{text}"
    );
    assert!(!f.wt_path("feat").exists());
}

#[test]
fn remove_refuses_ignored_directory_without_force() {
    let f = Fixture::new();
    commit_gitignore(&f, "node_modules/\n");
    assert_ok(&f.bf(&["new", "feat"]));
    let deep = f.wt_path("feat").join("node_modules/pkg/deep");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("a.js"), "module.exports = 1").unwrap();

    let err = assert_fails(&f.bf(&["remove", "feat"]));
    assert!(
        err.contains("node_modules/") && err.contains("1 git-ignored"),
        "{err}"
    );
    assert!(deep.join("a.js").is_file());

    assert_ok(&f.bf(&["remove", "feat", "--force"]));
    assert!(!f.wt_path("feat").exists());
}

#[test]
fn remove_detects_ignored_file_inside_a_tracked_directory() {
    let f = Fixture::new();
    fs::create_dir(f.repo.join("dist")).unwrap();
    fs::write(f.repo.join("dist/keep.txt"), "tracked").unwrap();
    git(&f.repo, &["add", "dist/keep.txt"]);
    commit_gitignore(&f, "dist/out.js\n");
    assert_ok(&f.bf(&["new", "feat"]));
    fs::write(f.wt_path("feat").join("dist/out.js"), "built").unwrap();

    let err = assert_fails(&f.bf(&["remove", "feat"]));
    assert!(err.contains("dist/out.js"), "{err}");
    assert!(f.wt_path("feat").join("dist/out.js").is_file());
}

#[test]
fn remove_still_works_without_force_when_nothing_is_ignored_or_dirty() {
    let f = Fixture::new();
    commit_gitignore(&f, ".env\nnode_modules/\n");
    assert_ok(&f.bf(&["new", "feat"]));
    assert_ok(&f.bf(&["remove", "feat"]));
    assert!(!f.wt_path("feat").exists());
}

#[test]
fn dirty_error_takes_precedence_and_ignored_is_reported_after_cleanup() {
    let f = Fixture::new();
    commit_gitignore(&f, ".env\n");
    assert_ok(&f.bf(&["new", "feat"]));
    let wt = f.wt_path("feat");
    fs::write(wt.join(".env"), "x").unwrap();
    fs::write(wt.join("scratch.txt"), "y").unwrap();

    let err = assert_fails(&f.bf(&["remove", "feat"]));
    assert!(
        err.contains("uncommitted changes") && err.contains("1 untracked"),
        "{err}"
    );
    fs::remove_file(wt.join("scratch.txt")).unwrap();
    let err = assert_fails(&f.bf(&["remove", "feat"]));
    assert!(err.contains("git-ignored"), "{err}");
    assert!(wt.join(".env").is_file());
}

#[test]
fn force_does_not_bypass_ownership_for_worktrees_with_ignored_content() {
    let f = Fixture::new();
    commit_gitignore(&f, ".env\n");
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
    fs::write(ext.join(".env"), "secret").unwrap();

    let err = assert_fails(&f.bf(&["remove", "manual", "--force"]));
    assert!(err.contains("not managed by BranchForge"), "{err}");
    assert_eq!(fs::read_to_string(ext.join(".env")).unwrap(), "secret");
}

#[test]
fn force_does_not_bypass_locked_or_current_directory_with_ignored_content() {
    let f = Fixture::new();
    commit_gitignore(&f, ".env\n");
    assert_ok(&f.bf(&["new", "busy"]));
    assert_ok(&f.bf(&["new", "here"]));
    fs::write(f.wt_path("busy").join(".env"), "a").unwrap();
    fs::write(f.wt_path("here").join(".env"), "b").unwrap();
    git(
        &f.repo,
        &["worktree", "lock", f.wt_path("busy").to_str().unwrap()],
    );

    let err = assert_fails(&f.bf(&["remove", "busy", "--force"]));
    assert!(err.contains("locked"), "{err}");
    let err = assert_fails(&f.bf_in(&f.wt_path("here"), &["remove", "here", "--force"]));
    assert!(err.contains("current directory is inside"), "{err}");
    assert!(f.wt_path("busy").join(".env").is_file());
    assert!(f.wt_path("here").join(".env").is_file());
}

#[test]
fn untracked_files_are_detected_even_if_user_config_hides_them_from_status() {
    let f = Fixture::new();
    git(&f.repo, &["config", "status.showUntrackedFiles", "no"]);
    assert_ok(&f.bf(&["new", "feat"]));
    fs::write(f.wt_path("feat").join("notes.txt"), "mine").unwrap();

    let err = assert_fails(&f.bf(&["remove", "feat"]));
    assert!(err.contains("1 untracked"), "{err}");
    assert!(f.wt_path("feat").join("notes.txt").is_file());
}

// ------------------------------------------------- new: no commits

#[test]
fn new_in_a_repository_without_commits_explains_how_to_fix_it() {
    let f = Fixture::new();
    let empty = f.root.join("empty");
    fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "-q", "-b", "main"]);

    for args in [&["new", "x"][..], &["new", "x", "--base", "HEAD"]] {
        let err = assert_fails(&f.bf_in(&empty, args));
        assert!(
            err.contains("no commits yet") && err.contains("git commit --allow-empty"),
            "{err}"
        );
    }
    // A bogus explicit base is still reported as a bad base, not as "no commits".
    let err = assert_fails(&f.bf_in(&empty, &["new", "x", "--base", "nope"]));
    assert!(err.contains("not a valid commit-ish"), "{err}");
    assert!(!f.root.join("empty.worktrees").exists());
    assert!(git(&empty, &["branch", "--list", "x"]).is_empty());
}

// ------------------------------------- status: symlinks and .DS_Store

#[cfg(unix)]
#[test]
fn status_survives_a_symlinked_managed_root_and_never_follows_it() {
    let f = Fixture::new();
    let elsewhere = f.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join("keep.txt"), "mine").unwrap();
    std::os::unix::fs::symlink(&elsewhere, f.managed_root()).unwrap();

    let text = assert_ok(&f.bf(&["status"]));
    assert!(text.contains("UNSAFE"), "{text}");
    assert!(
        text.contains("Repository   repo") && text.contains("1 total: 1 main"),
        "{text}"
    );
    assert!(
        !text.contains("keep.txt"),
        "must not list through the symlink: {text}"
    );
    assert!(
        fs::symlink_metadata(f.managed_root())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(elsewhere.join("keep.txt")).unwrap(),
        "mine"
    );
    assert_ok(&f.bf(&["list"]));
}

#[cfg(unix)]
#[test]
fn symlinked_managed_root_is_reported_consistently_and_blocks_removal() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "a"]));

    // Control: with a normal managed root the worktree is `managed`.
    let list = assert_ok(&f.bf(&["list"]));
    assert!(
        list.lines()
            .any(|l| l.starts_with('a') && l.contains("managed")),
        "{list}"
    );
    let status = assert_ok(&f.bf(&["status"]));
    assert!(
        status.contains("1 main, 1 managed, 0 external") && !status.contains("UNSAFE"),
        "{status}"
    );

    // Swap the managed root for a symlink to the same contents.
    let moved = f.root.join("moved");
    fs::rename(f.managed_root(), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, f.managed_root()).unwrap();

    // list/status now agree: nothing under an unsafe root is "managed".
    let list = assert_ok(&f.bf(&["list"]));
    let line = list.lines().find(|l| l.starts_with('a')).unwrap();
    assert!(
        line.contains("external") && !line.contains("managed"),
        "{line}"
    );
    let status = assert_ok(&f.bf(&["status"]));
    assert!(
        status.contains("UNSAFE") && status.contains("1 main, 0 managed, 1 external"),
        "{status}"
    );

    // remove still gives the specific root error, with and without --force.
    for args in [&["remove", "a"][..], &["remove", "a", "--force"]] {
        let err = assert_fails(&f.bf(args));
        assert!(err.contains("not a plain directory"), "{err}");
    }
    assert!(moved.join("a/README.md").is_file());
    assert!(
        fs::symlink_metadata(f.managed_root())
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn remove_with_force_refuses_when_managed_root_became_a_symlink() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "a"]));
    let moved = f.root.join("moved");
    fs::rename(f.managed_root(), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, f.managed_root()).unwrap();

    let err = assert_fails(&f.bf(&["remove", "a", "--force"]));
    assert!(err.contains("not a plain directory"), "{err}");
    assert!(moved.join("a/README.md").is_file());
}

#[cfg(unix)]
#[test]
fn symlinked_worktree_path_is_flagged_by_status_and_never_removed() {
    let f = Fixture::new();
    commit_gitignore(&f, ".env\n");
    assert_ok(&f.bf(&["new", "a"]));
    let real = f.root.join("real");
    fs::rename(f.wt_path("a"), &real).unwrap();
    fs::write(real.join(".env"), "secret").unwrap();
    std::os::unix::fs::symlink(&real, f.wt_path("a")).unwrap();

    let text = assert_ok(&f.bf(&["status"]));
    assert!(text.contains("worktree paths that are symlinks"), "{text}");

    for args in [&["remove", "a"][..], &["remove", "a", "--force"]] {
        let err = assert_fails(&f.bf(args));
        assert!(err.contains("not managed by BranchForge"), "{err}");
    }
    assert_eq!(fs::read_to_string(real.join(".env")).unwrap(), "secret");
    assert!(
        fs::symlink_metadata(f.wt_path("a"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn status_marks_stray_symlink_entries_and_does_not_follow_them() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "a"]));
    let elsewhere = f.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join("keep.txt"), "mine").unwrap();
    std::os::unix::fs::symlink(&elsewhere, f.wt_path("link")).unwrap();

    let text = assert_ok(&f.bf(&["status"]));
    let line = text
        .lines()
        .find(|l| l.contains("/link"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.ends_with("(symlink)"), "{line}");
    assert!(!text.contains("keep.txt"));
    assert!(elsewhere.join("keep.txt").is_file());
}

#[test]
fn status_ignores_a_lone_ds_store_but_still_reports_other_entries() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "a"]));
    fs::write(f.wt_path(".DS_Store"), "finder").unwrap();
    let text = assert_ok(&f.bf(&["status"]));
    assert!(
        !text.contains("Warning"),
        "a lone .DS_Store must not warn: {text}"
    );

    fs::write(f.wt_path("notes.txt"), "mine").unwrap();
    fs::create_dir(f.wt_path("stray-dir")).unwrap();
    let text = assert_ok(&f.bf(&["status"]));
    assert!(
        text.contains("Warning") && text.contains("notes.txt") && text.contains("stray-dir"),
        "{text}"
    );
    assert!(!text.contains(".DS_Store"), "{text}");
    assert!(f.wt_path(".DS_Store").is_file(), "never deleted");
}

#[test]
fn status_still_reports_a_directory_named_ds_store() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "a"]));
    fs::create_dir(f.wt_path(".DS_Store")).unwrap();
    let text = assert_ok(&f.bf(&["status"]));
    assert!(text.contains(".DS_Store"), "{text}");
}

#[test]
fn full_lifecycle() {
    let f = Fixture::new();
    assert_ok(&f.bf(&["new", "feature/one", "--base", "main"]));
    assert!(assert_ok(&f.bf(&["list"])).contains("feature/one"));
    assert_ok(&f.bf(&["remove", "feature/one"]));
    assert!(!assert_ok(&f.bf(&["list"])).contains("feature/one"));
    // Recreating after removal works (branch already exists, no --base).
    assert_ok(&f.bf(&["new", "feature/one"]));
}
