//! `kanban backup` — snapshot, list, verify, restore, prune.
//!
//! The command is a thin wrapper. Snapshot contents, locks, retention and
//! restore live in [`crate::core::backup`].

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Subcommand;

use crate::core::backup::{self, Kind, RestoreMode};
use crate::core::error::{KanbanError, Result};
use crate::core::project::ProjectStore;

#[derive(Subcommand)]
pub enum BackupAction {
    /// Write a snapshot (core, and artifacts when asked).
    Create {
        /// Include agent artifacts (`assets/` outside `images/`).
        #[arg(long)]
        with_artifacts: bool,
        /// Backup directory.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// List snapshots and the retention bucket each one falls in.
    List {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Check hashes, task/thread/config parsing, and the sync format.
    Verify {
        /// Archive path, or `latest`.
        target: String,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Restore a verified snapshot. Prints what changes and asks first; `--yes`
    /// skips the question and is required when stdin is not a terminal.
    Restore {
        /// Archive path, or `latest` (newest core snapshot).
        target: String,
        /// Restore one project (board mode only).
        #[arg(long)]
        project: Option<String>,
        /// `board` keeps this machine's sync; `full` is disaster recovery.
        #[arg(long, value_parser = ["board", "full"], default_value = "board")]
        mode: String,
        /// Restore `node.yaml`, `peers.yaml` and `tokens.yaml` from the archive.
        #[arg(long)]
        identity: bool,
        /// Do not ask; required when stdin is not a terminal.
        #[arg(long)]
        yes: bool,
        /// Restore over active agent sessions, or full-restore over existing tasks.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Apply retention. `--dry-run` only prints.
    Prune {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

pub fn run(
    with_artifacts: bool,
    dir: Option<PathBuf>,
    action: Option<BackupAction>,
) -> Result<ExitCode> {
    match action {
        None => create(with_artifacts, dir),
        Some(BackupAction::Create {
            with_artifacts,
            dir,
        }) => create(with_artifacts, dir),
        Some(BackupAction::List { json, dir }) => list(json, dir),
        Some(BackupAction::Verify { target, dir }) => verify(&target, dir),
        Some(BackupAction::Restore {
            target,
            project,
            mode,
            identity,
            yes,
            force,
            dir,
        }) => restore(&target, project, &mode, identity, yes, force, dir),
        Some(BackupAction::Prune { dry_run, dir }) => prune(dry_run, dir),
    }
}

fn create(with_artifacts: bool, dir: Option<PathBuf>) -> Result<ExitCode> {
    let store = ProjectStore::open()?;
    let (policy, dir) = backup::resolve_dir(store.root(), dir.as_deref())?;
    let created = backup::create(
        store.root(),
        &backup::CreateRequest {
            dir,
            kind: Kind::Manual,
            with_artifacts,
            policy,
        },
    )?;
    print_archive(&created.core);
    if let Some(artifacts) = &created.artifacts {
        print_archive(artifacts);
    }
    println!("locked copy {}ms", created.locked_copy.as_millis());
    Ok(ExitCode::SUCCESS)
}

fn print_archive(info: &backup::ArchiveInfo) {
    println!("{}", info.path.display());
    println!("{} bytes, {} files", info.bytes, info.files);
}

fn list(json: bool, dir: Option<PathBuf>) -> Result<ExitCode> {
    let store = ProjectStore::open()?;
    let (policy, dir) = backup::resolve_dir(store.root(), dir.as_deref())?;
    let rows = backup::list(&dir, &policy)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows)
                .map_err(|err| KanbanError::Invalid(err.to_string()))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    if rows.is_empty() {
        println!("no backups");
        return Ok(ExitCode::SUCCESS);
    }
    for row in rows {
        println!(
            "{}  {:<11} {:<11} core {:<10} artifacts {:<10} {}",
            row.created,
            row.kind,
            row.bucket,
            row.core_bytes,
            row.artifacts_bytes,
            if row.verified {
                "verified"
            } else {
                "unverified"
            }
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn verify(target: &str, dir: Option<PathBuf>) -> Result<ExitCode> {
    let store = ProjectStore::open()?;
    let (_policy, dir) = backup::resolve_dir(store.root(), dir.as_deref())?;
    let path = backup::resolve_archive(&dir, target)?;
    let manifest = backup::verify(&path)?;
    println!(
        "ok {} ({} files, {})",
        path.display(),
        manifest.files.len(),
        manifest.part
    );
    Ok(ExitCode::SUCCESS)
}

fn restore(
    target: &str,
    project: Option<String>,
    mode: &str,
    identity: bool,
    yes: bool,
    force: bool,
    dir: Option<PathBuf>,
) -> Result<ExitCode> {
    let mode = match mode {
        "full" => RestoreMode::Full,
        "board" => RestoreMode::Board,
        other => {
            return Err(KanbanError::Invalid(format!(
                "backup restore mode must be board or full, got {other}"
            )));
        }
    };
    if project.is_some() && mode == RestoreMode::Full {
        return Err(KanbanError::Invalid(
            "--project restores one board; it cannot be combined with --mode full".into(),
        ));
    }
    let store = ProjectStore::open()?;
    let (policy, dir) = backup::resolve_dir(store.root(), dir.as_deref())?;
    let archive = backup::resolve_archive(&dir, target)?;
    let tty = std::io::stdin().is_terminal();
    let shown = archive.display().to_string();
    let report = backup::restore_with(
        store.root(),
        &backup::RestoreRequest {
            archive,
            dir,
            mode,
            project,
            identity,
            yes,
            force,
            policy,
        },
        &mut |pending| {
            println!("restore {} from {shown}", pending.mode);
            println!("projects: {}", pending.projects.join(", "));
            if pending.mode != "artifacts" {
                println!(
                    "tasks: {} replaced, {} added, {} removed",
                    pending.tasks_replaced, pending.tasks_added, pending.tasks_removed
                );
                println!(
                    "threads: {} replaced, {} added, {} removed",
                    pending.threads_replaced, pending.threads_added, pending.threads_removed
                );
            }
            if identity {
                eprintln!(
                    "warning: --identity reuses the archived node_id; a second live machine with that id breaks HLC/LWW"
                );
            }
            if yes {
                return Ok(true);
            }
            if !tty {
                return Err(KanbanError::Invalid(
                    "restore requires --yes when stdin is not a terminal".into(),
                ));
            }
            print!("Proceed? [y/N] ");
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
        },
    )?;
    println!(
        "restored {} (tasks +{} ~{} -{}, threads +{} ~{} -{})",
        report.mode,
        report.tasks_added,
        report.tasks_replaced,
        report.tasks_removed,
        report.threads_added,
        report.threads_replaced,
        report.threads_removed
    );
    Ok(ExitCode::SUCCESS)
}

fn prune(dry_run: bool, dir: Option<PathBuf>) -> Result<ExitCode> {
    let store = ProjectStore::open()?;
    let (policy, dir) = backup::resolve_dir(store.root(), dir.as_deref())?;
    let removed = backup::prune(&dir, &policy, dry_run)?;
    if removed.is_empty() {
        println!("nothing to prune");
    }
    for path in removed {
        if dry_run {
            println!("would delete {}", path.display());
        } else {
            println!("deleted {}", path.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}
