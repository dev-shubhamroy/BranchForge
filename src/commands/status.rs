use crate::error::{Error, Result};
use crate::inspect::{Entry, Health, Kind, inspect};
use crate::layout::is_symlink;
use crate::render::render_table;
use crate::repo::Repo;

pub fn run(repo: &Repo) -> Result<()> {
    let entries = inspect(repo);
    let count = |kind: Kind| entries.iter().filter(|e| e.kind == kind).count();

    // A bad managed directory (e.g. a symlink) must not hide everything else:
    // report it as unsafe, never touch it, and carry on with the worktree info.
    let (root_note, root_usable) = match repo.layout.check_root() {
        Ok(true) => ("", true),
        Ok(false) => (" (not created yet)", false),
        Err(Error::BadManagedRoot(_)) => (
            " (UNSAFE: not a plain directory; BranchForge will not use it)",
            false,
        ),
        Err(e) => return Err(e),
    };

    println!("Repository   {}", repo.name());
    println!("Root         {}", repo.main().path.display());
    println!(
        "Managed dir  {}{root_note}",
        repo.layout.managed_root().display()
    );
    println!(
        "Worktrees    {} total: {} main, {} managed, {} external",
        entries.len(),
        count(Kind::Main),
        count(Kind::Managed),
        count(Kind::External)
    );

    let dirty = entries.iter().filter(|e| is_dirty(e)).count();
    let missing = entries
        .iter()
        .filter(|e| e.health == Health::Missing)
        .count();
    let locked = entries
        .iter()
        .filter(|e| e.worktree.locked.is_some())
        .count();
    println!("Attention    {dirty} dirty, {missing} missing, {locked} locked");

    let managed: Vec<Entry> = entries
        .iter()
        .filter(|e| e.kind == Kind::Managed)
        .cloned()
        .collect();
    println!();
    if managed.is_empty() {
        println!("No managed worktrees. Create one with `branchforge new <branch>`.");
    } else {
        println!("Managed worktrees:");
        print!("{}", render_table(&managed));
    }

    // Registered worktrees whose own path is a symlink are never "managed".
    let linked: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.kind != Kind::Main && is_symlink(&e.worktree.path))
        .collect();
    if !linked.is_empty() {
        println!();
        println!("Warning: worktree paths that are symlinks (treated as external, never removed):");
        for e in linked {
            println!("  {}", e.worktree.path.display());
        }
    }

    if root_usable {
        let stray = repo.layout.unregistered_entries(&repo.worktrees)?;
        if !stray.is_empty() {
            println!();
            println!(
                "Warning: entries in the managed directory that Git does not know as worktrees"
            );
            println!("(BranchForge never deletes or follows these; inspect them yourself):");
            for entry in stray {
                let note = if entry.symlink { " (symlink)" } else { "" };
                println!("  {}{note}", entry.path.display());
            }
        }
    }
    Ok(())
}

fn is_dirty(entry: &Entry) -> bool {
    matches!(&entry.health, Health::Ready(s) if s.is_dirty())
}
