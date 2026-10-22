//! Where BranchForge-managed worktrees live, and how branch names map to paths.
//!
//! For a repository at `/work/app`, managed worktrees go in the sibling
//! directory `/work/app.worktrees/`. Each worktree is a *direct child* of that
//! directory named by [`encode_branch`].
//!
//! Safety properties:
//! * **No traversal**: the encoded name only contains `[A-Za-z0-9._-]` and `%XX`
//!   escapes, so it can never contain `/`, `\` or be `.`/`..`.
//! * **No ambiguity**: encoding is injective (`%` is itself escaped), so
//!   `feature/login` -> `feature%2Flogin` can never collide with `feature-login`.
//! * **Strict ownership**: a worktree counts as managed only if its path is
//!   exactly `<managed root>/<encode(branch)>` for the branch it has checked out.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::worktree::Worktree;

/// Filesystems commonly cap a path component at 255 bytes; stay well below.
const MAX_DIR_NAME_LEN: usize = 200;

#[derive(Debug, Clone)]
pub struct Layout {
    managed_root: PathBuf,
}

impl Layout {
    /// Derive the managed directory from the main worktree's path.
    pub fn for_main_worktree(main: &Path) -> Result<Self> {
        let unsupported = || Error::UnsupportedRepoPath(main.to_path_buf());
        let name = main.file_name().ok_or_else(unsupported)?;
        let parent = main.parent().ok_or_else(unsupported)?;
        let mut dir_name = name.to_os_string();
        dir_name.push(".worktrees");
        Ok(Self {
            managed_root: parent.join(dir_name),
        })
    }

    pub fn managed_root(&self) -> &Path {
        &self.managed_root
    }

    /// The only path BranchForge will ever create for `branch`.
    pub fn path_for(&self, branch: &str) -> Result<PathBuf> {
        validate_branch_syntax(branch)?;
        let dir = encode_branch(branch);
        if dir.len() > MAX_DIR_NAME_LEN {
            return Err(Error::InvalidBranchName {
                name: branch.to_string(),
                reason: format!(
                    "too long to map to a directory name ({} > {MAX_DIR_NAME_LEN})",
                    dir.len()
                ),
            });
        }
        Ok(self.managed_root.join(dir))
    }

    /// Pure path check: is `wt` at exactly the location BranchForge would have
    /// created for its branch, and is that entry itself not a symlink?
    ///
    /// This says nothing about whether the managed root is safe to use; callers
    /// that act on the worktree (`remove`) must also call [`Layout::check_root`]
    /// so they can report *why* the root is unsafe. Display code should use
    /// [`Layout::is_managed`] instead.
    pub fn owns_path(&self, wt: &Worktree) -> bool {
        let Some(branch) = wt.branch_name() else {
            return false;
        };
        let Ok(expected) = self.path_for(branch) else {
            return false;
        };
        if is_symlink(&wt.path) {
            return false;
        }
        normalize(&wt.path) == normalize(&expected)
    }

    /// [`Layout::owns_path`] **and** the managed root is safe to use: either it
    /// does not exist yet, or it is a plain directory. A symlinked or
    /// non-directory root (or one that can't be inspected) means nothing under
    /// it is reported as managed.
    pub fn is_managed(&self, wt: &Worktree) -> bool {
        self.owns_path(wt) && self.check_root().is_ok()
    }

    /// Check the managed root before touching anything inside it.
    /// Returns whether it exists; errors if it's a symlink or not a directory.
    pub fn check_root(&self) -> Result<bool> {
        match fs::symlink_metadata(&self.managed_root) {
            Ok(meta) if meta.is_dir() => Ok(true),
            Ok(_) => Err(Error::BadManagedRoot(self.managed_root.clone())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io {
                path: self.managed_root.clone(),
                source,
            }),
        }
    }

    /// Create the managed root if needed (after validating it).
    pub fn ensure_root(&self) -> Result<()> {
        if !self.check_root()? {
            fs::create_dir(&self.managed_root).map_err(|source| Error::Io {
                path: self.managed_root.clone(),
                source,
            })?;
        }
        Ok(())
    }

    /// Entries inside the managed root that Git does not know as worktrees.
    /// Read-only: BranchForge reports these but never deletes or follows them.
    ///
    /// The one exception to "report everything" is a *regular file* named exactly
    /// `.DS_Store` (macOS Finder metadata). A directory or symlink with that name
    /// is still reported.
    pub fn unregistered_entries(&self, worktrees: &[Worktree]) -> Result<Vec<StrayEntry>> {
        if !self.check_root()? {
            return Ok(Vec::new());
        }
        let known: Vec<PathBuf> = worktrees.iter().map(|w| normalize(&w.path)).collect();
        let io_err = |source| Error::Io {
            path: self.managed_root.clone(),
            source,
        };
        let mut found = Vec::new();
        for entry in fs::read_dir(&self.managed_root).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            let path = entry.path();
            // `DirEntry::file_type` does not follow symlinks.
            let file_type = entry.file_type().map_err(io_err)?;
            if file_type.is_file() && entry.file_name() == ".DS_Store" {
                continue;
            }
            if !known.contains(&normalize(&path)) {
                found.push(StrayEntry {
                    path,
                    symlink: file_type.is_symlink(),
                });
            }
        }
        found.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(found)
    }
}

