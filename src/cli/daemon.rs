//! `kanban daemon` — foreground loop that pumps every registered project.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use crate::core::daemon::{self, try_lock};
use crate::core::error::{KanbanError, Result};
use crate::core::project::ProjectStore;

/// Foreground loop. Does not fork. `--once` is the cron / systemd-timer
/// entry point; the plain loop is what the user unit runs.
pub fn run(once: bool, interval: Option<u64>, project: Option<&str>) -> Result<ExitCode> {
    if let Some(0) = interval {
        return Err(KanbanError::Invalid(
            "--interval must be greater than 0".into(),
        ));
    }

    let store = ProjectStore::open()?;
    // A typo is reported now rather than as a per-tick warning. From here on
    // the project is allowed to disappear: `tick` degrades that to a warning
    // instead of exiting, which would crashloop the systemd unit.
    if let Some(needle) = project.map(str::trim).filter(|needle| !needle.is_empty())
        && store.find(needle)?.is_none()
    {
        return Err(KanbanError::Invalid(format!("no such project: {needle}")));
    }
    let _lock = try_lock(&store)?;
    let interval = interval.unwrap_or_else(|| {
        store
            .load_global_config()
            .map(|config| config.daemon_interval())
            .unwrap_or(daemon::DEFAULT_INTERVAL_SECS)
    });

    let mut warned_once = HashSet::new();
    loop {
        let lines = daemon::tick(&store, project, &mut warned_once)?;
        for line in &lines {
            println!("{line}");
            append_log(&store, line);
        }
        if once {
            break;
        }
        thread::sleep(Duration::from_secs(interval));
    }
    Ok(ExitCode::SUCCESS)
}

fn append_log(store: &ProjectStore, line: &str) {
    let path = daemon::log_path(store);
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}
