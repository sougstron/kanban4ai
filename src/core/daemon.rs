//! Headless dispatcher: advance every registered project's queue and
//! crash-restart schedule without a TUI.
//!
//! The CLI verb is `kanban daemon`. This module is the store-wide tick and
//! the single-instance lock; it has no terminal assumptions.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use chrono::NaiveDateTime;
use fs2::FileExt;

use super::config::Config;
use super::error::{KanbanError, Result};
use super::models::RunPhase;
use super::operations::{Operations, WaitWake};
use super::project::{Project, ProjectStore};
use super::session::SessionManager;
use super::storage::{Fingerprint, Storage, stat_ns};
use super::timefmt;

pub use super::global::DEFAULT_DAEMON_INTERVAL_SECS as DEFAULT_INTERVAL_SECS;

/// Exclusive lock at the store root. Two daemons are pointless; a TUI
/// pumping the same boards at the same time is fine — dispatch claims under
/// the board lock.
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";

/// Store-root lock and stamp shared by every store pump (each open TUI and
/// the daemon): held for one tick, and its mtime records when the last tick
/// finished. Separate from [`DAEMON_LOCK_FILE`], which a daemon holds for its
/// whole life.
pub const PUMP_LOCK_FILE: &str = "pump.lock";

/// How long a project with no visible change may go without a full pump.
/// Covers what no file stamp or deadline shows: a deferred cleanup waiting
/// for a process to leave its worktree, a provider limit lifting.
pub const QUIET_BACKSTOP: Duration = Duration::from_secs(60);

/// A stamp younger than `min_gap` minus this counts as fresh. Without the
/// slack two pumps on the same cadence that wake a few hundred ms apart would
/// each skip every other tick.
const PUMP_GAP_SLACK: Duration = Duration::from_secs(1);

/// Per-process memory of the store pump, kept across ticks.
#[derive(Default)]
pub struct PumpState {
    /// Warnings that would otherwise repeat every tick, keyed by kind so the
    /// different sources cannot collide.
    pub warned_once: HashSet<String>,
    /// Ids the last tick pumped in full; the others were skipped as quiet.
    pub last_pumped: Vec<String>,
    quiet: HashMap<String, Quiet>,
}

/// What a full pump left behind: the project's file stamp taken after the
/// pump's own writes, and the earliest clock deadline that would give the
/// next pump something to do. While both hold, pumping again cannot act.
struct Quiet {
    stamp: ProjectStamp,
    wake_at: Option<NaiveDateTime>,
    pumped_at: Instant,
}

impl Quiet {
    fn holds(&self, project: &Project, now: NaiveDateTime) -> bool {
        self.pumped_at.elapsed() < QUIET_BACKSTOP
            && self.wake_at.is_none_or(|at| now < at)
            && project_stamp(project) == self.stamp
    }
}

/// Every file input of [`pump_project`], by `stat` alone: tasks (all
/// columns), session files, the board config and the inherited global one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProjectStamp {
    tasks: Fingerprint,
    sessions: Fingerprint,
    config: Option<(u128, u64)>,
    global: Option<SystemTime>,
}

fn project_stamp(project: &Project) -> ProjectStamp {
    let storage = Storage::new(&project.data_root);
    let config = Config::new(&project.data_root);
    ProjectStamp {
        tasks: storage.tasks_fingerprint(),
        sessions: storage.sessions_fingerprint(),
        config: stat_ns(&config.config_file),
        global: config.global_stamp(),
    }
}

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
/// A project whose files are unchanged since this process last pumped it, and
/// whose next deadline (session timeout, wait, crash-restart, planned launch)
/// has not come, is skipped: pumping it again could not act. Most boards are
/// idle most of the time, so this keeps a tick down to a few `stat`s per
/// board. [`QUIET_BACKSTOP`] still pumps every board now and then.
pub fn tick(
    store: &ProjectStore,
    only: Option<&str>,
    state: &mut PumpState,
) -> Result<Vec<String>> {
    let warned_once = &mut state.warned_once;
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

    // Keep the shared limits cache warm while the TUI is closed: the
    // executor-pool gate reads the cached snapshot only (never a blocking
    // fetch), so without this a daemon-only board's numbers would go stale.
    // Never fails the tick — the gate treats an unreadable cache as usable.
    let limits_ttl = projects
        .first()
        .map(|project| {
            Operations::for_project(project)
                .config
                .get_threshold("limits_refresh_interval")
        })
        .unwrap_or_else(|| Ok(crate::core::limits::DEFAULT_REFRESH_INTERVAL))
        .unwrap_or(crate::core::limits::DEFAULT_REFRESH_INTERVAL);
    crate::core::limits::refresh_if_stale(limits_ttl);

    lines.append(&mut pump_all(projects, state));
    Ok(lines)
}

