//! Agent work-context for a task.
//!
//! Context lives in the task's sidecar thread as `MessageKind::Context`
//! messages (a sibling of questions and suggestions), keeping the task
//! `description` the user-authored task only. Legacy boards that embedded a
//! `## Context` heading in the description (or spilled it to
//! `.kanban/context/<task>.md`) are still read for back-compat, but new
//! context is never written there.

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::error::Result;
use crate::core::models::{Message, MessageKind, MessageRole};
use crate::core::storage::Storage;
use crate::core::thread::ThreadManager;
use crate::core::timefmt;

pub struct ContextManager {
    pub project_path: PathBuf,
    pub context_dir: PathBuf,
}

pub fn role_for_source(source: &str) -> MessageRole {
    match source.trim().to_lowercase().as_str() {
        "user" | "human" => MessageRole::Human,
        "system" => MessageRole::System,
        _ => MessageRole::Agent,
    }
}

impl ContextManager {
    pub fn new(project_path: impl AsRef<Path>) -> Self {
        let project_path = project_path.as_ref().to_path_buf();
        let context_dir = project_path.join(".kanban").join("context");
        ContextManager {
            project_path,
            context_dir,
        }
    }

    fn context_file(&self, task_id: &str) -> PathBuf {
        self.context_dir.join(format!("{task_id}.md"))
    }

    fn thread_manager(&self) -> Result<ThreadManager> {
        ThreadManager::new(&self.project_path)
    }

    fn format_entry(message: &Message) -> String {
        let author = message
            .author
            .clone()
            .unwrap_or_else(|| message.role.as_str().to_string());
        format!(
            "## Context Entry - {} ({})\n{}\n",
            timefmt::format(&message.created_at),
            author,
            message.body
        )
    }

    /// Post a `context` thread message and refresh the task's `context_size`.
    pub fn append_context(
        &self,
        task_id: &str,
        text: &str,
        source: &str,
        storage: &Storage,
    ) -> Result<()> {
        self.thread_manager()?.post(
            task_id,
            role_for_source(source),
            MessageKind::Context,
            text.trim_matches('\n'),
            None,
            vec![],
            Some(source.to_string()),
        )?;

        // Hold the board lock across load -> save so a concurrent status move
        // (e.g. `kanban done`) can't interleave.
        let _guard = storage.lock()?;
        if let Some(mut task) = storage.load_task(task_id)? {
            let context = self.gather_context(task_id, Some(&task))?;
            task.context_file = None;
            task.context_size = context.len() as u64;
            task.updated_at = timefmt::now();
            storage.save_task(&task)?;
        }
        Ok(())
    }

    fn gather_context(
        &self,
        task_id: &str,
        task: Option<&crate::core::models::Task>,
    ) -> Result<String> {
        let mut parts: Vec<String> = Vec::new();

        // Live context lives in the thread as CONTEXT messages.
        for message in self
            .thread_manager()?
            .messages_of_kind(task_id, MessageKind::Context)?
        {
            parts.push(Self::format_entry(&message));
        }

        // Back-compat: external spill file from older boards.
        let context_file = self.context_file(task_id);
        if context_file.exists()
            && let Ok(legacy) = fs::read_to_string(&context_file)
        {
            let legacy = legacy.trim();
            if !legacy.is_empty() {
                parts.push(legacy.to_string());
            }
        }

        // Back-compat: '## Context' heading embedded in the description.
        if let Some(task) = task
            && let Some(start) = task.description.find("## Context")
        {
            let legacy = task.description[start..].trim();
            if !legacy.is_empty() {
                parts.push(legacy.to_string());
            }
        }

        Ok(parts.join("\n").trim().to_string())
    }

    pub fn get_context(&self, task_id: &str, storage: &Storage) -> Result<String> {
        let task = storage.load_task(task_id)?;
        self.gather_context(task_id, task.as_ref())
    }

    pub fn get_context_summary(
        &self,
        task_id: &str,
        max_length: usize,
        storage: &Storage,
    ) -> Result<String> {
        let context = self.get_context(task_id, storage)?;
        if context.chars().count() > max_length {
            let truncated: String = context.chars().take(max_length).collect();
            Ok(format!("{truncated}\n... (context truncated)"))
        } else {
            Ok(context)
        }
    }

    /// Remove all recorded context (thread messages, legacy spill file, legacy
    /// description block) and zero the task's context counters.
    pub fn clear_context(&self, task_id: &str, storage: &Storage) -> Result<()> {
        self.thread_manager()?
            .remove_kind(task_id, MessageKind::Context)?;

        let context_file = self.context_file(task_id);
        if context_file.exists() {
            let _ = fs::remove_file(&context_file);
        }

        let _guard = storage.lock()?;
        if let Some(mut task) = storage.load_task(task_id)? {
            if let Some(pos) = task.description.find("## Context") {
                task.description = task.description[..pos].trim_end().to_string();
            }
            task.context_file = None;
            task.context_size = 0;
            task.updated_at = timefmt::now();
            storage.save_task(&task)?;
        }
        Ok(())
    }
}
