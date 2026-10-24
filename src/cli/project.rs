//! `kanban project` subcommands.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Subcommand;
use serde::Serialize;

use crate::core::error::{KanbanError, Result};
use crate::core::project::{AddOptions, Project, ProjectStore, normalize_path};
use crate::core::storage::Storage;
use crate::core::timefmt;

use super::resolve;

#[derive(Subcommand)]
pub enum ProjectCommand {
    /// List registered projects.
    List {
        #[arg(long = "format", value_parser = ["table", "json"], default_value = "table")]
        output_format: String,
    },
    /// Register a folder as a project (migrating a local .kanban if present).
    Add {
        /// Folder to register (default: current directory)
        path: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Copy the local .kanban into the store instead of moving it
        #[arg(long)]
        copy: bool,
        /// Move even when the board has active agent sessions
        #[arg(long)]
        force: bool,
    },
    /// Show one project.
    Show { id: String },
    /// Change a project's display name (the id stays the same).
    Rename { id: String, new_name: String },
    /// Repoint a project at another folder.
    #[command(name = "set-path")]
    SetPath { id: String, path: String },
    /// Print a project's work path and data root.
    Path {
        /// Project id, name, or path. Defaults to the current directory.
        id: Option<String>,
    },
    /// Unregister a project. `--purge` also moves the board data to the
    /// trash, where it stays restorable for `trash.retention_days` (7).
    Remove {
        id: String,
        #[arg(long)]
        purge: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Open the TUI on a project.
    Open { id: String },
    /// Projects removed with `--purge`, kept until the retention runs out.
    #[command(subcommand)]
    Trash(TrashCommand),
}

#[derive(Subcommand)]
pub enum TrashCommand {
    /// List trashed projects with their expiry.
    List {
        #[arg(long = "format", value_parser = ["table", "json"], default_value = "table")]
        output_format: String,
    },
    /// Put a trashed project back (entry name, or the old project id).
    Restore { entry: String },
    /// Delete one trashed project for good.
    Purge {
        entry: String,
        #[arg(long)]
        yes: bool,
    },
    /// Delete everything in the trash for good.
    Empty {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Serialize)]
struct TrashJson {
    entry: String,
    id: String,
    name: String,
    work_path: PathBuf,
    deleted_at: String,
    expires_at: String,
}

#[derive(Serialize)]
struct ProjectJson {
    id: String,
    name: String,
    work_path: PathBuf,
    data_root: PathBuf,
    created_at: String,
    last_opened_at: Option<String>,
    missing: bool,
}

impl From<&Project> for ProjectJson {
    fn from(project: &Project) -> Self {
        ProjectJson {
            id: project.id.clone(),
            name: project.name.clone(),
            work_path: project.work_path.clone(),
            data_root: project.data_root.clone(),
            created_at: timefmt::format(&project.created_at),
            last_opened_at: project.last_opened_at.as_ref().map(timefmt::format),
            missing: project.work_path_missing(),
        }
    }
}

pub fn run(command: ProjectCommand) -> Result<ExitCode> {
    let store = ProjectStore::open()?;
    match command {
        ProjectCommand::List { output_format } => list(&store, &output_format),
        ProjectCommand::Add {
            path,
            name,
            copy,
            force,
        } => add(&store, path.as_deref(), name.as_deref(), copy, force),
        ProjectCommand::Show { id } => show(&store, &id),
        ProjectCommand::Rename { id, new_name } => rename(&store, &id, &new_name),
        ProjectCommand::SetPath { id, path } => set_path(&store, &id, &path),
        ProjectCommand::Path { id } => print_path(&store, id.as_deref()),
        ProjectCommand::Remove { id, purge, yes } => remove(&store, &id, purge, yes),
        ProjectCommand::Open { id } => open(&store, &id),
        ProjectCommand::Trash(command) => trash(&store, command),
    }
}

fn trash(store: &ProjectStore, command: TrashCommand) -> Result<ExitCode> {
    store.prune_trash(timefmt::now())?;
    match command {
        TrashCommand::List { output_format } => {
            let items = store.trash_list()?;
            if output_format == "json" {
                let values: Vec<TrashJson> = items
                    .iter()
                    .map(|item| TrashJson {
                        entry: item.entry.clone(),
                        id: item.id.clone(),
                        name: item.name.clone(),
                        work_path: item.work_path.clone(),
                        deleted_at: timefmt::format(&item.deleted_at),
                        expires_at: timefmt::format(&item.expires_at),
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&values)
                        .map_err(|err| KanbanError::Invalid(err.to_string()))?
                );
                return Ok(ExitCode::SUCCESS);
            }
            if items.is_empty() {
                println!("Trash is empty.");
                return Ok(ExitCode::SUCCESS);
            }
            println!("{:<36} {:<24} {:<17} Expires", "Entry", "Name", "Deleted");
            println!("{}", "-".repeat(96));
            for item in items {
                println!(
                    "{:<36} {:<24} {:<17} {}",
                    item.entry,
                    item.name,
                    item.deleted_at.format("%Y-%m-%d %H:%M"),
                    item.expires_at.format("%Y-%m-%d %H:%M")
                );
            }
        }
        TrashCommand::Restore { entry } => {
            let entry = trash_entry(store, &entry)?;
            let project = store.restore_from_trash(&entry)?;
            println!("Restored project {} ({})", project.id, project.name);
        }
        TrashCommand::Purge { entry, yes } => {
            let entry = trash_entry(store, &entry)?;
            if !yes && !confirm(&format!("Delete {entry} from the trash for good? [y/N] "))? {
                println!("Cancelled.");
                return Ok(ExitCode::SUCCESS);
            }
            store.purge_from_trash(&entry)?;
            println!("Deleted {entry} for good");
        }
        TrashCommand::Empty { yes } => {
            if !yes && !confirm("Delete every project in the trash for good? [y/N] ")? {
                println!("Cancelled.");
                return Ok(ExitCode::SUCCESS);
            }
            let removed = store.empty_trash()?;
            println!("Deleted {removed} trashed project(s) for good");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// An exact entry name, else the newest entry whose old id or name matches.
fn trash_entry(store: &ProjectStore, needle: &str) -> Result<String> {
    let items = store.trash_list()?;
    let lowered = needle.to_lowercase();
    items
        .iter()
        .find(|item| item.entry == needle)
        .or_else(|| {
            items
                .iter()
                .find(|item| item.id == needle || item.name.to_lowercase() == lowered)
        })
        .map(|item| item.entry.clone())
        .ok_or_else(|| KanbanError::Invalid(format!("no such trash entry: {needle}")))
}

fn list(store: &ProjectStore, output_format: &str) -> Result<ExitCode> {
    let projects = store.list()?;
    if output_format == "json" {
        let values: Vec<ProjectJson> = projects.iter().map(ProjectJson::from).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&values)
                .map_err(|err| KanbanError::Invalid(err.to_string()))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    if projects.is_empty() {
        println!("No projects registered.");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{:<20} {:<24} Path", "ID", "Name");
    println!("{}", "-".repeat(64));
    for project in projects {
        let mut path = project.work_path.display().to_string();
        if project.work_path_missing() {
            path.push_str(" (missing)");
        }
        println!("{:<20} {:<24} {path}", project.id, project.name);
    }
    Ok(ExitCode::SUCCESS)
}

fn add(
    store: &ProjectStore,
    path: Option<&str>,
    name: Option<&str>,
    copy: bool,
    force: bool,
) -> Result<ExitCode> {
    let work = resolve_work_path(path.unwrap_or("."))?;
    let added = store.add_with(
        &work,
        name,
        AddOptions {
            copy,
            force,
            force_copy: false,
        },
    )?;
    if !added.created {
        println!(
            "Project {} is already registered ({})",
            added.project.name,
            added.project.data_root.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    Storage::new(&added.project.data_root).init_board()?;
    let verb = if added.restored { "Restored" } else { "Added" };
    println!(
        "{verb} project {} ({}) for {}",
        added.project.name,
        added.project.data_root.display(),
        added.project.work_path.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn show(store: &ProjectStore, needle: &str) -> Result<ExitCode> {
    let project = require(store, needle)?;
    println!("ID: {}", project.id);
    println!("Name: {}", project.name);
    println!("Work path: {}", project.work_path.display());
    println!("Data root: {}", project.data_root.display());
    println!("Created: {}", timefmt::format(&project.created_at));
    match project.last_opened_at {
        Some(opened) => println!("Last opened: {}", timefmt::format(&opened)),
        None => println!("Last opened:"),
    }
    if project.work_path_missing() {
        println!("Status: missing");
    }
    Ok(ExitCode::SUCCESS)
}

fn rename(store: &ProjectStore, needle: &str, new_name: &str) -> Result<ExitCode> {
    let project = require(store, needle)?;
    let renamed = store.rename(&project.id, new_name)?;
    // List display prefers `tui.name`; keep it in step with the registry.
    match crate::core::migrate::set_board_display_name(&renamed.data_root, &renamed.name) {
        Ok(()) => println!("Renamed project {} to {}", renamed.id, renamed.name),
        Err(err) => println!(
            "Renamed project {} to {}, but its settings kept the old name: {err}",
            renamed.id, renamed.name
        ),
    }
    Ok(ExitCode::SUCCESS)
}

fn set_path(store: &ProjectStore, needle: &str, path: &str) -> Result<ExitCode> {
    let project = require(store, needle)?;
    let work = resolve_work_path(path)?;
    let updated = store.set_path(&project.id, &work)?;
    println!(
        "Project {} now points at {}",
        updated.id,
        updated.work_path.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn print_path(store: &ProjectStore, needle: Option<&str>) -> Result<ExitCode> {
    let project = match needle {
        Some(needle) => require(store, needle)?,
        None => match resolve::resolve_project(None)? {
            resolve::Resolved::Project(project) => project,
            resolve::Resolved::InPlace(path) => {
                println!("{}", path.display());
                return Ok(ExitCode::SUCCESS);
            }
        },
    };
    println!("{}", project.work_path.display());
    Ok(ExitCode::SUCCESS)
}

fn remove(store: &ProjectStore, needle: &str, purge: bool, yes: bool) -> Result<ExitCode> {
    let project = require(store, needle)?;
    if !yes && !confirm_remove(&project.id, purge)? {
        println!("Cancelled.");
        return Ok(ExitCode::SUCCESS);
    }
    store.remove(&project.id, purge)?;
    if purge {
        println!(
            "Moved project {} to the trash; restore it within {} days with `kanban project trash restore {}`",
            project.id,
            store.trash_retention_days(),
            project.id
        );
    } else {
        println!(
            "Unregistered project {} (board data kept at {})",
            project.id,
            project.data_root.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn open(store: &ProjectStore, needle: &str) -> Result<ExitCode> {
    let project = require(store, needle)?;
    crate::tui::run_project(project)?;
    Ok(ExitCode::SUCCESS)
}

fn require(store: &ProjectStore, needle: &str) -> Result<Project> {
    store
        .find(needle)?
        .ok_or_else(|| KanbanError::Invalid(format!("no such project: {needle}")))
}

pub(super) fn resolve_work_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let normalized = normalize_path(&absolute);
    if !normalized.is_dir() {
        return Err(KanbanError::Invalid(format!(
            "not a directory: {}",
            normalized.display()
        )));
    }
    Ok(normalized)
}

fn confirm_remove(id: &str, purge: bool) -> Result<bool> {
    let prompt = if purge {
        format!("Move project {id} and its board data to the trash? [y/N] ")
    } else {
        format!("Unregister project {id}? Board data will be kept. [y/N] ")
    };
    confirm(&prompt)
}

fn confirm(prompt: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Err(KanbanError::Invalid(
            "refusing to delete without --yes".into(),
        ));
    }
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}