/// The per-project half of [`tick`], quiet skip included.
fn pump_all(projects: Vec<Project>, state: &mut PumpState) -> Vec<String> {
    let mut lines = Vec::new();
    state
        .quiet
        .retain(|id, _| projects.iter().any(|project| &project.id == id));
    state.last_pumped.clear();
    let now = timefmt::now();
    for project in projects {
        if state
            .quiet
            .get(&project.id)
            .is_some_and(|quiet| quiet.holds(&project, now))
        {
            continue;
        }
        state.quiet.remove(&project.id);
        state.last_pumped.push(project.id.clone());
        match pump_project(&project, &mut state.warned_once) {
            Ok((mut extra, quiet)) => {
                lines.append(&mut extra);
                if let Some(quiet) = quiet {
                    state.quiet.insert(project.id.clone(), quiet);
                }
            }
            Err(err) => lines.push(format!(
                "{} warning: project {}: {err}",
                timefmt::format(&timefmt::now()),
                project.id
            )),
        }
    }
    lines
}

/// [`tick`] for every project, coordinated across processes through
/// [`PUMP_LOCK_FILE`] so N open TUIs plus a daemon pump the store once, not
/// N + 1 times. Skipped (`None`) while another process is mid-tick, or when
/// the last tick by anyone finished less than `min_gap` ago.
pub fn shared_tick(
    store: &ProjectStore,
    min_gap: Duration,
    state: &mut PumpState,
) -> Result<Option<Vec<String>>> {
    exclusive_run(store, min_gap, || tick(store, None, state))
}

/// Run `pump` under [`PUMP_LOCK_FILE`] unless it is held or fresh, then
/// stamp it.
fn exclusive_run(
    store: &ProjectStore,
    min_gap: Duration,
    pump: impl FnOnce() -> Result<Vec<String>>,
) -> Result<Option<Vec<String>>> {
    std::fs::create_dir_all(store.root())?;
    let path = store.root().join(PUMP_LOCK_FILE);
    // A just-created file carries a fresh mtime that no tick wrote.
    let existed = path.exists();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let fresh = min_gap.saturating_sub(PUMP_GAP_SLACK);
    if existed
        && file
            .metadata()?
            .modified()
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age < fresh)
    {
        return Ok(None);
    }
    let lines = pump()?;
    let _ = file.set_modified(SystemTime::now());
    Ok(Some(lines))
}

