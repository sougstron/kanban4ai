//! Board reads and telemetry never hold the input/render thread hostage.
//! The UI owns scheduling (one request in flight); this thread owns its caches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use chrono::NaiveDateTime;

use super::app::BoardSnapshot;
use super::diag::PhaseTimer;
use super::event::AppEvent;
use crate::core::operations::{Operations, OperationsSeed};
use crate::core::session::SessionState;
use crate::core::stats::{self, EventsStamp, TaskAnalytics};
use crate::core::storage::BoardStamps;
use crate::core::telemetry::{self, SessionProgress, TokenBreakdown};

pub(super) struct LiveTask {
    id: String,
    session: String,
    backend: String,
    created_at: NaiveDateTime,
}

pub(super) fn live_tasks(board: &BoardSnapshot) -> Vec<LiveTask> {
    board
        .columns
        .iter()
        .flat_map(|column| &column.tasks)
        .filter_map(|task| {
            if !matches!(
                board.session_states.get(&task.id),
                Some(SessionState::Live | SessionState::Waiting)
            ) {
                return None;
            }
            Some(LiveTask {
                id: task.id.clone(),
                session: task.session.clone()?,
                backend: task
                    .agent_backend
                    .clone()
                    .unwrap_or_else(|| "claude".into()),
                created_at: task.created_at,
            })
        })
        .collect()
}

pub(super) struct TickRequest {
    pub generation: u64,
    pub main_stamps: BoardStamps,
    #[cfg(test)]
    pub before_read: Option<(mpsc::Receiver<()>, Sender<()>)>,
    pub force: bool,
    pub live: Vec<LiveTask>,
}

#[derive(Debug)]
pub struct TickResult {
    pub(super) worker_id: u64,
    pub(super) path: PathBuf,
    pub(super) tasks_changed: bool,
    pub(super) generation: u64,
    pub(super) board: Option<BoardSnapshot>,
    pub(super) progress: HashMap<String, SessionProgress>,
    pub(super) tokens: HashMap<String, TokenBreakdown>,
    pub(super) error: Option<String>,
}

#[derive(Default)]
pub(super) struct ProgressCache {
    analytics: HashMap<String, (EventsStamp, NaiveDateTime, TaskAnalytics)>,
}

impl ProgressCache {
    pub(super) fn refresh(
        &mut self,
        root: &Path,
        live: &[LiveTask],
    ) -> (
        HashMap<String, SessionProgress>,
        HashMap<String, TokenBreakdown>,
    ) {
        let mut timer = PhaseTimer::new("session progress");
        let stamp = stats::events_stamp(root);
        let missing: Vec<_> = live
            .iter()
            .filter(|task| {
                !self
                    .analytics
                    .get(&task.id)
                    .is_some_and(|(cached, since, _)| *cached == stamp && *since == task.created_at)
            })
            .map(|task| (task.id.clone(), task.created_at))
            .collect();
        let mut loaded = stats::tasks_analytics(root, &missing);
        timer.mark("task_analytics");
        let mut fresh = HashMap::with_capacity(live.len());
        let mut progress = HashMap::new();
        let mut tokens = HashMap::new();
        for task in live {
            let found = telemetry::read_session_progress(root, &task.session, &task.backend);
            let analytics = loaded
                .remove(&task.id)
                .or_else(|| {
                    self.analytics
                        .remove(&task.id)
                        .map(|(_, _, analytics)| analytics)
                })
                .unwrap_or_default();
            let total = analytics.tokens_with_live(Some(&task.session), found.breakdown.as_ref());
            if !total.is_empty() {
                tokens.insert(task.id.clone(), total);
            }
            fresh.insert(task.id.clone(), (stamp, task.created_at, analytics));
            if found.has_data() {
                progress.insert(task.id.clone(), found);
            }
        }
        timer.mark("read_session_progress");
        self.analytics = fresh;
        (progress, tokens)
    }
}

pub(super) struct TickWorker {
    pub id: u64,
    tx: Sender<TickRequest>,
    handle: JoinHandle<()>,
}

impl TickWorker {
    pub fn spawn(seed: OperationsSeed, wake: Sender<AppEvent>) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel::<TickRequest>();
        let handle = thread::spawn(move || {
            let ops = seed.build();
            let mut mirror = None;
            let mut cache = ProgressCache::default();
            while let Ok(request) = rx.recv() {
                #[cfg(test)]
                if let Some((gate, entered)) = &request.before_read {
                    let _ = entered.send(());
                    let _ = gate.recv();
                }
                let result = refresh(&ops, id, request, &mut mirror, &mut cache);
                if wake.send(AppEvent::TickReady(Box::new(result))).is_err() {
                    break;
                }
            }
        });
        Self { id, tx, handle }
    }

    pub fn finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub fn request(&self, request: TickRequest) -> bool {
        self.tx.send(request).is_ok()
    }
}

// Dropping the sender stops the read-only worker after its current request.
// Never join on project switch: a slow filesystem must not freeze navigation.
fn refresh(
    ops: &Operations,
    worker_id: u64,
    request: TickRequest,
    mirror: &mut Option<BoardSnapshot>,
    cache: &mut ProgressCache,
) -> TickResult {
    let mut timer = PhaseTimer::new("tick worker");
    let stamps = ops.storage.board_stamps();
    timer.mark("stamps");
    let loaded = (|| -> crate::core::error::Result<()> {
        if request.force
            || mirror
                .as_ref()
                .is_none_or(|board| board.stamps.tasks != stamps.tasks)
        {
            ops.config.load_fresh()?;
            *mirror = Some(BoardSnapshot::load_reusing(ops, mirror.as_ref())?);
        } else if let Some(board) = mirror.as_mut()
            && board.stamps != stamps
        {
            board.refresh_sidecars(ops, stamps)?;
        }
        Ok(())
    })();
    timer.mark("snapshot");
    let (progress, tokens) = cache.refresh(ops.data_root(), &request.live);
    timer.mark("progress");
    let board = if loaded.is_ok() {
        mirror
            .as_ref()
            .filter(|board| request.force || board.stamps != request.main_stamps)
            .cloned()
    } else {
        None
    };
    TickResult {
        worker_id,
        path: ops.data_root().to_path_buf(),
        generation: request.generation,
        tasks_changed: request.force || stamps.tasks != request.main_stamps.tasks,
        board,
        progress,
        tokens,
        error: loaded.err().map(|error| error.to_string()),
    }
}
