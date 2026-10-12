//! `kanban daemon` — foreground loop that pumps every registered project.

use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use crate::core::daemon::{self, PumpState, try_lock};
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

    let mut network = crate::core::net::Manager::new(store.root())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    #[cfg(unix)]
    let mut terminate = runtime.block_on(async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    })?;
    #[cfg(unix)]
    let mut interrupt = runtime.block_on(async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
    })?;
    let mut state = PumpState::default();
    loop {
        if let Err(error) = network.tick() {
            let line = error.to_string();
            eprintln!("{line}");
            append_log(&store, &line);
        }
        // The whole store goes through the shared pump, so an open TUI that
        // pumps more often leaves the daemon idle instead of doubling the
        // work; `--project` is an explicit request and always runs.
        let lines = if project.is_some() {
            daemon::tick(&store, project, &mut state)?
        } else {
            daemon::shared_tick(&store, Duration::from_secs(interval), &mut state)?
                .unwrap_or_default()
        };
        for line in &lines {
            println!("{line}");
            append_log(&store, line);
        }
        if once {
            break;
        }
        let stopped = runtime.block_on(async {
            #[cfg(unix)]
            {
                tokio::select! {
                    _ = terminate.recv() => true,
                    _ = interrupt.recv() => true,
                    _ = tokio::time::sleep(Duration::from_secs(interval)) => false,
                }
            }
            #[cfg(not(unix))]
            {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => true,
                    _ = tokio::time::sleep(Duration::from_secs(interval)) => false,
                }
            }
        });
        if stopped {
            break;
        }
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
