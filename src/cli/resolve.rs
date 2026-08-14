//! Which project a command talks to (`docs/plans/TASK-163-projects-store.md` §4).

use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::core::error::{KanbanError, Result};
use crate::core::operations::Operations;
use crate::core::project::{PROJECT_ENV, Project, ProjectStore};

const NOT_INSIDE: &str = "not inside a kanban project; run `kanban init` or `kanban project add`";

/// A board a command may operate on.
pub enum Resolved {
    Project(Project),
    InPlace(PathBuf),
}

impl Resolved {
    pub fn operations(&self) -> Operations {
        match self {
            Resolved::Project(project) => Operations::for_project(project),
            Resolved::InPlace(path) => Operations::new(path),
        }
    }
}

/// TUI entry: a resolved board, or `None` to open the projects list.
pub fn resolve_tui(selector: Option<&str>) -> Result<Option<Resolved>> {
    match resolve_project(selector) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(KanbanError::Invalid(message)) if message == NOT_INSIDE => Ok(None),
        Err(err) => Err(err),
    }
}

/// Resolve the board for a board-scoped command.
///
/// Order: `--project` / `$KANBAN_PROJECT`, registered project at or above cwd,
/// silent adoption of an unregistered `<cwd>/.kanban`, else an error.
pub fn resolve_project(selector: Option<&str>) -> Result<Resolved> {
    let store = ProjectStore::open()?;
    if let Some(needle) = selector
        .map(str::trim)
        .filter(|needle| !needle.is_empty())
        .map(str::to_string)
        .or_else(|| env::var(PROJECT_ENV).ok().filter(|v| !v.trim().is_empty()))
    {
        return store
            .find(&needle)?
            .map(Resolved::Project)
            .ok_or_else(|| KanbanError::Invalid(format!("no such project: {needle}")));
    }

    let cwd = env::current_dir()?;
    if let Some(project) = registered_at_or_above(&store, &cwd)? {
        return Ok(Resolved::Project(project));
    }
    adopt_or_inplace(&store, &cwd)
}

fn registered_at_or_above(store: &ProjectStore, cwd: &Path) -> Result<Option<Project>> {
    for dir in cwd.ancestors() {
        if let Some(project) = store.resolve_from_cwd(dir)? {
            return Ok(Some(project));
        }
    }
    Ok(None)
}

fn adopt_or_inplace(store: &ProjectStore, cwd: &Path) -> Result<Resolved> {
    let local = cwd.join(".kanban");
    if !local.is_dir() {
        return Err(KanbanError::Invalid(NOT_INSIDE.into()));
    }
    match store.add(cwd, None) {
        Ok(added) => Ok(Resolved::Project(added.project)),
        Err(KanbanError::ActiveSessions(path)) => {
            warn(&format!(
                "warning: {} has active agent sessions; leaving it in place — \
                 it will move into the store once they finish",
                path.display()
            ));
            Ok(Resolved::InPlace(cwd.to_path_buf()))
        }
        Err(err) => {
            warn(&format!(
                "warning: could not move {} into the store ({err}); leaving it in place",
                local.display()
            ));
            Ok(Resolved::InPlace(cwd.to_path_buf()))
        }
    }
}

fn warn(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
}
