use std::process::ExitCode;

use crate::core::error::Result;
use crate::core::project::{AddOptions, ProjectStore};
use crate::core::storage::Storage;

use super::project::resolve_work_path;

pub fn init(path: &str, copy: bool, force: bool) -> Result<ExitCode> {
    let work = resolve_work_path(path)?;
    let store = ProjectStore::open()?;
    let added = store.add_with(
        &work,
        None,
        AddOptions {
            copy,
            force,
            force_copy: false,
        },
    )?;
    if !added.created {
        println!(
            "Project {} is already initialized ({})",
            added.project.name,
            added.project.data_root.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    Storage::new(&added.project.data_root).init_board()?;
    println!(
        "Initialized project {} ({}) for {}",
        added.project.name,
        added.project.data_root.display(),
        added.project.work_path.display()
    );
    Ok(ExitCode::SUCCESS)
}
