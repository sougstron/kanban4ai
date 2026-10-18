//! Board sync status and task/message conflict controls. All probes run off-thread.
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use super::app::{App, HitAction, Hitbox};
use super::card::{sanitize_terminal_text, truncate_display};
use crate::cli::serve::settings::{self, BoardPeer};
use crate::core::sync::{Conflict, Journal, NodeRole};

/// Peer probes go over the network and the serve monitor already runs them
/// every `sync.interval`, so the status bar refreshes them far less often than
/// the board ticks.
const PEER_PROBE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(crate) struct SyncBoard {
    pub peers: Vec<BoardPeer>,
    pub conflicts: Vec<Conflict>,
    pub tasks: Vec<String>,
    pub error: Option<String>,
    pub selected: usize,
    board: PathBuf,
    last_probe: Option<Instant>,
    journal_stamp: Option<u64>,
    job: Option<JoinHandle<Result<Snapshot, String>>>,
}

/// One worker's result; a part is `None` when it was not due this round.
struct Snapshot {
    board: PathBuf,
    peers: Option<Vec<BoardPeer>>,
    conflicts: Option<(Vec<Conflict>, Vec<String>)>,
}

impl Drop for SyncBoard {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            let _ = job.join();
        }
    }
}

impl SyncBoard {
    /// Give an in-flight status worker to the next screen's sync state
    /// instead of joining it on drop: a peer probe can wait out network
    /// timeouts, and a project switch must not. `next` reaps it on its next
    /// poll (a result for another board is discarded) and starts no second
    /// probe while it runs.
    pub fn hand_off(&mut self, next: &mut SyncBoard) {
        if next.job.is_none() {
            next.job = self.job.take();
        }
    }

    /// Called every tick. The conflict list is re-read only when the journal
    /// changed on disk, and peers are probed at most every
    /// [`PEER_PROBE_INTERVAL`], so an idle board costs two `stat`s per tick.
    pub fn poll(&mut self, root: &Path, board: &Path, interval: Duration) {
        if self.board != board {
            self.board = board.into();
            self.peers.clear();
            self.conflicts.clear();
            self.tasks.clear();
            self.error = None;
            self.last_probe = None;
            self.journal_stamp = None;
        }
        if self.job.as_ref().is_some_and(|job| job.is_finished()) {
            match self.job.take().expect("finished job").join() {
                Ok(Ok(snapshot)) if snapshot.board == self.board => {
                    if let Some(peers) = snapshot.peers {
                        self.peers = peers;
                    }
                    if let Some((conflicts, tasks)) = snapshot.conflicts {
                        self.conflicts = conflicts;
                        self.tasks = tasks;
                    }
                    self.error = None;
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.journal_stamp = None;
                }
                Err(_) => {
                    self.error = Some("sync status worker failed".into());
                    self.journal_stamp = None;
                }
                _ => {}
            }
        }
        if self.job.is_some() {
            return;
        }
        // A store with sync off never opens the journal or probes peers.
        if !crate::core::sync::enabled(root) {
            return;
        }
        let stamp = journal_stamp(root);
        let reload_conflicts = self.journal_stamp != stamp;
        let probe = self
            .last_probe
            .is_none_or(|at| at.elapsed() >= PEER_PROBE_INTERVAL.max(interval));
        if !reload_conflicts && !probe {
            return;
        }
        if reload_conflicts {
            self.journal_stamp = stamp;
        }
        if probe {
            self.last_probe = Some(Instant::now());
        }
        let root = root.to_path_buf();
        let board = board.to_path_buf();
        self.job = Some(std::thread::spawn(move || {
            load(&root, &board, reload_conflicts, probe).map_err(|e| e.to_string())
        }));
    }

    pub fn has_conflict(&self, task: &str) -> bool {
        self.tasks.iter().any(|id| id == task)
    }

