//! Opt-in read-only timing harness; never constructs App or acquires a board lock.
//! Run with KANBAN_PROFILE_PROJECT=/path/to/board-data-root and
//! cargo test --lib tui::tick_profile::profile_tick_reads -- --ignored --nocapture.
//! Sync-enabled live boards skip snapshots because their loader opens a writable
//! sync journal. KANBAN_PROFILE_SNAPSHOT_PROJECT may point to an isolated copy
//! outside a sync-enabled store to measure those reads without touching live data.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::app::BoardSnapshot;
use crate::core::operations::Operations;
use crate::core::session::{SessionManager, SessionState};
use crate::core::storage::TaskCache;
use crate::core::{stats, sync, telemetry};

const ROUNDS: usize = 20;

fn measure<T>(
    samples: &mut BTreeMap<&'static str, Vec<Duration>>,
    name: &'static str,
    read: impl FnOnce() -> T,
) -> T {
    let start = Instant::now();
    let value = black_box(read());
    samples.entry(name).or_default().push(start.elapsed());
    value
}

fn snapshot_safe(ops: &Operations) -> bool {
    !sync::store_for_board(ops.data_root()).is_some_and(sync::enabled)
}

#[test]
#[ignore = "read-only live-board profiling; requires KANBAN_PROFILE_PROJECT"]
fn profile_tick_reads() {
    let root = PathBuf::from(
        std::env::var_os("KANBAN_PROFILE_PROJECT")
            .expect("set KANBAN_PROFILE_PROJECT to the directory containing .kanban"),
    )
    .canonicalize()
    .expect("profile project exists");
    assert!(root.join(".kanban").is_dir(), "expected a board data root");
    let ops = Operations::new(&root);
    let snapshot_ops = std::env::var_os("KANBAN_PROFILE_SNAPSHOT_PROJECT").map(|path| {
        let path = PathBuf::from(path)
            .canonicalize()
            .expect("snapshot copy exists");
        assert!(
            path.join(".kanban").is_dir(),
            "expected copied board data root"
        );
        let copy = Operations::new(path);
        assert!(
            snapshot_safe(&copy),
            "snapshot copy must be outside a sync-enabled store"
        );
        copy
    });
    let board_ops = snapshot_ops
        .as_ref()
        .or_else(|| snapshot_safe(&ops).then_some(&ops));
    eprintln!("read-only profile: live={} rounds={ROUNDS}", root.display());
    if let Some(board_ops) = board_ops {
        eprintln!("snapshot source={}", board_ops.data_root().display());
    } else {
        eprintln!(
            "SKIP snapshot/sidecars: sync-enabled live loader writes; set KANBAN_PROFILE_SNAPSHOT_PROJECT to an isolated copy"
        );
    }
    // Cold means an empty application cache, not a flushed OS page cache.
    let tasks = ops.storage.list_tasks_cached(&mut TaskCache::default());
    let heartbeat_timeout = ops
        .config
        .get_threshold("session_heartbeat_timeout")
        .expect("read heartbeat timeout");
    let active_sessions: BTreeMap<_, _> = SessionManager::new(ops.data_root())
        .list_sessions_with_state(heartbeat_timeout)
        .into_iter()
        .filter(|(_, state)| matches!(state, SessionState::Live | SessionState::Waiting))
        .map(|(session, _)| (session.id, session.task_id))
        .collect();
    let column_ids = ops.config.load().expect("read board columns").column_ids();
    let live_tasks: Vec<_> = tasks
        .iter()
        .filter(|task| {
            column_ids
                .iter()
                .any(|column| column == task.status.as_str())
                && task.session.as_ref().is_some_and(|id| {
                    active_sessions
                        .get(id)
                        .is_some_and(|task_id| task_id == &task.id)
                })
        })
        .collect();
    let requests: Vec<_> = live_tasks
        .iter()
        .map(|task| (task.id.clone(), task.created_at))
        .collect();
    let sessions: Vec<_> = live_tasks
        .iter()
        .filter_map(|task| {
            task.session
                .as_deref()
                .map(|id| (id, task.agent_backend.as_deref().unwrap_or("claude")))
        })
        .collect();
    eprintln!(
        "tasks={} tick_live_waiting_tasks={} active_session_records={}",
        tasks.len(),
        live_tasks.len(),
        active_sessions.len()
    );
    let mut samples = BTreeMap::new();
    let mut previous = None;
    for _ in 0..ROUNDS {
        let total = Instant::now();
        measure(&mut samples, "live board stamps", || {
            ops.storage.board_stamps()
        });
        measure(&mut samples, "live effective config", || {
            ops.config.load().expect("read config")
        });
        measure(&mut samples, "live global config", || {
            ops.config.load_global()
        });
        if let Some(board_ops) = board_ops {
            let cold = measure(&mut samples, "snapshot cold", || {
                BoardSnapshot::load_reusing(board_ops, None).expect("read cold snapshot")
            });
            let mut warm = measure(&mut samples, "snapshot warm", || {
                BoardSnapshot::load_reusing(board_ops, previous.as_ref().or(Some(&cold)))
                    .expect("read warm snapshot")
            });
            let stamps = board_ops.storage.board_stamps();
            measure(&mut samples, "sidecars unchanged", || {
                warm.refresh_sidecars(board_ops, stamps)
                    .expect("read unchanged sidecars")
            });
            // Invalidate only the in-memory signatures to force both sidecar paths.
            warm.stamps.sessions.0 = stamps.sessions.0.wrapping_add(1);
            warm.stamps.threads.0 = stamps.threads.0.wrapping_add(1);
            measure(&mut samples, "sidecars forced", || {
                warm.refresh_sidecars(board_ops, stamps)
                    .expect("read changed sidecars")
            });
            previous = Some(warm);
        }
        measure(&mut samples, "live session telemetry", || {
            for (id, backend) in &sessions {
                black_box(telemetry::read_session_progress(
                    ops.data_root(),
                    id,
                    backend,
                ));
            }
        });
        measure(&mut samples, "live events stamp", || {
            stats::events_stamp(ops.data_root())
        });
        measure(&mut samples, "tick analytics scalar cache miss", || {
            for (id, since) in &requests {
                black_box(stats::task_analytics(ops.data_root(), id, *since));
            }
        });
        measure(&mut samples, "tick analytics batch cache miss", || {
            if requests.is_empty() {
                None
            } else {
                Some(stats::tasks_analytics(ops.data_root(), &requests))
            }
        });
        // Sum this round's constituent measurements, excluding diagnostic
        // comparisons/config reads that an unchanged production tick skips.
        let latest = |name: &'static str| *samples.get(name).unwrap().last().unwrap();
        let unchanged = latest("live board stamps")
            + latest("live session telemetry")
            + latest("live events stamp");
        let forced = board_ops.map(|_| unchanged + latest("snapshot warm"));
        let scalar = latest("tick analytics scalar cache miss");
        let batch = latest("tick analytics batch cache miss");
        samples
            .entry("tick unchanged cached total")
            .or_default()
            .push(unchanged);
        if let Some(forced) = forced {
            samples
                .entry("tick forced reload cached total")
                .or_default()
                .push(forced);
            samples
                .entry("tick forced reload scalar miss")
                .or_default()
                .push(forced + scalar);
            samples
                .entry("tick forced reload batch miss")
                .or_default()
                .push(forced + batch);
        }
        samples
            .entry("total measured read-only parts")
            .or_default()
            .push(total.elapsed());
    }
    eprintln!(
        "milliseconds: min / median / max; tick totals sum measured read-only parts (no dispatch/sync work); forced reload uses the reported snapshot source; overall total includes comparisons"
    );
    for (name, mut times) in samples {
        times.sort_unstable();
        let median = (times[ROUNDS / 2 - 1].as_secs_f64() + times[ROUNDS / 2].as_secs_f64()) / 2.0;
        eprintln!(
            "{name:34} {:10.3} / {:10.3} / {:10.3}",
            times[0].as_secs_f64() * 1000.0,
            median * 1000.0,
            times[ROUNDS - 1].as_secs_f64() * 1000.0
        );
    }
}
