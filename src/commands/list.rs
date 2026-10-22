use crate::error::Result;
use crate::inspect::inspect;
use crate::render::render_table;
use crate::repo::Repo;

pub fn run(repo: &Repo) -> Result<()> {
    let entries = inspect(repo);
    print!("{}", render_table(&entries));
    Ok(())
}