    /// Status-bar indicator. A configured server is the authority when
    /// present; otherwise any reachable peer counts. Resolved conflicts stay
    /// in the journal indefinitely, so they mark their cards, not this light.
    pub fn indicator(&self) -> (SyncLevel, String) {
        if self.error.is_some() {
            return (SyncLevel::Error, "Sync error".into());
        }
        let server = self.peers.iter().any(|p| p.role == NodeRole::Server);
        let relevant: Vec<_> = self
            .peers
            .iter()
            .filter(|p| !server || p.role == NodeRole::Server)
            .collect();
        let online: Vec<_> = relevant.iter().filter(|p| p.code == "ONLINE").collect();
        if online.is_empty() {
            // Mobile peers come and go; only a dial that should be up and
            // reports a failure code is a connection error.
            let failing = relevant
                .iter()
                .find(|p| p.code != "OFFLINE" && p.role != NodeRole::Mobile);
            return match failing {
                Some(p) if p.code == "NODE_UNREACHABLE" => {
                    (SyncLevel::Error, "Connection error".into())
                }
                Some(_) => (SyncLevel::Error, "Sync error".into()),
                None => (SyncLevel::Offline, "Offline".into()),
            };
        }
        let pending: u64 = online.iter().map(|p| p.pending).sum();
        if pending > 0 {
            (SyncLevel::Syncing, format!("Syncing {pending}"))
        } else {
            (SyncLevel::Synced, "Synced".into())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncLevel {
    Offline,
    Error,
    Syncing,
    Synced,
}

fn journal_stamp(root: &Path) -> Option<u64> {
    crate::core::sync::change_token(root)
}

fn load(
    root: &Path,
    board: &Path,
    reload_conflicts: bool,
    probe: bool,
) -> crate::core::error::Result<Snapshot> {
    Ok(Snapshot {
        board: board.into(),
        peers: probe.then(|| settings::board_peers(root)).transpose()?,
        conflicts: reload_conflicts
            .then(|| load_conflicts(root, board))
            .transpose()?,
    })
}

fn load_conflicts(
    root: &Path,
    board: &Path,
) -> crate::core::error::Result<(Vec<Conflict>, Vec<String>)> {
    let journal = Journal::open(root)?;
    let state = journal.state()?;
    let alias = board.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let project = state.project_uids.get(alias);
    let mut conflicts = Vec::new();
    let mut tasks = Vec::new();
    for conflict in journal.conflicts()? {
        if project != Some(&conflict.project) {
            continue;
        }
        let task = if conflict.entity == "task" {
            Some(conflict.id.clone())
        } else if conflict.entity == "message" {
            let key = serde_json::to_string(&(&conflict.project, "message", &conflict.id))
                .expect("tuple");
            state
                .fields
                .get(&key)
                .and_then(|f| f.values.get("task_id"))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        } else {
            None
        };
        if let Some(task) = task {
            tasks.push(crate::core::sync::task_display(
                &state,
                &conflict.project,
                &task,
            ));
            conflicts.push(conflict);
        }
    }
    Ok((conflicts, tasks))
}

pub(crate) fn render_conflicts(frame: &mut Frame<'_>, app: &mut App, task: &str, area: Rect) {
    let count = app
        .sync_board
        .tasks
        .iter()
        .filter(|id| id.as_str() == task)
        .count();
    if count == 0 {
        return;
    }
    app.sync_board.selected = app.sync_board.selected.min(count - 1);
    let index = app
        .sync_board
        .tasks
        .iter()
        .enumerate()
        .filter(|(_, id)| id.as_str() == task)
        .nth(app.sync_board.selected)
        .expect("selected conflict")
        .0;
    let conflict = &app.sync_board.conflicts[index];
    let lines = [
        format!(
            "⚠ Sync conflict {}/{} · {} / {}",
            app.sync_board.selected + 1,
            count,
            conflict.entity,
            conflict.field
        ),
        format!(
            "Выиграло [{}]: {}",
            conflict.winner.2, conflict.winner_value
        ),
        format!("Проиграло [{}]: {}", conflict.loser.2, conflict.loser_value),
        "[ < ] [ > ] [ Взять другую версию Ctrl+V ]".into(),
    ];
    for (row, text) in lines.iter().enumerate() {
        if row as u16 >= area.height {
            return;
        }
        let slot = Rect::new(area.x, area.y + row as u16, area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(truncate_display(
                &sanitize_terminal_text(text),
                area.width as usize,
            )))
            .style(Style::default().fg(app.theme.warn)),
            slot,
        );
        if row == 3 {
            for (x, width, action) in [
                (0, 5, HitAction::SyncConflictSelect(-1)),
                (6, 5, HitAction::SyncConflictSelect(1)),
                (12, 28, HitAction::SyncConflict(index)),
            ] {
                if x < slot.width {
                    app.hitboxes.push(Hitbox {
                        area: Rect::new(slot.x + x, slot.y, width.min(slot.width - x), 1),
                        action,
                    });
                }
            }
        }
    }
}
