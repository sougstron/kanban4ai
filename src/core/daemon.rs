//! Headless dispatcher: advance every registered project's queue and
//! crash-restart schedule without a TUI.
//!
//! The CLI verb is `kanban daemon`. This module is the store-wide tick and
//! the single-instance lock; it has no terminal assumptions.

use std::collections::HashSet;
use std::fs::File;
use std::io::ErrorKind;
use std::path::PathBuf;

use fs2::FileExt;

use super::error::{KanbanError, Result};
use super::operations::{Operations, WaitWake};
use super::project::{Project, ProjectStore};
use super::session::SessionManager;
use super::timefmt;

pub use super::global::DEFAULT_DAEMON_INTERVAL_SECS as DEFAULT_INTERVAL_SECS;

/// Exclusive lock at the store root. Two daemons are pointless; a TUI
/// pumping the same boards at the same time is fine — dispatch claims under
/// the board lock.
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";

/// Append-only log under `<store>/logs/`. One line per resume / reap /
/// restart / dispatch (and the occasional warning).
pub const DAEMON_LOG_FILE: &str = "daemon.log";

/// Held for the lifetime of the daemon process.
pub struct DaemonLock {
    _file: File,
    pub path: PathBuf,
}

pub fn lock_path(store: &ProjectStore) -> PathBuf {
    store.root().join(DAEMON_LOCK_FILE)
}

pub fn log_path(store: &ProjectStore) -> PathBuf {
    store.root().join("logs").join(DAEMON_LOG_FILE)
}

/// `flock` the store `daemon.lock`. Returns a clear error when another
/// daemon already holds it (`WouldBlock`); does not block.
pub fn try_lock(store: &ProjectStore) -> Result<DaemonLock> {
    std::fs::create_dir_all(store.root())?;
    let path = lock_path(store);
    let file = File::create(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(DaemonLock { _file: file, path }),
        Err(err) if err.kind() == ErrorKind::WouldBlock => Err(KanbanError::Invalid(format!(
            "kanban daemon is already running (holds {})",
            path.display()
        ))),
        Err(err) => Err(err.into()),
    }
}

/// One tick across the store: every registered project, or just `--project`
/// when given. A missing work folder is warned once (then skipped); a
/// project with `orchestration.queue_enabled: false` is skipped quietly;
/// any other per-project error is a warning so one bad board cannot kill
/// the loop.
///
/// A `--project` that no longer resolves is a warning too, not an error. The
/// daemon is a long-lived unit with `Restart=on-failure`: propagating here
/// would exit the process and crashloop it every `--interval` seconds for as
/// long as the project stays unregistered. `run` still rejects an unknown
/// `--project` up front, so a typo is reported immediately at startup; only a
/// project that disappears *under* a running daemon degrades to this warning.
///
/// `warned_once` de-duplicates the warnings that would otherwise repeat every
/// tick, keyed by kind so the different sources cannot collide.
pub fn tick(
    store: &ProjectStore,
    only: Option<&str>,
    warned_once: &mut HashSet<String>,
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let projects = if let Some(needle) = only.map(str::trim).filter(|needle| !needle.is_empty()) {
        match store.find(needle)? {
            Some(project) => vec![project],
            None => {
                if warned_once.insert(format!("no-project:{needle}")) {
                    lines.push(format!(
                        "{} warning: no such project: {needle}; skipping",
                        timefmt::format(&timefmt::now())
                    ));
                }
                return Ok(lines);
            }
        }
    } else {
        store.list()?
    };

    for project in projects {
        match pump_project(&project, warned_once) {
            Ok(mut extra) => lines.append(&mut extra),
            Err(err) => lines.push(format!(
                "{} warning: project {}: {err}",
                timefmt::format(&timefmt::now()),
                project.id
            )),
        }
    }
    Ok(lines)
}

fn pump_project(project: &Project, warned_once: &mut HashSet<String>) -> Result<Vec<String>> {
    let ts = timefmt::format(&timefmt::now());
    if project.work_path_missing() {
        if warned_once.insert(format!("missing-work-path:{}", project.id)) {
            return Ok(vec![format!(
                "{ts} warning: project {} work folder is gone ({}); skipping",
                project.id,
                project.work_path.display()
            )]);
        }
        return Ok(Vec::new());
    }

    let ops = Operations::for_project(project);
    let orchestration = ops.config.get_orchestration()?;
    let mut lines = Vec::new();
    // Ineffective settings (an unknown backend in a cap map). Warned once per
    // project per daemon run, not once per tick, or the log would fill up.
    for warning in ops.config.warnings() {
        if warned_once.insert(format!("config:{}:{warning}", project.id)) {
            lines.push(format!("{ts} warning: project {}: {warning}", project.id));
        }
    }
    if !orchestration.queue_enabled {
        return Ok(lines);
    }

    for wake in ops.wake_expired_waits()? {
        match wake {
            WaitWake::Queued { task_id } => {
                lines.push(format!("{ts} {} queue {task_id}", project.id));
            }
            WaitWake::Resumed {
                task_id,
                session_id,
            } => {
                lines.push(format!(
                    "{ts} {} resume {task_id} → {session_id}",
                    project.id
                ));
            }
        }
    }

    // Reap then schedule, matching `dispatch_queue`: `check_sessions` only
    // returns sessions it just marked crashed, so a later dispatch tick
    // would otherwise miss them.
    let timeout = ops.config.get_threshold("session_heartbeat_timeout")?;
    for session in SessionManager::new(ops.data_root()).check_sessions(timeout)? {
        let _ = ops.schedule_crash_restart(&session.task_id);
        lines.push(format!(
            "{ts} {} reap {} (task: {})",
            project.id, session.id, session.task_id
        ));
    }

    for task_id in ops.due_restarts()? {
        lines.push(format!("{ts} {} restart {task_id}", project.id));
    }

    for item in ops.dispatch_queue()? {
        lines.push(format!(
            "{ts} {} dispatch {} → {} ({})",
            project.id, item.task_id, item.session_id, item.backend
        ));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::project::ProjectStore;

    #[test]
    fn second_lock_is_rejected() {
        let dir = tempfile::tempdir().expect("store");
        let store = ProjectStore::at(dir.path());
        let first = try_lock(&store).expect("first lock");
        match try_lock(&store) {
            Err(err) => assert!(
                err.to_string().contains("already running"),
                "unexpected error: {err}"
            ),
            Ok(_) => panic!("second lock should be rejected"),
        }
        drop(first);
        try_lock(&store).expect("lock released");
    }

    /// `Restart=on-failure` plus a propagated error is a crashloop: the unit
    /// would exit and be restarted every interval for as long as the project
    /// stays unregistered.
    #[test]
    fn a_missing_project_warns_once_instead_of_ending_the_loop() {
        let dir = tempfile::tempdir().expect("store");
        let store = ProjectStore::at(dir.path());
        let mut warned = HashSet::new();

        let first = tick(&store, Some("ghost"), &mut warned).expect("tick must not fail");
        assert_eq!(first.len(), 1, "one warning: {first:?}");
        assert!(
            first[0].contains("no such project: ghost"),
            "unexpected line: {}",
            first[0]
        );

        let second = tick(&store, Some("ghost"), &mut warned).expect("tick must not fail");
        assert!(
            second.is_empty(),
            "the warning must not repeat every tick: {second:?}"
        );
    }
}