/// An entry in the managed root that is not a registered worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrayEntry {
    pub path: PathBuf,
    /// The entry itself is a symlink (it is never followed).
    pub symlink: bool,
}

/// Cheap, git-independent checks. `git check-ref-format` runs on top of this.
pub fn validate_branch_syntax(name: &str) -> Result<()> {
    let reason = if name.is_empty() {
        "must not be empty"
    } else if name.starts_with('-') {
        "must not start with `-`"
    } else if name.starts_with('/') || name.ends_with('/') {
        "must not start or end with `/`"
    } else if name.chars().any(char::is_control) {
        "must not contain control characters"
    } else if name.contains("..") || name.split('/').any(|part| part == "." || part == "..") {
        "must not contain `..` or `.` path components"
    } else {
        return Ok(());
    };
    Err(Error::InvalidBranchName {
        name: name.to_string(),
        reason: reason.to_string(),
    })
}

/// Injective, filesystem-safe encoding of a branch name (see module docs).
pub fn encode_branch(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, byte) in name.bytes().enumerate() {
        let plain =
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || (byte == b'.' && i > 0);
        if plain {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Is the final component of `path` itself a symlink? (Never follows it.)
pub fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Canonicalize the parent directory (resolving e.g. macOS `/var` -> `/private/var`)
/// but not the final component, so a symlinked leaf is never silently followed.
/// Falls back to the lexical path if the parent doesn't exist.
fn normalize(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(name),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::for_main_worktree(Path::new("/nonexistent-bf/work/app")).unwrap()
    }

    fn wt(path: &str, branch: Option<&str>) -> Worktree {
        crate::worktree::parse_porcelain(&format!(
            "worktree {path}\n{}",
            branch
                .map(|b| format!("branch refs/heads/{b}\n"))
                .unwrap_or_default()
        ))
        .unwrap()
        .remove(0)
    }

    #[test]
    fn managed_root_is_a_sibling_named_after_the_repo() {
        assert_eq!(
            layout().managed_root(),
            Path::new("/nonexistent-bf/work/app.worktrees")
        );
    }

    #[test]
    fn root_directory_has_no_managed_dir() {
        assert!(matches!(
            Layout::for_main_worktree(Path::new("/")),
            Err(Error::UnsupportedRepoPath(_))
        ));
    }

    #[test]
    fn encoding_escapes_slashes_and_percent() {
        assert_eq!(encode_branch("feature/login"), "feature%2Flogin");
        assert_eq!(encode_branch("100%"), "100%25");
        assert_eq!(encode_branch("plain-name_1.2"), "plain-name_1.2");
        assert_eq!(encode_branch("naïve"), "na%C3%AFve");
        assert_eq!(encode_branch(".hidden"), "%2Ehidden");
    }

    #[test]
    fn encoding_is_injective_for_lookalikes() {
        let names = [
            "feature/login",
            "feature-login",
            "feature%2Flogin",
            "feature_login",
        ];
        let mut encoded: Vec<_> = names.iter().map(|n| encode_branch(n)).collect();
        encoded.sort();
        encoded.dedup();
        assert_eq!(encoded.len(), names.len());
    }

    #[test]
    fn encoded_names_never_contain_separators_or_dot_components() {
        for name in ["a/b", "../x", "..", ".", "a\\b", "x/../y", "C:evil"] {
            let e = encode_branch(name);
            assert!(!e.contains('/') && !e.contains('\\'), "{name} -> {e}");
            assert!(e != "." && e != "..", "{name} -> {e}");
        }
    }

    #[test]
    fn rejects_dangerous_branch_names() {
        for bad in [
            "",
            "-x",
            "--force",
            "/abs",
            "trail/",
            "a..b",
            "../evil",
            "a/./b",
            "a/../b",
            "tab\there",
            "nl\nx",
        ] {
            assert!(
                matches!(
                    validate_branch_syntax(bad),
                    Err(Error::InvalidBranchName { .. })
                ),
                "should reject {bad:?}"
            );
        }
        for good in ["main", "feature/login", "fix_1.2-rc", "naïve", "v1.0.0"] {
            assert!(
                validate_branch_syntax(good).is_ok(),
                "should accept {good:?}"
            );
        }
    }

    #[test]
    fn ds_store_file_is_ignored_but_lookalikes_and_other_entries_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        let root = tmp.path().join("app.worktrees");
        fs::create_dir(&root).unwrap();
        let layout = Layout::for_main_worktree(&app).unwrap();

        fs::write(root.join(".DS_Store"), "finder noise").unwrap();
        assert!(layout.unregistered_entries(&[]).unwrap().is_empty());

        fs::write(root.join("notes.txt"), "mine").unwrap();
        fs::write(root.join(".DS_Store.bak"), "x").unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        let names: Vec<_> = layout
            .unregistered_entries(&[])
            .unwrap()
            .iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [".DS_Store.bak", "dir", "notes.txt"]);
    }

    #[test]
    fn a_directory_named_ds_store_is_still_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("app.worktrees");
        fs::create_dir_all(root.join(".DS_Store")).unwrap();
        let layout = Layout::for_main_worktree(&tmp.path().join("app")).unwrap();
        assert_eq!(layout.unregistered_entries(&[]).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_entries_are_flagged_and_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("app.worktrees");
        fs::create_dir(&root).unwrap();
        let target = tmp.path().join("elsewhere");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, root.join("link")).unwrap();
        // A symlink *named* .DS_Store must not be hidden either.
        std::os::unix::fs::symlink(&target, root.join(".DS_Store")).unwrap();

        let layout = Layout::for_main_worktree(&tmp.path().join("app")).unwrap();
        let strays = layout.unregistered_entries(&[]).unwrap();
        assert_eq!(strays.len(), 2);
        assert!(strays.iter().all(|s| s.symlink));
    }

    #[test]
    fn overlong_names_are_rejected() {
        let long = "a".repeat(300);
        assert!(matches!(
            layout().path_for(&long),
            Err(Error::InvalidBranchName { .. })
        ));
    }

    #[test]
    fn path_for_is_a_direct_child_of_the_root() {
        let p = layout().path_for("feature/login").unwrap();
        assert_eq!(p.parent(), Some(layout().managed_root()));
        assert_eq!(p.file_name().unwrap(), "feature%2Flogin");
    }

    #[test]
    fn is_managed_matches_owns_path_for_a_plain_or_missing_managed_root() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::for_main_worktree(&tmp.path().join("app")).unwrap();
        let path = layout.path_for("feature/login").unwrap();
        let w = wt(path.to_str().unwrap(), Some("feature/login"));

        // Root does not exist yet: nothing is unsafe, so the path check decides.
        assert!(layout.owns_path(&w) && layout.is_managed(&w));

        // Root exists as a plain directory.
        fs::create_dir(layout.managed_root()).unwrap();
        assert!(layout.owns_path(&w) && layout.is_managed(&w));

        // A path that is not the expected one is never managed either way.
        let other = wt(
            layout.managed_root().join("other").to_str().unwrap(),
            Some("feature/login"),
        );
        assert!(!layout.owns_path(&other) && !layout.is_managed(&other));
    }

    #[test]
    fn a_managed_root_that_is_a_file_makes_nothing_managed() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::for_main_worktree(&tmp.path().join("app")).unwrap();
        fs::write(layout.managed_root(), "not a directory").unwrap();
        let path = layout.path_for("x").unwrap();
        let w = wt(path.to_str().unwrap(), Some("x"));

        assert!(layout.owns_path(&w), "the path check alone is unaffected");
        assert!(!layout.is_managed(&w));
        assert!(matches!(layout.check_root(), Err(Error::BadManagedRoot(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_managed_root_makes_nothing_managed_and_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        let layout = Layout::for_main_worktree(&tmp.path().join("app")).unwrap();
        std::os::unix::fs::symlink(&real, layout.managed_root()).unwrap();
        let path = layout.path_for("x").unwrap();
        let w = wt(path.to_str().unwrap(), Some("x"));

        // Path resolution alone would call this ours...
        assert!(layout.owns_path(&w));
        // ...but display/classification must not.
        assert!(!layout.is_managed(&w));
        assert!(matches!(layout.check_root(), Err(Error::BadManagedRoot(_))));
        // The symlink and its target are untouched.
        assert!(is_symlink(layout.managed_root()));
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
    }

    #[test]
    fn is_managed_requires_exact_location_for_the_checked_out_branch() {
        let l = layout();
        let root = l.managed_root().display().to_string();

        assert!(l.is_managed(&wt(
            &format!("{root}/feature%2Flogin"),
            Some("feature/login")
        )));

        // Right directory, wrong branch.
        assert!(!l.is_managed(&wt(&format!("{root}/feature%2Flogin"), Some("other"))));
        // Right branch, outside the managed root.
        assert!(!l.is_managed(&wt(
            "/nonexistent-bf/elsewhere/feature%2Flogin",
            Some("feature/login")
        )));
        // Nested one level deeper.
        assert!(!l.is_managed(&wt(
            &format!("{root}/sub/feature%2Flogin"),
            Some("feature/login")
        )));
        // The managed root itself.
        assert!(!l.is_managed(&wt(&root, Some("feature/login"))));
        // Detached HEAD has no branch to derive a path from.
        assert!(!l.is_managed(&wt(&format!("{root}/anything"), None)));
        // The main worktree is never managed.
        assert!(!l.is_managed(&wt("/nonexistent-bf/work/app", Some("main"))));
    }
}
