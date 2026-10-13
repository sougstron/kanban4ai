//! Board sync status and task/message conflict controls. All probes run off-thread.
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

#[derive(Default)]
pub(crate) struct SyncBoard {
    pub peers: Vec<BoardPeer>,
    pub conflicts: Vec<Conflict>,
    pub tasks: Vec<String>,
    pub error: Option<String>,
    pub selected: usize,
    board: PathBuf,
    last_poll: Option<Instant>,
    job: Option<JoinHandle<Result<Snapshot, String>>>,
}

struct Snapshot {
    board: PathBuf,
    peers: Vec<BoardPeer>,
    conflicts: Vec<Conflict>,
    tasks: Vec<String>,
}

impl Drop for SyncBoard {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            let _ = job.join();
        }
    }
}

impl SyncBoard {
    pub fn poll(&mut self, root: &Path, board: &Path, interval: Duration) {
        if self.board != board {
            self.board = board.into();
            self.peers.clear();
            self.conflicts.clear();
            self.tasks.clear();
            self.error = None;
            self.last_poll = None;
        }
        if self.job.as_ref().is_some_and(|job| job.is_finished()) {
            match self.job.take().expect("finished job").join() {
                Ok(Ok(snapshot)) if snapshot.board == self.board => {
                    self.peers = snapshot.peers;
                    self.conflicts = snapshot.conflicts;
                    self.tasks = snapshot.tasks;
                    self.error = None;
                }
                Ok(Err(error)) => self.error = Some(error),
                Err(_) => self.error = Some("sync status worker failed".into()),
                _ => {}
            }
        }
        if self.job.is_some() || self.last_poll.is_some_and(|at| at.elapsed() < interval) {
            return;
        }
        // Legacy in-place boards and stores without sync remain unchanged.
        if !root.join("sync/node.yaml").exists() {
            return;
        }
        self.last_poll = Some(Instant::now());
        let root = root.to_path_buf();
        let board = board.to_path_buf();
        self.job = Some(std::thread::spawn(move || {
            load(&root, &board).map_err(|e| e.to_string())
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

fn load(root: &Path, board: &Path) -> crate::core::error::Result<Snapshot> {
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
            tasks.push(task);
            conflicts.push(conflict);
        }
    }
    Ok(Snapshot {
        board: board.into(),
        peers: settings::board_peers(root)?,
        conflicts,
        tasks,
    })
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
