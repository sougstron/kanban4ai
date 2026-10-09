//! Sidecar thread files (`.kanban/threads/TASK-NNN.yaml`).
//!
//! Saves use an optimistic read-modify-write: the thread remembers the state
//! it was loaded from (`base_rev` / `base_messages`); on save the current file
//! is re-read and, if someone else bumped `rev` meanwhile, the two message
//! lists are merged (additions from both sides survive, locally-changed
//! messages win over their stale base) before writing `rev + 1`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::core::error::{KanbanError, Result};
use crate::core::models::{Message, MessageKind, MessageRole, MessageStatus, Task, Thread};
use crate::core::storage::{atomic_write_text, stat_ns};
use crate::core::timefmt;

pub struct ThreadManager {
    pub project_path: PathBuf,
    pub threads_dir: PathBuf,
}

impl ThreadManager {
    pub fn new(project_path: impl AsRef<Path>) -> Result<Self> {
        let project_path = project_path.as_ref().to_path_buf();
        let threads_dir = project_path.join(".kanban").join("threads");
        Ok(ThreadManager {
            project_path,
            threads_dir,
        })
    }

    fn thread_file(&self, task_id: &str) -> PathBuf {
        self.threads_dir.join(format!("{task_id}.yaml"))
    }

    pub fn load(&self, task_id: &str) -> Result<Thread> {
        let thread_file = self.thread_file(task_id);
        if !thread_file.exists() {
            return Ok(Thread::new(task_id));
        }
        let raw = fs::read_to_string(&thread_file)?;
        let mut thread = parse_thread(&raw, task_id)?;
        thread.snapshot_base();
        Ok(thread)
    }

    /// Merge-and-write `thread`, then update it in place to the stored state.
    ///
    /// When the stored `rev` still equals the one `thread` was loaded at,
    /// nobody wrote in between and the merge is a no-op, so only the
    /// top-level `rev:` line is read instead of parsing the whole file
    /// again; a busy thread is otherwise parsed twice per write.
    pub fn save(&self, task_id: &str, thread: &mut Thread) -> Result<()> {
        let thread_file = self.thread_file(task_id);
        let raw = match fs::read_to_string(&thread_file) {
            Ok(raw) => Some(raw),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        };
        let unchanged = match raw.as_deref() {
            None => thread.rev == 0,
            Some(raw) => peek_rev(raw) == Some(thread.rev),
        };
        fs::create_dir_all(&self.threads_dir)?;
        if unchanged {
            thread.task_id = task_id.to_string();
            thread.rev += 1;
            if let Err(err) =
                serialize_thread(thread).and_then(|text| atomic_write_text(&thread_file, &text))
            {
                thread.rev -= 1;
                return Err(err);
            }
        } else {
            let current = match raw.as_deref() {
                Some(raw) => parse_thread(raw, task_id)?,
                None => Thread::new(task_id),
            };
            let mut merged = merge_threads(current, thread);
            merged.task_id = task_id.to_string();
            merged.rev += 1;
            atomic_write_text(&thread_file, &serialize_thread(&merged)?)?;
            *thread = merged;
        }
        thread.snapshot_base();
        Ok(())
    }

    pub fn next_msg_id(&self, task_id: &str) -> Result<String> {
        Ok(next_msg_id_for_thread(&self.load(task_id)?))
    }

    /// Persist the task's opening system/task messages in its sidecar thread.
    pub fn initialize_task_thread(&self, task: &Task) -> Result<()> {
        let mut thread = self.load(&task.id)?;
        if !thread.messages.is_empty() {
            return Ok(());
        }

        let mut system_message = Message::new(
            next_msg_id_for_thread(&thread),
            MessageRole::System,
            MessageKind::System,
            format!(
                "Task created: {}\nStatus: {}\nCreated at: {}",
                task.title,
                task.status,
                timefmt::format(&task.created_at)
            ),
        );
        system_message.author = Some("kanban".to_string());
        system_message.created_at = task.created_at;
        system_message.updated_at = task.created_at;
        thread.messages.push(system_message);

        let mut task_message = Message::new(
            next_msg_id_for_thread(&thread),
            MessageRole::Human,
            MessageKind::Task,
            task_body_of(task),
        );
        task_message.author = Some("user".to_string());
        task_message.created_at = task.created_at;
        task_message.updated_at = task.created_at;
        thread.messages.push(task_message);

        self.save(&task.id, &mut thread)
    }