fn pump_project(
    project: &Project,
    warned_once: &mut HashSet<String>,
) -> Result<(Vec<String>, Option<Quiet>)> {
    let ts = timefmt::format(&timefmt::now());
    if project.work_path_missing() {
        if warned_once.insert(format!("missing-work-path:{}", project.id)) {
            return Ok((
                vec![format!(
                    "{ts} warning: project {} work folder is gone ({}); skipping",
                    project.id,
                    project.work_path.display()
                )],
                None,
            ));
        }
        return Ok((Vec::new(), None));
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
    // Deferred post-landing cleanups no `agent-exit` will finish. Unrelated
    // to the queue, so it runs even when the queue is off.
    // Best effort: a failed sweep must not stall dispatch.
    for task_id in ops.sweep_deferred_cleanups().unwrap_or_default() {
        lines.push(format!("{ts} {} cleanup {task_id}", project.id));
    }
    if !orchestration.queue_enabled {
        return Ok((lines, settle(project, &ops, false)?));
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
        let _ = ops.schedule_crash_restart(&session.task_id, "heartbeat timeout");
        lines.push(format!(
            "{ts} {} reap {} (task: {})",
            project.id, session.id, session.task_id
        ));
    }

    for task_id in ops.due_restarts()? {
        lines.push(format!("{ts} {} restart {task_id}", project.id));
    }

    for task_id in ops.due_launches()? {
        lines.push(format!("{ts} {} launch {task_id}", project.id));
    }

    // The graph's pull step, between the restart schedule and dispatch so a
    // node that just became ready is queued and started in the same tick.
    for task_id in ops.dispatch_ready_dependents()? {
        lines.push(format!("{ts} {} ready {task_id}", project.id));
    }

    for item in ops.dispatch_queue()? {
        lines.push(format!(
            "{ts} {} dispatch {} → {} ({})",
            project.id, item.task_id, item.session_id, item.backend
        ));
    }
    Ok((lines, settle(project, &ops, true)?))
}

/// The [`Quiet`] a finished pump leaves, or `None` while a queued task still
/// waits for a slot (a run elsewhere on the board may end any moment), so
/// the next tick pumps the board again. `queue_on` is false when the pump
/// stopped after the cleanup sweep: then no deadline can make it act.
fn settle(project: &Project, ops: &Operations, queue_on: bool) -> Result<Option<Quiet>> {
    let stamp = project_stamp(project);
    let mut wake_at = None;
    if queue_on {
        let now = timefmt::now();
        let timeout = ops.config.get_threshold("session_heartbeat_timeout")?;
        let dispatching = ops.queue_can_dispatch()?;
        let mut deadlines = Vec::new();
        for session in ops.session_manager().list_active_sessions() {
            // Reaped once silent past the timeout, unless inside a declared
            // wait; an expired wait is woken on the same timeout.
            deadlines.push(match session.wait_until {
                Some(until) if now <= until => until,
                _ => session.last_seen + chrono::Duration::seconds(timeout),
            });
        }
        for task in ops.storage.list_tasks(Some("in_progress"))? {
            if dispatching
                && task.run_phase == Some(RunPhase::Queued)
                && task.restart_at.is_none_or(|at| at <= now)
            {
                return Ok(None);
            }
            deadlines.extend(task.restart_at);
        }
        for task in ops.storage.list_tasks(Some("todo"))? {
            deadlines.extend(task.launch_at);
        }
        // A deadline already behind us is one this pump could not act on
        // (restarts off, say); only a config or task change revives it.
        // The extra second clears the strict comparisons and whole-second
        // rounding of the checks themselves.
        wake_at = deadlines
            .into_iter()
            .filter(|at| *at > now)
            .min()
            .map(|at| at + chrono::Duration::seconds(1));
    }
    Ok(Some(Quiet {
        stamp,
        wake_at,
        pumped_at: Instant::now(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::TaskStatus;
    use crate::core::project::ProjectStore;
    use crate::core::storage::NewTask;

    fn board(store: &std::path::Path, work: &std::path::Path) -> Project {
        let project = ProjectStore::at(store)
            .add(work, None)
            .expect("add")
            .project;
        Storage::new(&project.data_root).init_board().expect("init");
        project
    }

    #[test]
    fn an_unchanged_project_is_skipped_until_its_files_change() {
        let store = tempfile::tempdir().expect("store");
        let work = tempfile::tempdir().expect("work");
        let project = board(store.path(), work.path());
        let mut state = PumpState::default();

        pump_all(vec![project.clone()], &mut state);
        assert_eq!(state.last_pumped, vec![project.id.clone()]);

        pump_all(vec![project.clone()], &mut state);
        assert!(state.last_pumped.is_empty(), "nothing changed, nothing due");

        Storage::new(&project.data_root)
            .create_task(NewTask::titled("New work"))
            .expect("task");
        pump_all(vec![project.clone()], &mut state);
        assert_eq!(state.last_pumped, vec![project.id.clone()]);
    }

    #[test]
    fn settle_wakes_at_the_next_deadline_and_stays_busy_on_a_queued_task() {
        let store = tempfile::tempdir().expect("store");
        let work = tempfile::tempdir().expect("work");
        let project = board(store.path(), work.path());
        let ops = Operations::for_project(&project);

        let later = ops
            .storage
            .create_task(NewTask {
                launch_at: Some(timefmt::now() + chrono::Duration::hours(1)),
                ..NewTask::titled("Planned")
            })
            .expect("task");
        let launch_at = ops
            .storage
            .load_task(&later.id)
            .expect("load")
            .and_then(|task| task.launch_at)
            .expect("launch_at");
        let quiet = settle(&project, &ops, true)
            .expect("settle")
            .expect("quiet");
        assert_eq!(
            quiet.wake_at,
            Some(launch_at + chrono::Duration::seconds(1))
        );
        // With the queue off no deadline can make the pump act.
        let quiet = settle(&project, &ops, false)
            .expect("settle")
            .expect("quiet");
        assert_eq!(quiet.wake_at, None);

        let mut queued = ops
            .storage
            .create_task_in_status(NewTask::titled("Queued"), TaskStatus::InProgress)
            .expect("task");
        queued.run_phase = Some(RunPhase::Queued);
        ops.storage.save_task(&queued).expect("save");
        assert!(
            settle(&project, &ops, true).expect("settle").is_none(),
            "a task waiting for a slot keeps the board busy"
        );
    }

    #[test]
    fn the_shared_pump_runs_once_per_gap_and_never_concurrently() {
        let dir = tempfile::tempdir().expect("store");
        let store = ProjectStore::at(dir.path());
        let gap = Duration::from_secs(60);
        let run = || Ok(vec!["pumped".to_string()]);

        assert!(exclusive_run(&store, gap, run).expect("first").is_some());
        assert!(
            exclusive_run(&store, gap, run).expect("second").is_none(),
            "another process pumped within the gap"
        );

        let holder = File::open(store.root().join(PUMP_LOCK_FILE)).expect("open");
        holder.lock_exclusive().expect("hold");
        assert!(
            exclusive_run(&store, Duration::ZERO, run)
                .expect("held")
                .is_none(),
            "another process is mid-tick"
        );
        drop(holder);
        assert!(
            exclusive_run(&store, Duration::ZERO, run)
                .expect("free")
                .is_some()
        );
    }

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
        let mut warned = PumpState::default();

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
