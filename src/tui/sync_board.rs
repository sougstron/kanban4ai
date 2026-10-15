//! Board sync status and task/message conflict controls. All probes run off-thread.
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Local, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use super::app::{App, HitAction, Hitbox};
use super::card::{sanitize_terminal_text, truncate_display};
use crate::cli::serve::settings::{self, BoardPeer};
use crate::core::sync::{Conflict, Journal};

/// Peer probes go over the network and the serve monitor already runs them
/// every `sync.interval`, so the header refreshes them far less often than
/// the board ticks.
const PEER_PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// `(len, mtime)` of the journal files the conflict list is derived from.
type JournalStamp = [Option<(u64, SystemTime)>; 2];

#[derive(Default)]
pub(crate) struct SyncBoard {
    pub peers: Vec<BoardPeer>,
    pub conflicts: Vec<Conflict>,
    pub tasks: Vec<String>,
    pub error: Option<String>,
    pub selected: usize,
    board: PathBuf,
    last_probe: Option<Instant>,
    journal_stamp: Option<JournalStamp>,
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
        // Legacy in-place boards and stores without sync remain unchanged.
        if !root.join("sync/node.yaml").exists() {
            return;
        }
        let stamp = journal_stamp(root);
        let reload_conflicts = self.journal_stamp != Some(stamp);
        let probe = self
            .last_probe
            .is_none_or(|at| at.elapsed() >= PEER_PROBE_INTERVAL.max(interval));
        if !reload_conflicts && !probe {
            return;
        }
        if reload_conflicts {
            self.journal_stamp = Some(stamp);
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

    pub fn header(&self, now: DateTime<Utc>) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(format!("Sync: {}", sanitize_terminal_text(error)));
        }
        if self.peers.is_empty() {
            return None;
        }
        Some(
            self.peers
                .iter()
                .map(|peer| {
                    let name = sanitize_terminal_text(&peer.name);
                    if peer.code == "ONLINE" && peer.pending == 0 {
                        peer.exchange.map_or_else(
                            || format!("{name}: ожидание синхронизации"),
                            |at| {
                                format!(
                                    "синхронизировано с {name} {} с назад",
                                    (now - at).num_seconds().max(0)
                                )
                            },
                        )
                    } else if peer.code == "ONLINE" {
                        format!("{name}: {} операций ждут", peer.pending)
                    } else if peer.role == crate::core::sync::NodeRole::Mobile {
                        peer.since.or(peer.exchange).map_or_else(
                            || format!("{name}: ещё не был в сети"),
                            |at| {
                                format!(
                                    "{name}: был в сети {}",
                                    at.with_timezone(&Local).format("%H:%M")
                                )
                            },
                        )
                    } else {
                        let since = peer.since.or(peer.exchange).map_or("—".into(), |at| {
                            at.with_timezone(&Local).format("%H:%M").to_string()
                        });
                        format!(
                            "{name}: офлайн с {since}, {} операций ждут · {}",
                            peer.pending, peer.code
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join(" | "),
        )
    }
}

fn journal_stamp(root: &Path) -> JournalStamp {
    ["sync/meta/state.json", "sync/meta/conflicts.jsonl"].map(|file| {
        let meta = std::fs::metadata(root.join(file)).ok()?;
        Some((meta.len(), meta.modified().ok()?))
    })
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