    /// Keep the user-authored task message aligned with the task description.
    pub fn update_task_message(&self, task: &Task) -> Result<()> {
        let mut thread = self.load(&task.id)?;
        let task_body = task_body_of(task);
        let now = timefmt::now();

        if let Some(message) = thread
            .messages
            .iter_mut()
            .find(|m| m.kind == MessageKind::Task)
        {
            if message.body == task_body {
                return Ok(());
            }
            message.body = task_body;
            message.updated_at = now;
            return self.save(&task.id, &mut thread);
        }

        if thread.messages.is_empty() {
            return self.initialize_task_thread(task);
        }

        let mut message = Message::new(
            next_msg_id_for_thread(&thread),
            MessageRole::Human,
            MessageKind::Task,
            task_body,
        );
        message.author = Some("user".to_string());
        message.created_at = now;
        message.updated_at = now;
        thread.messages.push(message);
        self.save(&task.id, &mut thread)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn post(
        &self,
        task_id: &str,
        role: MessageRole,
        kind: MessageKind,
        body: &str,
        parent_id: Option<String>,
        variants: Vec<String>,
        author: Option<String>,
    ) -> Result<Message> {
        self.post_with_origin(task_id, role, kind, body, parent_id, variants, author, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn post_with_origin(
        &self,
        task_id: &str,
        role: MessageRole,
        kind: MessageKind,
        body: &str,
        parent_id: Option<String>,
        variants: Vec<String>,
        author: Option<String>,
        origin: Option<String>,
    ) -> Result<Message> {
        let mut thread = self.load(task_id)?;
        let mut message = Message::new(next_msg_id_for_thread(&thread), role, kind, body);
        message.parent_id = parent_id;
        message.variants = variants;
        message.author = author;
        message.origin = origin;
        let msg_id = message.id.clone();
        thread.messages.push(message);
        self.save(task_id, &mut thread)?;
        take_message(thread, &msg_id).ok_or_else(|| {
            KanbanError::Invalid(format!("Failed to store message {msg_id} for {task_id}"))
        })
    }

    /// Post a board-authored `agent_step` note unless it repeats the last
    /// message of the thread (ignoring a collapsed `(×N)` counter). Returns
    /// `None` when the note was a repeat and nothing was written. Board notes
    /// come from every process that pumps the store, so an in-process dedupe
    /// is not enough to keep a stuck condition from flooding the thread.
    pub fn post_board_note(&self, task_id: &str, body: &str) -> Result<Option<Message>> {
        let mut thread = self.load(task_id)?;
        if thread
            .messages
            .last()
            .is_some_and(|last| is_board_note(last) && strip_repeat(&last.body) == body)
        {
            return Ok(None);
        }
        let mut message = Message::new(
            next_msg_id_for_thread(&thread),
            MessageRole::System,
            MessageKind::AgentStep,
            body,
        );
        message.author = Some(BOARD_AUTHOR.to_string());
        message.origin = Some(BOARD_AUTHOR.to_string());
        let msg_id = message.id.clone();
        thread.messages.push(message);
        self.save(task_id, &mut thread)?;
        Ok(take_message(thread, &msg_id))
    }

    pub fn answer(
        &self,
        task_id: &str,
        msg_id: &str,
        answer: &str,
        role: MessageRole,
    ) -> Result<Message> {
        let mut thread = self.load(task_id)?;
        let message = require_message(&mut thread, msg_id)?;
        let now = timefmt::now();
        message.answer = Some(answer.to_string());
        message.answered_by_role = Some(role);
        message.status = MessageStatus::Answered;
        message.updated_at = now;
        message.resolved_at = Some(now);
        message.draft.clear();
        if role == MessageRole::Human && message.origin.is_none() {
            message.origin = Some("human".to_string());
        }
        self.save(task_id, &mut thread)?;
        take_message(thread, msg_id).ok_or_else(|| {
            KanbanError::Invalid(format!("Failed to answer message {msg_id} for {task_id}"))
        })
    }

    /// Stash (or clear, with an empty `draft`) the human's in-progress typed
    /// answer on a question so the TUI can restore it after a close/reopen.
    pub fn set_draft(&self, task_id: &str, msg_id: &str, draft: &str) -> Result<Message> {
        let mut thread = self.load(task_id)?;
        let message = require_message(&mut thread, msg_id)?;
        message.draft = draft.to_string();
        message.updated_at = timefmt::now();
        self.save(task_id, &mut thread)?;
        take_message(thread, msg_id).ok_or_else(|| {
            KanbanError::Invalid(format!("Failed to store draft on {msg_id} for {task_id}"))
        })
    }

    pub fn resolve(&self, task_id: &str, msg_id: &str, status: MessageStatus) -> Result<Message> {
        let mut thread = self.load(task_id)?;
        let message = require_message(&mut thread, msg_id)?;
        let now = timefmt::now();
        message.status = status;
        message.updated_at = now;
        message.resolved_at = if status == MessageStatus::Open {
            None
        } else {
            Some(now)
        };
        self.save(task_id, &mut thread)?;
        take_message(thread, msg_id).ok_or_else(|| {
            KanbanError::Invalid(format!("Failed to resolve message {msg_id} for {task_id}"))
        })
    }

    pub fn get_message(&self, task_id: &str, msg_id: &str) -> Result<Option<Message>> {
        Ok(self
            .load(task_id)?
            .messages
            .into_iter()
            .find(|m| m.id == msg_id))
    }

    /// Open messages; with no `kind` filter only questions and suggestions
    /// are returned (matching the Python behavior).
    pub fn open_messages(&self, task_id: &str, kind: Option<MessageKind>) -> Result<Vec<Message>> {
        let thread = self.load(task_id)?;
        Ok(thread
            .messages
            .into_iter()
            .filter(|m| m.status == MessageStatus::Open)
            .filter(|m| match kind {
                Some(kind) => m.kind == kind,
                None => matches!(m.kind, MessageKind::Question | MessageKind::Suggestion),
            })
            .collect())
    }

    pub fn has_open_questions(&self, task_id: &str) -> Result<bool> {
        Ok(!self
            .open_messages(task_id, Some(MessageKind::Question))?
            .is_empty())
    }

    pub fn messages_of_kind(&self, task_id: &str, kind: MessageKind) -> Result<Vec<Message>> {
        Ok(self
            .load(task_id)?
            .messages
            .into_iter()
            .filter(|m| m.kind == kind)
            .collect())
    }

    /// `(mtime, size)` of the thread file, or `None` when it does not exist;
    /// lets callers skip re-parsing an unchanged thread.
    pub fn stamp(&self, task_id: &str) -> Option<(u128, u64)> {
        stat_ns(&self.thread_file(task_id))
    }

    /// Delete every message of `kind`, returning the count removed.
    ///
    /// Deletion bypasses the merge-on-save (which only ever adds/updates
    /// messages) and writes directly under a fresh load + rev bump, so a
    /// concurrent add can't resurrect a message we just cleared.
    pub fn remove_kind(&self, task_id: &str, kind: MessageKind) -> Result<usize> {
        let thread_file = self.thread_file(task_id);
        if !thread_file.exists() {
            return Ok(0);
        }
        let mut thread = self.load(task_id)?;
        let before = thread.messages.len();
        thread.messages.retain(|m| m.kind != kind);
        let removed = before - thread.messages.len();
        if removed == 0 {
            return Ok(0);
        }
        thread.rev += 1;
        atomic_write_text(&thread_file, &serialize_thread(&thread)?)?;
        Ok(removed)
    }

    /// Collapse every run of consecutive identical board notes (see
    /// [`Self::post_board_note`]) into its first message, carrying a `(×N)`
    /// counter and the last repeat's `updated_at`. Messages another message
    /// replies to are kept as they are. Returns `(before, after)` message
    /// counts, or `None` when there was nothing to collapse.
    ///
    /// Like [`Self::remove_kind`] this writes directly under a rev bump:
    /// merge-on-save would resurrect the dropped repeats.
    pub fn compact_repeats(&self, task_id: &str) -> Result<Option<(usize, usize)>> {
        let thread_file = self.thread_file(task_id);
        if !thread_file.exists() {
            return Ok(None);
        }
        let mut thread = self.load(task_id)?;
        let before = thread.messages.len();
        let referenced: HashSet<String> = thread
            .messages
            .iter()
            .filter_map(|m| m.parent_id.clone())
            .collect();
        let collapsible = |m: &Message| is_board_note(m) && !referenced.contains(&m.id);
        let mut kept: Vec<Message> = Vec::with_capacity(before);
        let mut run = 1usize;
        for message in std::mem::take(&mut thread.messages) {
            if let Some(head) = kept.last_mut()
                && collapsible(head)
                && collapsible(&message)
                && strip_repeat(&head.body) == strip_repeat(&message.body)
            {
                run += repeat_count(&message.body);
                head.body = format!("{} (×{run})", strip_repeat(&head.body));
                head.updated_at = head.updated_at.max(message.updated_at);
                continue;
            }
            run = repeat_count(&message.body);
            kept.push(message);
        }
        if kept.len() == before {
            return Ok(None);
        }
        thread.messages = kept;
        thread.rev += 1;
        atomic_write_text(&thread_file, &serialize_thread(&thread)?)?;
        Ok(Some((before, thread.messages.len())))
    }

    /// [`Self::compact_repeats`] over every thread of the board; returns
    /// `(task_id, before, after)` for each thread that shrank.
    pub fn compact_all_repeats(&self) -> Result<Vec<(String, usize, usize)>> {
        let entries = match fs::read_dir(&self.threads_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut task_ids: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                Some(name.strip_suffix(".yaml")?.to_string())
            })
            .collect();
        task_ids.sort();
        let mut compacted = Vec::new();
        for task_id in task_ids {
            if let Some((before, after)) = self.compact_repeats(&task_id)? {
                compacted.push((task_id, before, after));
            }
        }
        Ok(compacted)
    }

    /// Drop the whole sidecar thread of `task_id`.
    ///
    /// Task ids are recycled (`Storage::get_next_id` hands out `max + 1`), so a
    /// thread left behind by a deleted task would otherwise be adopted by the
    /// next task that lands on the same id.
    pub fn discard_thread(&self, task_id: &str) -> Result<bool> {
        let thread_file = self.thread_file(task_id);
        if !thread_file.exists() {
            return Ok(false);
        }
        fs::remove_file(&thread_file)?;
        Ok(true)
    }
}

fn task_body_of(task: &Task) -> String {
    let trimmed = task.description.trim();
    if trimmed.is_empty() {
        "(no description provided)".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Author and origin of notes the board itself posts.
const BOARD_AUTHOR: &str = "kanban";

fn is_board_note(message: &Message) -> bool {
    message.kind == MessageKind::AgentStep
        && message.author.as_deref() == Some(BOARD_AUTHOR)
        && message.answer.is_none()
}

/// `body` without the ` (×N)` counter [`ThreadManager::compact_repeats`] appends.
fn strip_repeat(body: &str) -> &str {
    split_repeat(body).map_or(body, |(base, _)| base)
}

/// How many notes `body` stands for: its ` (×N)` counter, or 1.
fn repeat_count(body: &str) -> usize {
    split_repeat(body).map_or(1, |(_, count)| count)
}

fn split_repeat(body: &str) -> Option<(&str, usize)> {
    let (base, tail) = body.strip_suffix(')')?.rsplit_once(" (×")?;
    Some((base, tail.parse().ok().filter(|n| *n > 1)?))
}

fn parse_thread(raw: &str, task_id: &str) -> Result<Thread> {
    let mut thread: Thread = if raw.trim().is_empty() {
        Thread::default()
    } else {
        serde_yaml_ng::from_str(raw)?
    };
    if thread.task_id.is_empty() {
        thread.task_id = task_id.to_string();
    }
    Ok(thread)
}

/// The top-level `rev:` of a serialized thread without parsing the
/// messages. Message fields are indented under the `messages:` sequence, so
/// an unindented `rev:` line can only be the thread's own. `None` when the
/// line is missing or unreadable; the caller then parses the whole file.
fn peek_rev(raw: &str) -> Option<u64> {
    raw.lines()
        .find_map(|line| line.strip_prefix("rev:"))
        .and_then(|value| value.trim().trim_matches(['\'', '"']).parse().ok())
}

fn take_message(thread: Thread, msg_id: &str) -> Option<Message> {
    thread.messages.into_iter().find(|m| m.id == msg_id)
}

fn merge_threads(current: Thread, desired: &Thread) -> Thread {
    let mut merged = current;
    let mut index: HashMap<String, usize> = merged
        .messages
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i))
        .collect();
    for desired_message in &desired.messages {
        match index.get(&desired_message.id).copied() {
            None => {
                index.insert(desired_message.id.clone(), merged.messages.len());
                merged.messages.push(desired_message.clone());
            }
            Some(index) => {
                // Only overwrite the concurrent copy if we actually changed
                // this message relative to the state we loaded it from.
                let base = desired.base_messages.get(&desired_message.id);
                if base != Some(desired_message) {
                    merged.messages[index] = desired_message.clone();
                }
            }
        }
    }
    merged.messages.sort_by_key(message_sort_key);
    merged
}

fn serialize_thread(thread: &Thread) -> Result<String> {
    let yaml = serde_yaml_ng::to_string(thread)?;
    Ok(timefmt::quote_yaml_timestamp_fields(
        &yaml,
        &["created_at", "updated_at", "resolved_at"],
    ))
}

fn next_msg_id_for_thread(thread: &Thread) -> String {
    let max_num = thread
        .messages
        .iter()
        .filter_map(|m| msg_number(&m.id))
        .max()
        .unwrap_or(0);
    format!("MSG-{:03}", max_num + 1)
}

fn msg_number(id: &str) -> Option<u64> {
    id.strip_prefix("MSG-")?.parse().ok()
}

fn message_sort_key(message: &Message) -> (u64, String) {
    match msg_number(&message.id) {
        Some(num) => (num, message.id.clone()),
        None => (1_000_000_000, message.id.clone()),
    }
}

fn require_message<'a>(thread: &'a mut Thread, msg_id: &str) -> Result<&'a mut Message> {
    thread
        .messages
        .iter_mut()
        .find(|m| m.id == msg_id)
        .ok_or_else(|| KanbanError::Invalid(format!("Message not found: {msg_id}")))
}
