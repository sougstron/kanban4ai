//! Sidecar thread files (`.kanban/threads/TASK-NNN.yaml`).
//!
//! Saves use an optimistic read-modify-write: the thread remembers the state
//! it was loaded from (`base_rev` / `base_messages`); on save the current file
//! is re-read and, if someone else bumped `rev` meanwhile, the two message
//! lists are merged (additions from both sides survive, locally-changed
//! messages win over their stale base) before writing `rev + 1`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::error::{KanbanError, Result};
use crate::core::models::{Message, MessageKind, MessageRole, MessageStatus, Task, Thread};
use crate::core::storage::atomic_write_text;
use crate::core::timefmt;

pub struct ThreadManager {
    pub project_path: PathBuf,
    pub threads_dir: PathBuf,
}

impl ThreadManager {
    pub fn new(project_path: impl AsRef<Path>) -> Result<Self> {
        let project_path = project_path.as_ref().to_path_buf();
        let threads_dir = project_path.join(".kanban").join("threads");
        fs::create_dir_all(&threads_dir)?;
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
        let mut thread: Thread = if raw.trim().is_empty() {
            Thread::default()
        } else {
            serde_yaml_ng::from_str(&raw)?
        };
        if thread.task_id.is_empty() {
            thread.task_id = task_id.to_string();
        }
        thread.snapshot_base();
        Ok(thread)
    }

    /// Merge-and-write `thread`, then update it in place to the stored state.
    pub fn save(&self, task_id: &str, thread: &mut Thread) -> Result<()> {
        let desired = thread.clone();
        let current = self.load(task_id)?;
        let mut merged = if current.rev == desired.rev {
            desired
        } else {
            merge_threads(&current, &desired)
        };
        merged.task_id = task_id.to_string();
        merged.rev = current.rev + 1;
        atomic_write_text(
            &self.thread_file(task_id),
            &serde_yaml_ng::to_string(&merged)?,
        )?;
        *thread = merged;
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
        let mut thread = self.load(task_id)?;
        let mut message = Message::new(next_msg_id_for_thread(&thread), role, kind, body);
        message.parent_id = parent_id;
        message.variants = variants;
        message.author = author;
        let msg_id = message.id.clone();
        thread.messages.push(message);
        self.save(task_id, &mut thread)?;
        self.get_message(task_id, &msg_id)?.ok_or_else(|| {
            KanbanError::Invalid(format!("Failed to store message {msg_id} for {task_id}"))
        })
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
        self.save(task_id, &mut thread)?;
        self.get_message(task_id, msg_id)?.ok_or_else(|| {
            KanbanError::Invalid(format!("Failed to answer message {msg_id} for {task_id}"))
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
        self.get_message(task_id, msg_id)?.ok_or_else(|| {
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
        atomic_write_text(&thread_file, &serde_yaml_ng::to_string(&thread)?)?;
        Ok(removed)
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

fn merge_threads(current: &Thread, desired: &Thread) -> Thread {
    let mut merged = current.clone();
    for desired_message in &desired.messages {
        match merged
            .messages
            .iter()
            .position(|m| m.id == desired_message.id)
        {
            None => merged.messages.push(desired_message.clone()),
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
