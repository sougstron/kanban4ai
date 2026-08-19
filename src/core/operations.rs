//! The single business-logic hub. Both the CLI and the TUI go through this
//! module; it owns CRUD, move/rule enforcement, questions, review edits,
//! chaining, and delegates agent process launching to an [`AgentLauncher`].

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::Regex;

use crate::agent::{KanbanLauncher, build_agent_prompt, resolve_launch_settings};
use crate::core::ask_form::AskForm;
use crate::core::config::Config;
use crate::core::context::{ContextManager, role_for_source};
use crate::core::error::{KanbanError, Result};
use crate::core::models::{
    Message, MessageKind, MessageRole, MessageStatus, SessionStatus, Task, TaskStatus,
};
use crate::core::notifier::{DesktopNotifier, NotificationConfig};
use crate::core::project::{Project, Roots};
use crate::core::provenance::{
    self, ClaudeHarvester, InputManifest, OpencodeHarvester, PiFamilyHarvester, TranscriptHarvester,
};
use crate::core::reply;
use crate::core::session::SessionManager;
use crate::core::storage::{NewTask, Storage, atomic_write_text};
use crate::core::thread::ThreadManager;
use crate::core::timefmt;

/// Seam for spawning the actual agent process. Tests inject a recording stub;
/// production uses the configured launcher in [`Operations::new`].
///
/// The launcher receives both roots: board files come from `data_root`, the
/// spawned process runs in `work_path`.
pub trait AgentLauncher {
    fn launch(&self, roots: Roots<'_>, task: &Task, session_id: &str, revert: bool)
    -> Result<bool>;
}

/// Test and fallback launcher that deliberately does not spawn processes.
pub struct NoopLauncher;

impl AgentLauncher for NoopLauncher {
    fn launch(
        &self,
        _roots: Roots<'_>,
        _task: &Task,
        _session_id: &str,
        _revert: bool,
    ) -> Result<bool> {
        Ok(false)
    }
}

/// Field-wise task update; `Some(new_value)` applies, `None` leaves untouched.
/// Option-valued task fields use `Some(None)` to clear.
#[derive(Debug, Default, Clone)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub ai_model: Option<Option<String>>,
    pub ai_effort: Option<Option<String>>,
    pub agent_backend: Option<Option<String>>,
    pub agent_name: Option<Option<String>>,
    pub interactive: Option<bool>,
    pub chained_to: Option<Option<String>>,
    pub session: Option<Option<String>>,
}

#[derive(Debug, Clone)]
pub enum QuestionRef {
    Index(usize),
    MsgId(String),
}

/// What [`Operations::reconcile_agent_exit`] decided about an exited agent
/// process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentExitOutcome {
    /// Non-zero exit: the session was marked crashed.
    Crashed,
    /// Clean exit with nothing left to do (task completed, moved on, or
    /// waiting for a human answer): the session was closed.
    Closed,
    /// Clean exit inside a declared, unexpired wait: the session stays alive
    /// until its wait deadline relaunches the agent.
    Waiting,
    /// Clean exit that stranded an In Progress task: the agent was relaunched
    /// on the returned session id.
    Resumed(String),
    /// Stranded task, but the auto-resume budget is spent: the session was
    /// marked crashed and the user notified.
    ResumeExhausted,
    /// The task was claimed for a resume session, but launching the agent
    /// failed; the new session was marked crashed.
    LaunchFailed(String),
}

impl AgentExitOutcome {
    /// Short human-readable word for the `AgentStep` exit audit line.
    fn label(&self) -> String {
        match self {
            AgentExitOutcome::Crashed => "Crashed".to_string(),
            AgentExitOutcome::Closed => "Closed".to_string(),
            AgentExitOutcome::Waiting => "Waiting".to_string(),
            AgentExitOutcome::Resumed(session) => format!("Resumed({session})"),
            AgentExitOutcome::ResumeExhausted => "ResumeExhausted".to_string(),
            AgentExitOutcome::LaunchFailed(session) => format!("LaunchFailed({session})"),
        }
    }
}

enum RespawnOutcome {
    Spawned(String),
    Noop,
    LaunchFailed(String),
}

/// A long-running command started by [`Operations::detach_command`]: fully
/// detached from the agent session, with output and exit status recorded on
/// disk so the relaunched agent can check the result.
#[derive(Debug, Clone)]
pub struct DetachedJob {
    pub pid: u32,
    pub log_file: PathBuf,
    pub status_file: PathBuf,
    pub deadline: chrono::NaiveDateTime,
}

pub struct Operations {
    pub storage: Storage,
    pub config: Config,
    /// Where agents run. Equal to the data root for a board used in place;
    /// the registered project's code folder once the board lives in the store.
    work_path: PathBuf,
    /// Registered project id, when this board came from the store.
    project_id: Option<String>,
    launcher: Box<dyn AgentLauncher>,
}

impl Operations {
    pub fn new(project_path: impl AsRef<Path>) -> Self {
        Self::with_launcher(project_path, Box::new(KanbanLauncher))
    }

    pub fn with_launcher(project_path: impl AsRef<Path>, launcher: Box<dyn AgentLauncher>) -> Self {
        let project_path = project_path.as_ref();
        Operations {
            storage: Storage::new(project_path),
            config: Config::new(project_path),
            work_path: project_path.to_path_buf(),
            project_id: None,
            launcher,
        }
    }

    /// Operations on a registered project: board data in the store, agents
    /// launched in the project's code folder.
    pub fn for_project(project: &Project) -> Self {
        Self::for_project_with_launcher(project, Box::new(KanbanLauncher))
    }

    pub fn for_project_with_launcher(project: &Project, launcher: Box<dyn AgentLauncher>) -> Self {
        Operations {
            storage: Storage::new(&project.data_root),
            config: Config::new(&project.data_root),
            work_path: project.work_path.clone(),
            project_id: Some(project.id.clone()),
            launcher,
        }
    }

    /// The board data root: everything under `.kanban` hangs off this.
    pub fn data_root(&self) -> &Path {
        &self.storage.project_path
    }

    /// The folder agents are launched in, and the only path a user's code is
    /// ever read from or written to.
    pub fn work_path(&self) -> &Path {
        &self.work_path
    }

    pub fn roots(&self) -> Roots<'_> {
        Roots::new(
            self.data_root(),
            self.work_path(),
            self.project_id.as_deref(),
        )
    }

    fn thread_manager(&self) -> Result<ThreadManager> {
        ThreadManager::new(self.data_root())
    }

    fn session_manager(&self) -> SessionManager {
        SessionManager::new(self.data_root())
    }

    fn context_manager(&self) -> ContextManager {
        ContextManager::new(self.data_root())
    }

    fn notifier(&self) -> Result<DesktopNotifier> {
        let mapping = self.config.get_notifications()?;
        Ok(DesktopNotifier::new(NotificationConfig::from_mapping(
            &mapping,
        )))
    }

    fn notify_question(&self, task: &Task, question: &str) {
        if let Ok(notifier) = self.notifier() {
            notifier.question(&task.id, &task.title, question);
        }
    }

    fn notify_completion(&self, task: &Task) {
        if let Ok(notifier) = self.notifier() {
            let status = if task.status == TaskStatus::Review {
                "ready for review".to_string()
            } else {
                task.status.to_string()
            };
            notifier.completion(&task.id, &task.title, &status);
        }
    }

    fn notify_chained_start(&self, task: &Task, target_task_id: &str) {
        if let Ok(notifier) = self.notifier() {
            notifier.chained_start(&task.id, &task.title, target_task_id);
        }
    }

    // ------------------------------------------------------------------ CRUD

    pub fn create_task(&self, new_task: NewTask) -> Result<Task> {
        self.storage.create_task(new_task)
    }

    /// Create directly in the requested board column, without exposing an
    /// intermediate To Do task to other board users.
    pub fn create_task_in_status(&self, new_task: NewTask, status: TaskStatus) -> Result<Task> {
        let task = self.storage.create_task_in_status(new_task, status)?;
        if status == TaskStatus::Review {
            self.trigger_chained_tasks(&task.id)?;
        }
        Ok(task)
    }

    pub fn update_task(&self, task_id: &str, patch: TaskPatch) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(None);
        };
        if let Some(title) = patch.title {
            task.title = title;
        }
        if let Some(description) = patch.description {
            task.description = description;
        }
        if let Some(status) = patch.status {
            let previous_status = task.status;
            let next_status = status.parse()?;
            task.status = next_status;
            if previous_status != next_status
                && matches!(next_status, TaskStatus::Review | TaskStatus::Done)
            {
                task.completed_at = Some(timefmt::now());
            }
        }
        if let Some(ai_model) = patch.ai_model {
            task.ai_model = ai_model;
        }
        if let Some(ai_effort) = patch.ai_effort {
            task.ai_effort = ai_effort;
        }
        if let Some(agent_backend) = patch.agent_backend {
            task.agent_backend = agent_backend;
        }
        if let Some(agent_name) = patch.agent_name {
            task.agent_name = agent_name;
        }
        if let Some(interactive) = patch.interactive {
            task.interactive = interactive;
        }
        if let Some(chained_to) = patch.chained_to {
            task.chained_to = chained_to;
        }
        if let Some(session) = patch.session {
            task.session = session;
        }
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        Ok(Some(task))
    }

    pub fn delete_task(&self, task_id: &str) -> Result<bool> {
        self.storage.delete_task(task_id)
    }

    /// Delete a task together with its thread, assets, context, backups, and
    /// sessions.
    pub fn abandon_task(&self, task_id: &str) -> Result<bool> {
        let Some(task) = self.storage.load_task(task_id)? else {
            return Ok(false);
        };
        self.clear_task_assets(&task);
        self.context_manager()
            .clear_context(&task.id, &self.storage)?;
        self.clear_task_backups(&task.id);
        self.clear_task_logs_and_sessions(&task);
        let deleted = self.storage.delete_task(&task.id)?;
        // After the task file is gone, so that nothing rewrites the sidecar:
        // ids are recycled, and a surviving thread would resurface under the
        // next task that gets this id.
        self.thread_manager()?.discard_thread(&task.id)?;
        Ok(deleted)
    }

    /// Abandon in-progress tasks whose session died without leaving questions.
    pub fn abandon_stalled_tasks(&self) -> Result<Vec<Task>> {
        let session_mgr = self.session_manager();
        let tm = self.thread_manager()?;
        let mut abandoned = Vec::new();
        for task in self
            .storage
            .list_tasks(Some(TaskStatus::InProgress.as_str()))?
        {
            let session_alive = task
                .session
                .as_deref()
                .is_some_and(|s| session_mgr.is_session_active(s));
            if task.session.is_none() || session_alive {
                continue;
            }
            // A task with open questions is waiting for an answer, not stalled.
            if tm.has_open_questions(&task.id)? {
                continue;
            }
            if self.abandon_task(&task.id)? {
                abandoned.push(task);
            }
        }
        Ok(abandoned)
    }

    pub fn get_task(&self, task_id: &str) -> Result<Option<Task>> {
        self.storage.load_task(task_id)
    }

    pub fn list_tasks(
        &self,
        status: Option<&str>,
        search: Option<&str>,
        sort_by: &str,
        order: &str,
    ) -> Result<Vec<Task>> {
        let mut tasks = self.storage.list_tasks(status)?;

        if let Some(search) = search {
            let query = search.to_lowercase();
            tasks.retain(|t| {
                t.title.to_lowercase().contains(&query)
                    || t.description.to_lowercase().contains(&query)
            });
        }

        sort_tasks(&mut tasks, sort_by, order);
        Ok(tasks)
    }

    pub fn list_archived_tasks(&self, search: Option<&str>) -> Result<Vec<Task>> {
        self.list_tasks(
            Some(TaskStatus::Archive.as_str()),
            search,
            "updated",
            "desc",
        )
    }

    /// Recover a task to To Do as one locked read-modify-write cycle. Used by
    /// both CLI and TUI. The stale session id stays on the task as the record
    /// of its last session; liveness is decided by the session record, not by
    /// this field being set.
    pub fn recover_task(&self, task_id: &str) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(None);
        };
        task.status = TaskStatus::Todo;
        task.auto_resumes = 0;
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        Ok(Some(task))
    }

    /// Restore an archived task to To Do. Returns `None` when the task is
    /// missing or not archived.
    pub fn unarchive_task(&self, task_id: &str) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(None);
        };
        if task.status != TaskStatus::Archive {
            return Ok(None);
        }
        task.status = TaskStatus::Todo;
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        Ok(Some(task))
    }

    // ------------------------------------------------------------------ moves

    pub fn move_task(
        &self,
        task_id: &str,
        target_status: &str,
        is_agent: bool,
    ) -> Result<Option<Task>> {
        let (moved, entered_review) = {
            let _guard = self.storage.lock()?;
            let Some(task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };

            let config = self.config.load()?;
            let mut valid: Vec<String> = config.column_ids();
            let archive = TaskStatus::Archive.as_str().to_string();
            if !valid.contains(&archive) {
                valid.push(archive);
            }
            if !valid.iter().any(|s| s == target_status) {
                return Err(KanbanError::Invalid(format!(
                    "Invalid status '{}'. Valid: {}",
                    target_status,
                    valid.join(", ")
                )));
            }

            if is_agent && self.config.get_rule("user_only_review_to_done")? {
                if target_status == TaskStatus::Done.as_str() {
                    return Err(KanbanError::Permission(
                        "Agent cannot move tasks to Done".to_string(),
                    ));
                }
                if task.status == TaskStatus::Review {
                    return Err(KanbanError::Permission(
                        "Agent cannot move tasks from Review".to_string(),
                    ));
                }
            }

            if target_status == TaskStatus::Done.as_str() {
                let done = self.move_task_to_done(&task)?;
                (done, false)
            } else {
                let moved = self.storage.move_task(task_id, target_status, is_agent)?;
                let entered_review = moved.as_ref().is_some_and(|m| {
                    task.status != TaskStatus::Review && m.status == TaskStatus::Review
                });
                (moved, entered_review)
            }
        };

        if entered_review && let Some(moved) = &moved {
            self.trigger_chained_tasks(&moved.id)?;
        }
        Ok(moved)
    }

    /// Move every task in `from` to `to` as one locked batch, with human-mode
    /// rules (bulk moves are a board-owner action). Moving into Done runs the
    /// per-task cleanup path; tasks that entered Review fire their chained
    /// tasks after the batch commits.
    pub fn bulk_move(&self, from: TaskStatus, to: TaskStatus) -> Result<Vec<Task>> {
        if from == to {
            return Ok(Vec::new());
        }
        let moved = {
            let _guard = self.storage.lock()?;
            let tasks = self.storage.list_tasks(Some(from.as_str()))?;
            self.move_tasks_locked(tasks, to)?
        };
        self.trigger_bulk_review_chains(to, &moved)?;
        Ok(moved)
    }

    /// Move exactly the set the user confirmed. `None` means the source
    /// column changed while its confirmation dialog was open.
    pub fn bulk_move_exact(
        &self,
        from: TaskStatus,
        to: TaskStatus,
        expected_ids: &[String],
    ) -> Result<Option<Vec<Task>>> {
        if from == to {
            return Ok(Some(Vec::new()));
        }
        let moved = {
            let _guard = self.storage.lock()?;
            let tasks = self.storage.list_tasks(Some(from.as_str()))?;
            let mut current_ids = tasks.iter().map(|task| task.id.clone()).collect::<Vec<_>>();
            let mut confirmed_ids = expected_ids.to_vec();
            current_ids.sort();
            confirmed_ids.sort();
            if current_ids != confirmed_ids {
                return Ok(None);
            }
            self.move_tasks_locked(tasks, to)?
        };
        self.trigger_bulk_review_chains(to, &moved)?;
        Ok(Some(moved))
    }

    fn move_tasks_locked(&self, tasks: Vec<Task>, to: TaskStatus) -> Result<Vec<Task>> {
        let mut moved = Vec::new();
        for task in tasks {
            let result = if to == TaskStatus::Done {
                self.move_task_to_done(&task)?
            } else {
                self.storage.move_task(&task.id, to.as_str(), false)?
            };
            moved.extend(result);
        }
        Ok(moved)
    }

    fn trigger_bulk_review_chains(&self, to: TaskStatus, moved: &[Task]) -> Result<()> {
        if to == TaskStatus::Review {
            for task in moved {
                self.trigger_chained_tasks(&task.id)?;
            }
        }
        Ok(())
    }

    pub fn archive_done_tasks(&self) -> Result<Vec<Task>> {
        self.bulk_move(TaskStatus::Done, TaskStatus::Archive)
    }

    pub fn mark_review_tasks_done(&self) -> Result<Vec<Task>> {
        self.bulk_move(TaskStatus::Review, TaskStatus::Done)
    }

    /// Clear per-task artifacts and move the task to Done. Callers may already
    /// hold the board lock — the lock is reentrant per thread. The session id
    /// stays on the task even though the session's files are gone: it is the
    /// record of which session last worked the task.
    fn move_task_to_done(&self, task: &Task) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        self.clear_task_assets(task);
        self.context_manager()
            .clear_context(&task.id, &self.storage)?;
        self.clear_task_backups(&task.id);
        self.clear_task_logs_and_sessions(task);

        let Some(mut current) = self.storage.load_task(&task.id)? else {
            return Ok(None);
        };
        let entered_done = current.status != TaskStatus::Done;
        current.status = TaskStatus::Done;
        current.review_unseen = false;
        let now = timefmt::now();
        current.updated_at = now;
        if entered_done {
            current.completed_at = Some(now);
        }
        self.storage.save_task(&current)?;
        Ok(Some(current))
    }

    // ------------------------------------------------------- take / complete

    pub fn take_task(
        &self,
        task_id: &str,
        session_id: &str,
        is_agent: bool,
    ) -> Result<Option<Task>> {
        let session_mgr = self.session_manager();
        SessionManager::validate_session_id(session_id)?;
        let (task, previous_status) = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            let previous_status = task.status;

            if is_agent
                && self.config.get_rule("one_task_per_instance")?
                && session_mgr
                    .list_active_sessions()
                    .iter()
                    .any(|s| s.id == session_id && s.task_id != task_id)
            {
                return Ok(None);
            }

            task.session = Some(session_id.to_string());
            task.auto_resumes = 0;
            if self.config.get_rule("auto_move_on_assign")? {
                task.status = TaskStatus::InProgress;
            }
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            session_mgr.link_named_session(task_id, session_id, &task.title)?;
            (task, previous_status)
        };

        if is_agent
            && self.config.get_rule("auto_launch_on_delegate")?
            && self.auto_launch_enabled()?
        {
            match self.finish_launch(session_id, self.launch_agent(task_id, session_id, false)) {
                Ok(true) => {}
                failed => {
                    // The status rolls back, but the crashed session stays on the task:
                    // it is the last session that was assigned to it.
                    let _guard = self.storage.lock()?;
                    if let Some(mut current) = self.storage.load_task(task_id)? {
                        current.status = previous_status;
                        current.updated_at = timefmt::now();
                        self.storage.save_task(&current)?;
                    }
                    failed?;
                    return Ok(None);
                }
            }
        }

        Ok(Some(task))
    }

    /// The human "Run" action: start a task on a fresh agent session
    /// immediately, with no confirmation flow. Returns the new session id,
    /// `Ok(None)` when the task does not exist, and an error when the task is
    /// already running or the agent fails to launch.
    pub fn start_task(&self, task_id: &str) -> Result<Option<String>> {
        let session_mgr = self.session_manager();
        let backend = {
            let _guard = self.storage.lock()?;
            let Some(task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            if let Some(session) = task.session.as_deref()
                && session_mgr.is_session_active(session)
            {
                return Err(KanbanError::Invalid(format!(
                    "Task {task_id} is already running (session {session})"
                )));
            }
            self.resolve_backend(&task)?
        };
        let session_id = format!("ses-{}-{}", backend, timefmt::now().format("%Y%m%d-%H%M%S"));
        if self.take_task(task_id, &session_id, true)?.is_none() {
            return Err(KanbanError::Invalid(format!(
                "Agent launch failed for {task_id}"
            )));
        }
        Ok(Some(session_id))
    }

    /// Stop a running agent session: mark it closed first so a racing
    /// `agent-exit` cannot auto-resume, then kill its tmux host when present.
    /// Background (non-tmux) agent processes cannot be signalled — their
    /// session record is still closed so the board stops treating the task as
    /// running. The task keeps its current status and its session id (now the
    /// last session that worked it); recover or rerun decide what happens next.
    pub fn stop_session(&self, session_id: &str) -> Result<Option<Task>> {
        let session_mgr = self.session_manager();
        let Some(session) = session_mgr.load_session(session_id) else {
            return Ok(None);
        };
        let was_active = session.status == SessionStatus::Active;
        if was_active {
            session_mgr.close_session(session_id)?;
        }
        let _ = crate::agent::kill_session(session_id);
        let Some(task) = ({
            let _guard = self.storage.lock()?;
            self.storage.load_task(&session.task_id)?
        }) else {
            return Ok(None);
        };
        if was_active {
            self.thread_manager()?.post(
                &task.id,
                MessageRole::System,
                MessageKind::System,
                &format!("Session {session_id} was stopped by the user."),
                None,
                vec![],
                Some("kanban".to_string()),
            )?;
        }
        Ok(Some(task))
    }

    /// Stop the active agent session attached to a task. Missing tasks return
    /// `Ok(None)`; a task with no active session is an error.
    pub fn stop_task(&self, task_id: &str) -> Result<Option<Task>> {
        let task = {
            let _guard = self.storage.lock()?;
            self.storage.load_task(task_id)?
        };
        let Some(task) = task else {
            return Ok(None);
        };
        let Some(session_id) = task.session.clone() else {
            return Err(KanbanError::Invalid(format!(
                "Task {task_id} has no active session"
            )));
        };
        if !self.session_manager().is_session_active(&session_id) {
            return Err(KanbanError::Invalid(format!(
                "Task {task_id} has no active session"
            )));
        }
        self.stop_session(&session_id)
    }

    pub fn complete_task(
        &self,
        task_id: &str,
        session_id: &str,
        is_agent: bool,
    ) -> Result<Option<Task>> {
        let task = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };

            if is_agent && self.config.get_rule("user_only_review_to_done")? {
                if task.status == TaskStatus::Review {
                    return Ok(None);
                }
                self.require_current_agent_session(&task, session_id)?;
                let context = self.context_manager().get_context(task_id, &self.storage)?;
                if context.trim().is_empty() {
                    return Err(KanbanError::Permission(
                        "Agent cannot complete task without recording context".to_string(),
                    ));
                }

                if let Some(command) = self.config.get_verification_command()? {
                    let (exit_code, output_tail) =
                        self.run_verification_command(&command, task_id, session_id)?;
                    if exit_code != 0 && self.config.get_verification_block_on_failure()? {
                        task.updated_at = timefmt::now();
                        self.storage.save_task(&task)?;
                        let body =
                            format!("✗ gate failed code={exit_code} cmd={command}\n{output_tail}");
                        let _ = self.thread_manager().and_then(|tm| {
                            tm.post_with_origin(
                                task_id,
                                MessageRole::System,
                                MessageKind::AgentStep,
                                &body,
                                None,
                                vec![],
                                Some("kanban".to_string()),
                                Some("kanban".to_string()),
                            )
                        });
                        drop(_guard);
                        self.session_manager().close_session(session_id)?;
                        if let Ok(notifier) = self.notifier() {
                            notifier.stranded(
                                &task.id,
                                &task.title,
                                &format!(
                                    "Verification gate failed (code {exit_code}); \
                                     task stays In Progress."
                                ),
                            );
                        }
                        return Ok(Some(task));
                    }
                    let body = format!("✓ gate passed cmd={command}");
                    let _ = self.thread_manager().and_then(|tm| {
                        tm.post_with_origin(
                            task_id,
                            MessageRole::System,
                            MessageKind::AgentStep,
                            &body,
                            None,
                            vec![],
                            Some("kanban".to_string()),
                            Some("kanban".to_string()),
                        )
                    });
                }

                task.status = TaskStatus::Review;
                task.review_unseen = true;
            } else {
                if is_agent {
                    self.require_current_agent_session(&task, session_id)?;
                }
                let Some(done) = self.move_task_to_done(&task)? else {
                    return Ok(None);
                };
                drop(_guard);
                self.session_manager().close_session(session_id)?;
                self.notify_completion(&done);
                return Ok(Some(done));
            }

            let now = timefmt::now();
            task.updated_at = now;
            task.completed_at = Some(now);
            self.storage.save_task(&task)?;
            task
        };

        self.session_manager().close_session(session_id)?;

        // The task just entered Review: launch any task chained to its completion.
        self.trigger_chained_tasks(&task.id)?;
        self.notify_completion(&task);

        Ok(Some(task))
    }

    /// Maximum number of characters from the verification command's combined
    /// stdout/stderr that is stored in the gate-failed `AgentStep` message.
    const VERIFICATION_OUTPUT_TAIL: usize = 2000;

    /// Run the configured verification command in the work folder (it builds
    /// and tests the user's code, not the board) and return its exit code plus
    /// a tail of the combined output.
    fn run_verification_command(
        &self,
        command: &str,
        task_id: &str,
        _session_id: &str,
    ) -> Result<(i32, String)> {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(self.work_path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|err| {
                KanbanError::Invalid(format!(
                    "Failed to run verification command for {task_id}: {err}"
                ))
            })?;

        let exit_code = output.status.code().unwrap_or(-1);
        let combined = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        let tail = combined
            .chars()
            .rev()
            .take(Self::VERIFICATION_OUTPUT_TAIL)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        let tail = tail.trim().to_string();
        Ok((exit_code, tail))
    }

    // ------------------------------------------------------------- chaining

    /// Tasks waiting on `target_task_id` to reach Review before they auto-run.
    pub fn chained_tasks(&self, target_task_id: &str) -> Result<Vec<Task>> {
        Ok(self
            .storage
            .get_all_tasks()?
            .into_iter()
            .filter(|t| t.chained_to.as_deref() == Some(target_task_id))
            .collect())
    }

    /// Auto-launch every To Do task chained to `target_task_id` now that it has
    /// entered Review. Gated by the `auto_launch_chained` rule and the master
    /// `auto_launch.enabled` switch. Only To Do tasks are launched so a chained
    /// task already in progress, review, or done is never re-triggered.
    fn trigger_chained_tasks(&self, target_task_id: &str) -> Result<Vec<Task>> {
        if !self.config.get_rule("auto_launch_chained")? || !self.auto_launch_enabled()? {
            return Ok(Vec::new());
        }

        let session_mgr = self.session_manager();
        let mut launched = Vec::new();
        for task in self.chained_tasks(target_task_id)? {
            if task.id == target_task_id || task.status != TaskStatus::Todo {
                continue;
            }
            if task
                .session
                .as_deref()
                .is_some_and(|s| session_mgr.is_session_active(s))
            {
                continue;
            }
            let backend = self.resolve_backend(&task)?;
            let session_id = format!(
                "ses-{}-{}",
                backend,
                timefmt::now().format("%Y%m%d-%H%M%S-%6f")
            );
            if let Some(result) = self.take_task(&task.id, &session_id, true)? {
                self.notify_chained_start(&result, target_task_id);
                launched.push(result);
            }
        }
        Ok(launched)
    }

    // ---------------------------------------------------- questions / thread

    pub fn ask_question(
        &self,
        task_id: &str,
        question: &str,
        source: &str,
        variants: Vec<String>,
    ) -> Result<Option<Task>> {
        self.ask_question_for_session(task_id, question, source, None, variants)
    }

    pub fn ask_question_for_session(
        &self,
        task_id: &str,
        question: &str,
        source: &str,
        session_id: Option<&str>,
        variants: Vec<String>,
    ) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        let Some((mut task, tm)) = self.load_task_and_prepare_thread(task_id)? else {
            return Ok(None);
        };

        if let Some(session_id) = session_id {
            self.require_current_agent_session(&task, session_id)?;
        }

        tm.post(
            &task.id,
            role_for_source(source),
            MessageKind::Question,
            question,
            None,
            variants,
            Some(source.to_string()),
        )?;

        task.has_questions = tm.has_open_questions(&task.id)?;
        task.updated_at = timefmt::now();
        if self.config.get_rule("questions_go_to_review")? {
            task.status = TaskStatus::Review;
            task.review_unseen = role_for_source(source) == MessageRole::Agent;
        }
        self.storage.save_task(&task)?;
        self.notify_question(&task, question);
        Ok(Some(task))
    }

    /// Post every question in a validated [`AskForm`] as its own `question`
    /// message, mapping each entry's `options` onto the message `variants`.
    /// Locks the board once, saves the task once, and notifies once (with the
    /// question count) so a large form does not fan out into N notifications.
    /// Returns the updated task plus the number of questions posted.
    pub fn ask_form(
        &self,
        task_id: &str,
        form: &AskForm,
        source: &str,
        session_id: Option<&str>,
    ) -> Result<Option<(Task, usize)>> {
        let _guard = self.storage.lock()?;
        let Some((mut task, tm)) = self.load_task_and_prepare_thread(task_id)? else {
            return Ok(None);
        };

        if let Some(session_id) = session_id {
            self.require_current_agent_session(&task, session_id)?;
        }

        let role = role_for_source(source);
        for question in &form.questions {
            tm.post(
                &task.id,
                role,
                MessageKind::Question,
                &question.body(),
                None,
                question.options.clone(),
                Some(source.to_string()),
            )?;
        }

        let count = form.questions.len();
        task.has_questions = tm.has_open_questions(&task.id)?;
        task.updated_at = timefmt::now();
        if self.config.get_rule("questions_go_to_review")? {
            task.status = TaskStatus::Review;
            task.review_unseen = role == MessageRole::Agent;
        }
        self.storage.save_task(&task)?;
        self.notify_question(&task, &format!("{count} question(s) via form"));
        Ok(Some((task, count)))
    }

    pub fn answer_question(
        &self,
        task_id: &str,
        question_ref: QuestionRef,
        answer: &str,
    ) -> Result<Option<Task>> {
        let (mut task, tm, expected_session) = {
            let _guard = self.storage.lock()?;
            let Some((mut task, tm)) = self.load_task_and_prepare_thread(task_id)? else {
                return Ok(None);
            };

            let msg_id = match question_ref {
                QuestionRef::MsgId(id) => id,
                QuestionRef::Index(index) => {
                    let open = tm.open_messages(&task.id, Some(MessageKind::Question))?;
                    match open.get(index) {
                        Some(message) => message.id.clone(),
                        None => return Ok(None),
                    }
                }
            };

            if tm.get_message(&task.id, &msg_id)?.is_none() {
                return Ok(None);
            }

            tm.answer(&task.id, &msg_id, answer, MessageRole::Human)?;
            task.has_questions = tm.has_open_questions(&task.id)?;
            task.review_unseen = false;
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            let expected_session = task.session.clone();
            (task, tm, expected_session)
        };

        // Async interactive mode: once every open question is answered,
        // relaunch the agent with the full dialog so it can continue.
        if task.interactive
            && task.status == TaskStatus::InProgress
            && !tm.has_open_questions(&task.id)?
            && let Some(resumed) =
                self.resume_interactive_agent(&task.id, expected_session.as_deref())?
        {
            task = resumed;
        }
        Ok(Some(task))
    }

    fn resume_interactive_agent(
        &self,
        task_id: &str,
        expected_session: Option<&str>,
    ) -> Result<Option<Task>> {
        if !self.auto_launch_enabled()? {
            return Ok(None);
        }

        let session_mgr = self.session_manager();
        {
            let _guard = self.storage.lock()?;
            let Some(current) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            let tm = self.thread_manager()?;
            if !current.interactive
                || current.status != TaskStatus::InProgress
                || tm.has_open_questions(task_id)?
                || current.session.as_deref() != expected_session
            {
                return Ok(None);
            }
            if expected_session.is_some_and(|session| {
                session_mgr.load_session(session).is_some_and(|record| {
                    record.task_id == current.id
                        && record.status == SessionStatus::Active
                        && record.wait_until.is_none()
                })
            }) {
                // `ask --wait` is still polling the answered message and wakes
                // itself. Only a declared wait needs a replacement process.
                return Ok(None);
            }
        }

        self.revoke_in_progress_task(task_id, expected_session)
    }

    /// Replace the exact session currently assigned to an In Progress task.
    /// The expected-session comparison fences concurrent answer, timer, exit,
    /// and manual revoke paths so only one of them can install a successor.
    pub fn revoke_in_progress_task(
        &self,
        task_id: &str,
        expected_session: Option<&str>,
    ) -> Result<Option<Task>> {
        if let Some(session_id) = expected_session {
            SessionManager::validate_session_id(session_id)?;
        }

        let session_mgr = self.session_manager();
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(None);
        };
        if task.status != TaskStatus::InProgress || task.session.as_deref() != expected_session {
            return Ok(None);
        }

        if let Some(old_session) = expected_session {
            let mut record = session_mgr.load_session(old_session);
            if record
                .as_ref()
                .is_some_and(|session| session.task_id != task.id)
            {
                return Ok(None);
            }
            let process_already_gone = record.as_ref().is_none_or(|session| {
                session.status != SessionStatus::Active || session.wait_exited
            });
            match crate::agent::kill_session(old_session) {
                Ok(true) => {}
                Ok(false) if process_already_gone => {}
                Err(_) if process_already_gone => {}
                Ok(false) | Err(_)
                    if record.as_ref().is_some_and(|session| {
                        session.status == SessionStatus::Active && session.wait_until.is_some()
                    }) =>
                {
                    // A background fallback cannot be signalled safely. Make
                    // its original timer irrelevant: as soon as the wrapper
                    // exits, `agent-exit` observes an expired wait and performs
                    // the normal expected-session respawn.
                    let Some(session) = record.as_mut() else {
                        return Err(KanbanError::Invalid(format!(
                            "Session {old_session} disappeared during revoke"
                        )));
                    };
                    session.wait_until = Some(timefmt::now() - chrono::Duration::seconds(1));
                    session_mgr.save_session(session)?;
                    self.thread_manager()?.post_with_origin(
                        &task.id,
                        MessageRole::System,
                        MessageKind::Context,
                        "Wake requested while the background agent was still exiting; resume immediately after its process ends.",
                        None,
                        vec![],
                        Some("kanban".to_string()),
                        Some("kanban".to_string()),
                    )?;
                    return Ok(Some(task));
                }
                Ok(false) => {
                    return Err(KanbanError::Invalid(format!(
                        "Cannot revoke active session {old_session}: its process is not hosted in tmux and has not exited"
                    )));
                }
                Err(err) => return Err(err),
            }
        }

        let backend = safe_session_component(&self.resolve_backend(&task)?);
        let new_session_id = self.fresh_session_id(&backend);
        session_mgr.link_named_session(&task.id, &new_session_id, &task.title)?;
        if let Err(err) = self.thread_manager()?.post_with_origin(
            &task.id,
            MessageRole::System,
            MessageKind::Context,
            &format!(
                "Session {} was revoked. Wake immediately on a fresh session and continue from the current thread context.",
                expected_session.unwrap_or("none")
            ),
            None,
            vec![],
            Some("kanban".to_string()),
            Some("kanban".to_string()),
        ) {
            session_mgr.unlink_session(&new_session_id);
            return Err(err);
        }
        task.session = Some(new_session_id.clone());
        task.auto_resumes = 0;
        task.updated_at = timefmt::now();
        if let Err(err) = self.storage.save_task(&task) {
            session_mgr.unlink_session(&new_session_id);
            return Err(err);
        }
        if let Some(old_session) = expected_session {
            // Ownership has already moved and the old process is gone. A
            // failed archival write must not strand the published successor.
            let _ = session_mgr.close_session(old_session);
        }

        if self.auto_launch_enabled()?
            && !self.finish_launch(
                &new_session_id,
                self.launch_agent(&task.id, &new_session_id, false),
            )?
        {
            return Ok(None);
        }
        Ok(Some(task))
    }

    /// Allocate under the board lock; suffixing makes wall-clock repetition
    /// harmless and prevents an authority token from being overwritten.
    fn fresh_session_id(&self, backend: &str) -> String {
        let base = format!(
            "ses-{}-{}",
            backend,
            timefmt::now().format("%Y%m%d-%H%M%S-%6f")
        );
        let mut candidate = base.clone();
        let mut suffix = 2_u32;
        while self
            .session_manager()
            .sessions_dir
            .join(format!("{candidate}.yaml"))
            .exists()
        {
            candidate = format!("{base}-{suffix}");
            suffix += 1;
        }
        candidate
    }

    fn require_current_agent_session(&self, task: &Task, session_id: &str) -> Result<()> {
        let valid = task.status == TaskStatus::InProgress
            && task.session.as_deref() == Some(session_id)
            && self
                .session_manager()
                .load_session(session_id)
                .is_some_and(|session| {
                    session.task_id == task.id && session.status == SessionStatus::Active
                });
        if valid {
            Ok(())
        } else {
            Err(KanbanError::Permission(format!(
                "Session {session_id} is no longer the active session of task {}",
                task.id
            )))
        }
    }

    pub fn suggest_improvement(
        &self,
        task_id: &str,
        suggestion: &str,
        source: &str,
        variants: Vec<String>,
    ) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        let Some((mut task, tm)) = self.load_task_and_prepare_thread(task_id)? else {
            return Ok(None);
        };

        tm.post(
            &task.id,
            role_for_source(source),
            MessageKind::Suggestion,
            suggestion,
            None,
            variants,
            Some(source.to_string()),
        )?;
        task.has_questions = tm.has_open_questions(&task.id)?;
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        Ok(Some(task))
    }

    /// Quarantine a thread message: `status = rejected` so it is excluded
    /// from every future agent prompt and gathered context, while staying
    /// visible in the thread for audit. `Ok(None)` when the task or message
    /// doesn't exist.
    pub fn reject_message(&self, task_id: &str, msg_id: &str) -> Result<Option<Message>> {
        self.set_message_rejected(task_id, msg_id, true)
    }

    /// Undo [`Operations::reject_message`], restoring the message to `open`
    /// so it is fed back into the supply chain.
    pub fn unreject_message(&self, task_id: &str, msg_id: &str) -> Result<Option<Message>> {
        self.set_message_rejected(task_id, msg_id, false)
    }

    fn set_message_rejected(
        &self,
        task_id: &str,
        msg_id: &str,
        rejected: bool,
    ) -> Result<Option<Message>> {
        let _guard = self.storage.lock()?;
        let Some((_task, tm)) = self.load_task_and_prepare_thread(task_id)? else {
            return Ok(None);
        };
        if tm.get_message(task_id, msg_id)?.is_none() {
            return Ok(None);
        }
        let status = if rejected {
            MessageStatus::Rejected
        } else {
            MessageStatus::Open
        };
        Ok(Some(tm.resolve(task_id, msg_id, status)?))
    }

    pub fn list_open_messages(&self, task_id: &str) -> Result<Vec<Message>> {
        let _guard = self.storage.lock()?;
        let Some((task, tm)) = self.load_task_and_prepare_thread(task_id)? else {
            return Ok(Vec::new());
        };
        tm.open_messages(&task.id, None)
    }

    /// Earliest open question on a task, for board-card previews. Read-only:
    /// no lock and no legacy-thread migration, so it is cheap enough for
    /// snapshot building.
    pub fn first_open_question(&self, task_id: &str) -> Result<Option<Message>> {
        Ok(self
            .thread_manager()?
            .open_messages(task_id, Some(MessageKind::Question))?
            .into_iter()
            .next())
    }

    /// Post the question, then block until it is answered or the timeout
    /// expires (a timeout records a system answer so the agent never hangs).
    pub fn ask_and_wait(
        &self,
        task_id: &str,
        question: &str,
        session_id: Option<&str>,
        variants: Vec<String>,
        timeout: Option<i64>,
        poll_interval: Option<i64>,
    ) -> Result<Option<Message>> {
        let timeout = match timeout {
            Some(t) => t,
            None => self.config.get_threshold("question_wait_timeout")?,
        };
        let poll_interval = match poll_interval {
            Some(p) => p,
            None => self.config.get_threshold("question_poll_interval")?,
        };

        let (task, tm, message) = {
            let _guard = self.storage.lock()?;
            let Some((mut task, tm)) = self.load_task_and_prepare_thread(task_id)? else {
                return Ok(None);
            };
            if let Some(session_id) = session_id {
                self.require_current_agent_session(&task, session_id)?;
            }
            let message = tm.post(
                &task.id,
                MessageRole::Agent,
                MessageKind::Question,
                question,
                None,
                variants,
                Some("agent".to_string()),
            )?;
            task.has_questions = tm.has_open_questions(&task.id)?;
            task.updated_at = timefmt::now();
            if self.config.get_rule("questions_go_to_review")? {
                task.status = TaskStatus::Review;
                task.review_unseen = true;
            }
            self.storage.save_task(&task)?;
            (task, tm, message)
        };
        self.notify_question(&task, question);

        let session_mgr = self.session_manager();
        let started = Instant::now();
        loop {
            if let Some(session_id) = session_id {
                let _guard = self.storage.lock()?;
                let Some(task) = self.storage.load_task(task_id)? else {
                    return Ok(None);
                };
                self.require_current_agent_session(&task, session_id)?;
                session_mgr.heartbeat(session_id)?;
            }

            if let Some(current) = tm.get_message(&task.id, &message.id)?
                && current.status == MessageStatus::Answered
            {
                return Ok(Some(current));
            }

            if started.elapsed().as_secs() as i64 >= timeout {
                if let Some(current) = tm.get_message(&task.id, &message.id)?
                    && current.status == MessageStatus::Answered
                {
                    return Ok(Some(current));
                }
                let answered = tm.answer(
                    &task.id,
                    &message.id,
                    "(timeout - no answer received)",
                    MessageRole::System,
                )?;
                let _guard = self.storage.lock()?;
                if let Some(mut task) = self.storage.load_task(&task.id)? {
                    task.has_questions = tm.has_open_questions(&task.id)?;
                    task.updated_at = timefmt::now();
                    self.storage.save_task(&task)?;
                }
                return Ok(Some(answered));
            }

            std::thread::sleep(Duration::from_secs(poll_interval.max(0) as u64));
        }
    }

    // ------------------------------------------------------- review / rerun

    /// Clear the `review_unseen` notifier flag. Called when the user opens a
    /// task's detail view: the act of opening it is the "I've seen it" signal.
    /// Returns true when the flag was set and is now cleared (a write happened).
    pub fn mark_review_seen(&self, task_id: &str) -> Result<bool> {
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(false);
        };
        if !task.review_unseen {
            return Ok(false);
        }
        task.review_unseen = false;
        self.storage.save_task(&task)?;
        Ok(true)
    }

    /// Stash the human's review-edit notes in the per-task buffer. Folded into
    /// the thread and cleared on the next re-run — see [`Self::rerun_review_task`].
    pub fn set_review_edits(&self, task_id: &str, text: &str) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(None);
        };
        task.review_edits = text.to_string();
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        Ok(Some(task))
    }

    /// Fold the pending review-edits buffer into the thread as a permanent
    /// `review_edit` message, clear it, and relaunch the agent.
    pub fn rerun_review_task(
        &self,
        task_id: &str,
        session_id: Option<&str>,
    ) -> Result<Option<Task>> {
        if let Some(session_id) = session_id {
            SessionManager::validate_session_id(session_id)?;
        }
        let (task, session_id) = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            let tm = self.thread_manager()?;
            let edits = task.review_edits.trim().to_string();
            if !edits.is_empty() {
                tm.post_with_origin(
                    &task.id,
                    MessageRole::Human,
                    MessageKind::ReviewEdit,
                    &edits,
                    None,
                    vec![],
                    Some("user".to_string()),
                    Some("human".to_string()),
                )?;
            }

            let backend = self.resolve_backend(&task)?;
            let session_id = match session_id {
                Some(s) => s.to_string(),
                None => format!("ses-{}-{}", backend, timefmt::now().format("%Y%m%d-%H%M%S")),
            };
            task.review_edits = String::new();
            task.session = Some(session_id.clone());
            task.auto_resumes = 0;
            task.review_unseen = false;
            task.status = TaskStatus::InProgress;
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            self.session_manager()
                .link_named_session(&task.id, &session_id, &task.title)?;
            (task, session_id)
        };

        if self.auto_launch_enabled()? {
            match self.finish_launch(&session_id, self.launch_agent(&task.id, &session_id, false)) {
                Ok(true) => {}
                failed => {
                    let _guard = self.storage.lock()?;
                    if let Some(mut current) = self.storage.load_task(&task.id)? {
                        current.status = TaskStatus::Review;
                        current.updated_at = timefmt::now();
                        self.storage.save_task(&current)?;
                    }
                    failed?;
                    return Ok(None);
                }
            }
        }
        Ok(Some(task))
    }

    /// Move a stalled or questioned in-progress task to a fresh agent session.
    pub fn rerun_in_progress_task(
        &self,
        task_id: &str,
        session_id: Option<&str>,
    ) -> Result<Option<Task>> {
        if let Some(session_id) = session_id {
            SessionManager::validate_session_id(session_id)?;
        }
        let session_mgr = self.session_manager();
        let (task, session_id) = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            if task.status != TaskStatus::InProgress {
                return Ok(None);
            }

            let tm = self.thread_manager()?;
            let old_session_id = task.session.clone();
            let has_open_questions = task.has_questions || tm.has_open_questions(&task.id)?;
            let session_running = old_session_id
                .as_deref()
                .is_some_and(|s| session_mgr.is_session_active(s));
            if session_running && !has_open_questions {
                return Ok(None);
            }
            if old_session_id.is_none() && !has_open_questions {
                return Ok(None);
            }

            let backend = self.resolve_backend(&task)?;
            let session_id = match session_id {
                Some(s) => s.to_string(),
                None => format!("ses-{}-{}", backend, timefmt::now().format("%Y%m%d-%H%M%S")),
            };
            if let Some(old) = old_session_id.as_deref() {
                session_mgr.crash_session(old)?;
            }
            let reason = if has_open_questions {
                "open questions"
            } else {
                "stalled session"
            };
            tm.post(
                &task.id,
                MessageRole::System,
                MessageKind::System,
                &format!(
                    "Task was re-run from In Progress.\nReason: {}\nPrevious session: {}\nNew session: {}",
                    reason,
                    old_session_id.as_deref().unwrap_or("none"),
                    session_id
                ),
                None,
                vec![],
                Some("kanban".to_string()),
            )?;

            task.session = Some(session_id.clone());
            task.auto_resumes = 0;
            task.has_questions = tm.has_open_questions(&task.id)?;
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            session_mgr.link_named_session(&task.id, &session_id, &task.title)?;
            (task, session_id)
        };

        if self.auto_launch_enabled()?
            && !self.finish_launch(&session_id, self.launch_agent(&task.id, &session_id, false))?
        {
            return Ok(None);
        }
        Ok(Some(task))
    }

    // --------------------------------------------------------------- launch

    fn auto_launch_enabled(&self) -> Result<bool> {
        Ok(self
            .config
            .load()?
            .auto_launch
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true))
    }

    /// Backend key for a task: its `agent_backend`, else the configured
    /// default, falling back to `opencode` for unknown backends.
    pub fn resolve_backend(&self, task: &Task) -> Result<String> {
        let config = self.config.load()?;
        let backend = task.agent_backend.clone().unwrap_or_else(|| {
            config
                .auto_launch
                .get("default_agent")
                .and_then(|v| v.as_str())
                .unwrap_or("opencode")
                .to_string()
        });
        if config.agents.contains_key(backend.as_str()) {
            Ok(backend)
        } else {
            Ok("opencode".to_string())
        }
    }

    /// Configured opencode agent personas selectable per task (opencode-only).
    pub fn opencode_agent_options(&self) -> Result<Vec<String>> {
        let config = self.config.load()?;
        Ok(config
            .agents
            .get("opencode")
            .and_then(|v| v.get("agent_options"))
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    fn launch_agent(&self, task_id: &str, session_id: &str, revert: bool) -> Result<bool> {
        let Some(task) = self.storage.load_task(task_id)? else {
            return Err(KanbanError::Invalid(format!(
                "Task {task_id} not found. Agent not started."
            )));
        };
        let task = self.record_launch_settings(task)?;
        self.log_launch_step(&task, session_id, revert);
        match self
            .launcher
            .launch(self.roots(), &task, session_id, revert)
        {
            Ok(started) => Ok(started),
            Err(err) => {
                self.log_launch_failure(&task, session_id, &err.to_string());
                Err(err)
            }
        }
    }

    fn log_launch_failure(&self, task: &Task, session_id: &str, detail: &str) {
        let body = format!("✖ launch session={session_id} failed: {detail}");
        let _ = self.thread_manager().and_then(|tm| {
            tm.post_with_origin(
                &task.id,
                MessageRole::System,
                MessageKind::AgentStep,
                &body,
                None,
                vec![],
                Some("kanban".to_string()),
                Some("kanban".to_string()),
            )
        });
    }

    /// Crash the session when the agent did not start. `Ok(false)` stays a
    /// soft failure; `Err` is the exact spawn error for the TUI status bar.
    fn finish_launch(&self, session_id: &str, launched: Result<bool>) -> Result<bool> {
        match launched {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.session_manager().crash_session(session_id)?;
                Ok(false)
            }
            Err(err) => {
                let _ = self.session_manager().crash_session(session_id);
                Err(err)
            }
        }
    }

    /// Dump the assembled prompt to `.kanban/logs/<session>.prompt.txt` and
    /// post an `AgentStep` audit entry recording this launch. Best-effort:
    /// a failure here must never block the actual launch.
    fn log_launch_step(&self, task: &Task, session_id: &str, revert: bool) {
        let prompt_path = self
            .storage
            .logs_dir
            .join(format!("{session_id}.prompt.txt"));
        if let Ok(prompt) = build_agent_prompt(self.roots(), task, session_id, revert) {
            let _ = atomic_write_text(&prompt_path, &prompt);
        }

        let rel_prompt_path = prompt_path
            .strip_prefix(self.data_root())
            .unwrap_or(&prompt_path)
            .display();
        let body = format!(
            "▶ launch session={session_id} backend={} model={} effort={} agent={} revert={revert} → prompt: {rel_prompt_path}",
            task.agent_backend.as_deref().unwrap_or("-"),
            task.ai_model.as_deref().unwrap_or("-"),
            task.ai_effort.as_deref().unwrap_or("-"),
            task.agent_name.as_deref().unwrap_or("-"),
        );
        let _ = self.thread_manager().and_then(|tm| {
            tm.post_with_origin(
                &task.id,
                MessageRole::System,
                MessageKind::AgentStep,
                &body,
                None,
                vec![],
                Some("kanban".to_string()),
                Some("kanban".to_string()),
            )
        });
    }

    /// Pin the backend, model, effort, and agent persona this launch resolved
    /// onto the task, so its fields describe the session that actually ran
    /// instead of showing nothing when the values came from board config.
    /// Fields the task already sets are left untouched.
    fn record_launch_settings(&self, task: Task) -> Result<Task> {
        let settings = resolve_launch_settings(&self.config.load()?, &task)?;
        let _guard = self.storage.lock()?;
        let Some(mut current) = self.storage.load_task(&task.id)? else {
            return Ok(task);
        };
        let recorded = Task {
            agent_backend: Some(settings.backend),
            ai_model: settings.model.or(current.ai_model.clone()),
            ai_effort: settings.effort.or(current.ai_effort.clone()),
            agent_name: settings.agent.or(current.agent_name.clone()),
            ..current.clone()
        };
        if recorded == current {
            return Ok(current);
        }
        current = recorded;
        self.storage.save_task(&current)?;
        Ok(current)
    }

    /// Spawn a revert job restoring every file under the task's backup dir.
    pub fn launch_revert(&self, task_id: &str, session_id: &str) -> Result<bool> {
        SessionManager::validate_session_id(session_id)?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(false);
        };
        if !self.task_has_backups(task_id) {
            return Ok(false);
        }

        task.session = Some(session_id.to_string());
        task.status = TaskStatus::InProgress;
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        self.session_manager()
            .link_named_session(task_id, session_id, &task.title)?;
        self.finish_launch(session_id, self.launch_agent(task_id, session_id, true))
    }

    // ----------------------------------------- waiting / exit reconciliation

    /// Record a wait the agent declared with `kanban waiting`: the session is
    /// kept alive until `eta × waiting_eta_multiplier` seconds from now, and
    /// if the agent process exits meanwhile it is relaunched at that deadline
    /// to check the awaited result. Returns the relaunch deadline.
    pub fn declare_waiting(
        &self,
        task_id: &str,
        session_id: &str,
        eta: Option<i64>,
        note: Option<&str>,
    ) -> Result<chrono::NaiveDateTime> {
        let min_eta = self.config.get_threshold("waiting_min_eta")?.max(1);
        let max_eta = self.config.get_threshold("waiting_max_eta")?.max(min_eta);
        let eta = match eta {
            Some(value) => value,
            None => self.config.get_threshold("waiting_default_eta")?,
        }
        .clamp(min_eta, max_eta);
        let multiplier = self.config.get_threshold("waiting_eta_multiplier")?.max(1);
        let note_max_chars = self.config.get_threshold("waiting_note_max_chars")?.max(1) as usize;
        let wait_seconds = eta.checked_mul(multiplier).ok_or_else(|| {
            KanbanError::Invalid("waiting ETA is too large after applying multiplier".to_string())
        })?;
        let duration = chrono::Duration::try_seconds(wait_seconds)
            .ok_or_else(|| KanbanError::Invalid("waiting duration is out of range".to_string()))?;
        let deadline = timefmt::now()
            .checked_add_signed(duration)
            .ok_or_else(|| KanbanError::Invalid("waiting deadline is out of range".to_string()))?;

        let session_mgr = self.session_manager();
        let task = {
            let _guard = self.storage.lock()?;
            let Some(task) = self.storage.load_task(task_id)? else {
                return Err(KanbanError::Invalid(format!("Task {task_id} not found")));
            };
            let valid_session = session_mgr.load_session(session_id).is_some_and(|s| {
                s.task_id == task_id
                    && s.status == SessionStatus::Active
                    && task.status == TaskStatus::InProgress
                    && task.session.as_deref() == Some(session_id)
            });
            if !valid_session {
                return Err(KanbanError::Invalid(format!(
                    "Session {session_id} is not the active In Progress session of task {task_id}"
                )));
            }
            let note_text = note
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or("a long-running result")
                .chars()
                .take(note_max_chars)
                .collect::<String>();
            if !session_mgr.set_wait(session_id, deadline, Some(note_text.clone()))? {
                return Err(KanbanError::Invalid(format!(
                    "Session {session_id} is not an active session of task {task_id}"
                )));
            }
            (task, note_text)
        };
        let (task, note_text) = task;

        // `context` kind (not `system`) so the note reaches the relaunch prompt.
        self.thread_manager()?.post_with_origin(
            task_id,
            MessageRole::Agent,
            MessageKind::Context,
            &format!(
                "⏳ Entering wait mode: {note_text}\nExpected ~{eta}s; relaunch deadline {} (×{multiplier} safety buffer).",
                timefmt::format(&deadline)
            ),
            None,
            vec![],
            Some("agent".to_string()),
            Some(format!("agent:{session_id}")),
        )?;
        if let Ok(notifier) = self.notifier() {
            notifier.waiting(
                task_id,
                &task.title,
                &note_text,
                &timefmt::format(&deadline),
            );
        }
        Ok(deadline)
    }

    /// Start `command` fully detached from the agent session, then declare a
    /// wait for its result (see [`Operations::declare_waiting`]).
    ///
    /// Agent sessions run inside a disposable tmux session whose whole
    /// process group is signalled when the agent's reply ends, so a plain
    /// shell background job never survives the session. This helper runs the
    /// command as its own session leader (`setsid`) with stdin from
    /// /dev/null, appends stdout/stderr to a log file under
    /// `.kanban/detached/`, and writes the exit code to a `.status` file next
    /// to it, so the relaunched agent can check the outcome. The wait note
    /// carries both paths into the relaunch prompt.
    pub fn detach_command(
        &self,
        task_id: &str,
        session_id: &str,
        eta: Option<i64>,
        note: Option<&str>,
        command: &[String],
    ) -> Result<DetachedJob> {
        if command.is_empty() {
            return Err(KanbanError::Invalid(
                "detach requires a command to run (after --)".to_string(),
            ));
        }
        // Cheap pre-check so an invalid session does not spawn a process;
        // declare_waiting re-validates under the board lock afterwards.
        let session_ok = self
            .session_manager()
            .load_session(session_id)
            .is_some_and(|session| {
                session.task_id == task_id && session.status == SessionStatus::Active
            });
        if !session_ok {
            return Err(KanbanError::Invalid(format!(
                "Session {session_id} is not the active In Progress session of task {task_id}"
            )));
        }

        let detached_dir = self.data_root().join(".kanban").join("detached");
        fs::create_dir_all(&detached_dir)?;
        let stamp = timefmt::now().format("%Y%m%d-%H%M%S%3f");
        let log_file = detached_dir.join(format!("{task_id}-{stamp}.log"));
        let status_file = detached_dir.join(format!("{task_id}-{stamp}.status"));

        let quote_err =
            |_| KanbanError::Invalid("detach command contains an unquotable argument".to_string());
        let command_line =
            shlex::try_join(command.iter().map(String::as_str)).map_err(quote_err)?;
        let log_quoted = shlex::try_quote(&log_file.display().to_string())
            .map_err(quote_err)?
            .into_owned();
        let status_quoted = shlex::try_quote(&status_file.display().to_string())
            .map_err(quote_err)?
            .into_owned();
        // The wrapper records the exit code atomically (tmp + rename) so a
        // present .status file always holds the final result.
        let script = format!(
            "{command_line} </dev/null >>{log_quoted} 2>&1; status=$?; \
             printf '%s\\n' \"$status\" >{status_quoted}.tmp && \
             mv {status_quoted}.tmp {status_quoted}; exit $status"
        );

        let mut child_cmd = std::process::Command::new("bash");
        child_cmd
            .args(["-c", &script])
            // The detached job is the agent's own command line: it runs where
            // the agent runs, not where the board lives.
            .current_dir(self.work_path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            // New session: no controlling terminal, own process group, so
            // the job outlives the tmux session hosting the agent.
            child_cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = child_cmd.spawn()?;
        let pid = child.id();

        let base_note = note
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("a detached command result");
        // The note is read back by the relaunched agent, whose cwd is the work
        // folder: paths outside it (a board in the store) stay absolute.
        let rel = |path: &Path| {
            path.strip_prefix(self.work_path())
                .unwrap_or(path)
                .display()
                .to_string()
        };
        let full_note = format!(
            "{base_note} [detached pid {pid}; output: {}; exit code: {}]",
            rel(&log_file),
            rel(&status_file)
        );
        let deadline = match self.declare_waiting(task_id, session_id, eta, Some(&full_note)) {
            Ok(deadline) => deadline,
            Err(err) => {
                // The wait was never recorded, so nothing will ever check on
                // this job; stop its whole process group rather than leak it.
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
                return Err(err);
            }
        };

        Ok(DetachedJob {
            pid,
            log_file,
            status_file,
            deadline,
        })
    }

    /// Reconcile an exited agent process (called by the launch wrapper).
    ///
    /// A clean exit that leaves the task In Progress with no `done`, no open
    /// question, and no declared wait is an unfinished run — typically an
    /// agent that ended its reply expecting a background notification that
    /// can never arrive. Such tasks are auto-resumed on a fresh session,
    /// bounded by the `max_auto_resumes` threshold.
    ///
    /// Wraps [`Self::reconcile_agent_exit_inner`] with a single `AgentStep`
    /// audit post, skipped only when `session_id` never belonged to
    /// `task_id` (a spurious/stale callback, nothing to log).
    pub fn reconcile_agent_exit(
        &self,
        task_id: &str,
        session_id: &str,
        exit_status: i32,
    ) -> Result<AgentExitOutcome> {
        let session_matched_task = self
            .session_manager()
            .load_session(session_id)
            .is_some_and(|session| session.task_id == task_id && session.id == session_id);
        let outcome = self.reconcile_agent_exit_inner(task_id, session_id, exit_status)?;
        if session_matched_task {
            let manifest = self.harvest_provenance(task_id, session_id);
            // The agent's closing answer goes in before the exit audit line so
            // the thread reads in the order it happened: what the agent said,
            // then the session closing.
            self.record_agent_reply(task_id, session_id);
            self.log_exit_step(
                task_id,
                session_id,
                exit_status,
                &outcome,
                manifest.as_ref(),
            );
        }
        Ok(outcome)
    }

    /// Harvest the backend's machine transcript into an input-provenance
    /// manifest (`.kanban/provenance/<session>.yaml`) recording what the run
    /// actually consumed — files read into context (including via Bash), files
    /// written, URLs, MCP calls. Best-effort and backend-gated: claude and
    /// opencode emit a parseable transcript, and any failure is a soft warning
    /// that never disturbs the reconciled exit.
    fn harvest_provenance(&self, task_id: &str, session_id: &str) -> Option<InputManifest> {
        let backend = self.task_backend(task_id)?;
        let transcript = self
            .storage
            .logs_dir
            .join(format!("{session_id}.transcript.jsonl"));
        if !transcript.exists() {
            return None;
        }
        let prompt_path = self
            .storage
            .logs_dir
            .join(format!("{session_id}.prompt.txt"));
        let prompt_dump = prompt_path.exists().then(|| {
            prompt_path
                .strip_prefix(self.data_root())
                .unwrap_or(&prompt_path)
                .display()
                .to_string()
        });
        let session = session_id.to_string();
        // The harvester's root relativizes the files a run read and wrote:
        // those are the user's code, so it is the work folder.
        let repo_root = self.work_path().to_path_buf();
        let harvester: Box<dyn TranscriptHarvester> = match backend.as_str() {
            "claude" => Box::new(ClaudeHarvester {
                session_id: session,
                prompt_dump,
                root: repo_root,
            }),
            "opencode" => Box::new(OpencodeHarvester {
                session_id: session,
                prompt_dump,
                root: repo_root,
            }),
            "pi" | "omp" => Box::new(PiFamilyHarvester {
                session_id: session,
                backend: backend.clone(),
                prompt_dump,
                root: repo_root,
            }),
            _ => return None,
        };
        match harvester.harvest(&transcript) {
            Ok(manifest) => {
                let _ = provenance::write_manifest(&self.storage.provenance_dir, &manifest);
                Some(manifest)
            }
            Err(_) => None,
        }
    }

    /// Backend a task's launches resolve to (its own field, or the board
    /// default), which decides how its session transcript is parsed at exit.
    fn task_backend(&self, task_id: &str) -> Option<String> {
        let task = self.storage.load_task(task_id).ok().flatten()?;
        Some(
            resolve_launch_settings(&self.config.load().ok()?, &task)
                .ok()?
                .backend,
        )
    }

    /// Post the agent's closing answer — the summary it prints as its last
    /// words — to the task thread as a `context` message authored by
    /// `agent-reply`. Without this the thread holds only the audit trail
    /// (launch, agent-written context, exit) while the answer itself stays
    /// buried in `.kanban/logs/<session>.log`. It is taken from the backend's
    /// own machine transcript, so it is what the agent actually said rather
    /// than prose it chose to re-type through `kanban context`.
    ///
    /// Best-effort: any failure is a soft warning — the exit has already been
    /// reconciled by the time this runs. Setting the `agent_reply_max_chars`
    /// threshold to 0 turns the whole behavior off; otherwise it caps how much
    /// of a long reply is kept, since every thread entry is replayed into the
    /// next prompt.
    fn record_agent_reply(&self, task_id: &str, session_id: &str) {
        let Ok(max_chars) = self.config.get_threshold("agent_reply_max_chars") else {
            return;
        };
        if max_chars <= 0 {
            return;
        }
        let Some(backend) = self.task_backend(task_id) else {
            return;
        };
        let transcript = self
            .storage
            .logs_dir
            .join(format!("{session_id}.transcript.jsonl"));
        let Some(text) = reply::final_reply(&backend, &transcript) else {
            return;
        };
        let body = reply::truncate_reply(&text, max_chars as usize);
        // Agents commonly repeat their summary through `kanban context` before
        // finishing; posting identical text twice would only duplicate it in
        // the next prompt.
        let already_recorded = self
            .thread_manager()
            .and_then(|tm| tm.messages_of_kind(task_id, MessageKind::Context))
            .map(|messages| {
                messages
                    .iter()
                    .any(|message| message.body.trim() == body.trim())
            })
            .unwrap_or(false);
        if already_recorded {
            return;
        }
        let _ = self.context_manager().append_context_with_session(
            task_id,
            &body,
            "agent-reply",
            Some(session_id),
            &self.storage,
        );
    }

    fn reconcile_agent_exit_inner(
        &self,
        task_id: &str,
        session_id: &str,
        exit_status: i32,
    ) -> Result<AgentExitOutcome> {
        let session_mgr = self.session_manager();
        let Some(session) = session_mgr.load_session(session_id) else {
            return Ok(AgentExitOutcome::Closed);
        };
        if session.task_id != task_id || session.id != session_id {
            return Ok(AgentExitOutcome::Closed);
        }
        if session.status != SessionStatus::Active {
            return Ok(AgentExitOutcome::Closed);
        }
        if exit_status != 0 {
            session_mgr.crash_session(session_id)?;
            return Ok(AgentExitOutcome::Crashed);
        }

        let now = timefmt::now();
        let in_declared_wait = session.status == SessionStatus::Active
            && session.wait_until.is_some_and(|deadline| now <= deadline);
        if in_declared_wait {
            session_mgr.mark_wait_exited(session_id)?;
            return Ok(AgentExitOutcome::Waiting);
        }

        let stranded = {
            let _guard = self.storage.lock()?;
            self.storage.load_task(task_id)?.filter(|task| {
                task.status == TaskStatus::InProgress && task.session.as_deref() == Some(session_id)
            })
        };
        let Some(task) = stranded else {
            session_mgr.close_session(session_id)?;
            return Ok(AgentExitOutcome::Closed);
        };
        if self.thread_manager()?.has_open_questions(&task.id)? {
            // Waiting for a human answer, not stranded.
            session_mgr.close_session(session_id)?;
            return Ok(AgentExitOutcome::Closed);
        }
        if !self.auto_launch_enabled()? {
            session_mgr.close_session(session_id)?;
            return Ok(AgentExitOutcome::Closed);
        }

        let max_resumes = self.config.get_threshold("max_auto_resumes")?;
        if i64::from(task.auto_resumes) >= max_resumes {
            session_mgr.crash_session(session_id)?;
            if let Ok(notifier) = self.notifier() {
                notifier.stranded(
                    &task.id,
                    &task.title,
                    &format!(
                        "Agent exited without done/ask/waiting {max_resumes} times in a row; \
                         auto-resume budget is spent. Re-run or recover the task manually."
                    ),
                );
            }
            return Ok(AgentExitOutcome::ResumeExhausted);
        }

        session_mgr.close_session(session_id)?;
        let attempt = task.auto_resumes + 1;
        let note = format!(
            "Session {session_id} ended without completing the task, asking a question, or \
             declaring a wait; auto-resuming (attempt {attempt}/{max_resumes}).\n\
             If you were waiting on a background process or notification: background work does \
             not survive the end of a reply, so check its current state now and continue. Block \
             on long commands in the foreground, or declare long waits with the waiting command."
        );
        match self.respawn_session(task_id, session_id, &note, Some(attempt))? {
            RespawnOutcome::Spawned(new_session) => Ok(AgentExitOutcome::Resumed(new_session)),
            RespawnOutcome::Noop => Ok(AgentExitOutcome::Closed),
            RespawnOutcome::LaunchFailed(new_session) => {
                Ok(AgentExitOutcome::LaunchFailed(new_session))
            }
        }
    }

    /// Post an `AgentStep` audit entry summarizing an agent exit. Best-effort:
    /// a failure here must never propagate — the exit has already been
    /// reconciled by the time this runs.
    fn log_exit_step(
        &self,
        task_id: &str,
        session_id: &str,
        exit_status: i32,
        outcome: &AgentExitOutcome,
        manifest: Option<&InputManifest>,
    ) {
        let auto_resumes = self
            .storage
            .load_task(task_id)
            .ok()
            .flatten()
            .map(|task| task.auto_resumes.to_string())
            .unwrap_or_else(|| "-".to_string());
        let mut body = format!(
            "■ exit session={session_id} code={exit_status} outcome={} auto_resumes={auto_resumes}",
            outcome.label()
        );
        // Reference the input-provenance manifest (never inline it — it is
        // telemetry, kept out of the thread the next prompt is built from).
        if let Some(manifest) = manifest {
            body.push_str(&format!(
                " {} → provenance: .kanban/provenance/{session_id}.yaml",
                manifest.summary()
            ));
        }
        let _ = self.thread_manager().and_then(|tm| {
            tm.post_with_origin(
                task_id,
                MessageRole::System,
                MessageKind::AgentStep,
                &body,
                None,
                vec![],
                Some("kanban".to_string()),
                Some("kanban".to_string()),
            )
        });
    }

    /// Relaunch agents whose declared wait deadline has passed while their
    /// process is gone — the "ping": the fresh session is told to check the
    /// awaited result, report, and either finish or declare a new wait.
    /// Called from the TUI tick and `kanban check-sessions`.
    pub fn resume_expired_waits(&self) -> Result<Vec<(String, String)>> {
        let session_mgr = self.session_manager();
        let heartbeat_timeout = self.config.get_threshold("session_heartbeat_timeout")?;
        let now = timefmt::now();
        let mut resumed = Vec::new();
        for session in session_mgr.list_active_sessions() {
            let Some(wait_until) = session.wait_until else {
                continue;
            };
            if now <= wait_until {
                continue;
            }
            // A live process past its deadline is still working (its wrapper
            // keeps heartbeating); only relaunch when the process is gone.
            let process_gone =
                session.wait_exited || (now - session.last_seen).num_seconds() > heartbeat_timeout;
            if !process_gone {
                continue;
            }
            let Some(task) = self.storage.load_task(&session.task_id)? else {
                session_mgr.close_session(&session.id)?;
                continue;
            };
            let max_resumes = self.config.get_threshold("max_auto_resumes")?;
            if i64::from(task.auto_resumes) >= max_resumes {
                session_mgr.crash_session(&session.id)?;
                if let Ok(notifier) = self.notifier() {
                    notifier.stranded(
                        &task.id,
                        &task.title,
                        &format!(
                            "Waiting deadline passed, but the auto-resume budget ({max_resumes}) is spent. Re-run or recover the task manually."
                        ),
                    );
                }
                continue;
            }
            let attempt = task.auto_resumes + 1;
            let note = format!(
                "⏰ Waiting deadline passed at {}.\nYou were waiting for: {}.\nCheck the awaited \
                 result now, record what you find with the context command, and continue. If it \
                 is still not ready, declare waiting again with a new --eta.",
                timefmt::format(&wait_until),
                session.wait_note.as_deref().unwrap_or("(no note)")
            );
            match self.respawn_session(&session.task_id, &session.id, &note, Some(attempt))? {
                RespawnOutcome::Spawned(new_session) => {
                    session_mgr.close_session(&session.id)?;
                    resumed.push((session.task_id.clone(), new_session));
                }
                RespawnOutcome::LaunchFailed(new_session) => {
                    session_mgr.close_session(&session.id)?;
                    if let Ok(notifier) = self.notifier() {
                        notifier.stranded(
                            &task.id,
                            &task.title,
                            &format!(
                                "Waiting deadline passed, but relaunch failed; {new_session} was marked crashed. Re-run or recover the task manually."
                            ),
                        );
                    }
                }
                RespawnOutcome::Noop => {
                    let still_stranded =
                        self.storage
                            .load_task(&session.task_id)?
                            .is_some_and(|task| {
                                task.status == TaskStatus::InProgress
                                    && task.session.as_deref() == Some(session.id.as_str())
                            });
                    if still_stranded {
                        session_mgr.crash_session(&session.id)?;
                        if let Ok(notifier) = self.notifier() {
                            notifier.stranded(
                                &task.id,
                                &task.title,
                                "Waiting deadline passed, but the task could not be relaunched automatically. Re-run or recover it manually.",
                            );
                        }
                    } else {
                        session_mgr.close_session(&session.id)?;
                    }
                }
            }
        }
        Ok(resumed)
    }

    /// Relaunch a task's agent on a fresh session after `expected_session`
    /// ended. No-op (returns `None`) when the task is gone, no longer In
    /// Progress, or already handed to a different session — the guard that
    /// makes concurrent resume paths (TUI tick vs `check-sessions`) safe.
    fn respawn_session(
        &self,
        task_id: &str,
        expected_session: &str,
        note: &str,
        resume_attempt: Option<u32>,
    ) -> Result<RespawnOutcome> {
        if !self.auto_launch_enabled()? {
            return Ok(RespawnOutcome::Noop);
        }
        let session_mgr = self.session_manager();
        let new_session_id = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(RespawnOutcome::Noop);
            };
            if task.status != TaskStatus::InProgress
                || task.session.as_deref() != Some(expected_session)
            {
                return Ok(RespawnOutcome::Noop);
            }
            let backend = self.resolve_backend(&task)?;
            let backend = safe_session_component(&backend);
            let new_session_id = self.fresh_session_id(&backend);
            // `context` kind so the relaunch prompt carries the reason.
            self.thread_manager()?.post_with_origin(
                &task.id,
                MessageRole::System,
                MessageKind::Context,
                note,
                None,
                vec![],
                Some("kanban".to_string()),
                Some("kanban".to_string()),
            )?;
            if let Some(attempt) = resume_attempt {
                task.auto_resumes = attempt;
            }
            task.session = Some(new_session_id.clone());
            task.updated_at = timefmt::now();
            session_mgr.link_named_session(&task.id, &new_session_id, &task.title)?;
            if let Err(err) = self.storage.save_task(&task) {
                session_mgr.unlink_session(&new_session_id);
                return Err(err);
            }
            new_session_id
        };

        if !self.finish_launch(
            &new_session_id,
            self.launch_agent(task_id, &new_session_id, false),
        )? {
            return Ok(RespawnOutcome::LaunchFailed(new_session_id));
        }
        Ok(RespawnOutcome::Spawned(new_session_id))
    }

    // ------------------------------------------------- per-task housekeeping

    pub fn backup_dir(&self, task_id: &str) -> PathBuf {
        self.data_root()
            .join(".kanban")
            .join("backups")
            .join(task_id)
    }

    pub fn task_has_backups(&self, task_id: &str) -> bool {
        let backup_dir = self.backup_dir(task_id);
        backup_dir.is_dir() && dir_has_files(&backup_dir)
    }

    /// The gathered work-context for a task, exactly as fed into agent prompts.
    /// Empty when the task carries no context.
    pub fn task_context(&self, task_id: &str) -> Result<String> {
        self.context_manager().get_context(task_id, &self.storage)
    }

    fn prompt_dump_path(&self, session_id: &str) -> PathBuf {
        self.storage
            .logs_dir
            .join(format!("{session_id}.prompt.txt"))
    }

    /// Whether an assembled-prompt dump exists for any of this task's sessions.
    /// A cheap existence check for the detail-view button (no file read).
    pub fn task_has_prompt(&self, task: &Task) -> bool {
        self.task_session_ids(task)
            .iter()
            .any(|session_id| self.prompt_dump_path(session_id).exists())
    }

    /// The most recent assembled-prompt dump recorded for this task across its
    /// sessions, or `None` if the task has never been launched.
    pub fn task_prompt(&self, task: &Task) -> Option<String> {
        let mut newest: Option<(std::time::SystemTime, String)> = None;
        for session_id in self.task_session_ids(task) {
            let path = self.prompt_dump_path(&session_id);
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if newest.as_ref().is_some_and(|(seen, _)| *seen >= modified) {
                continue;
            }
            if let Ok(text) = fs::read_to_string(&path) {
                newest = Some((modified, text));
            }
        }
        newest.map(|(_, text)| text)
    }

    fn clear_task_backups(&self, task_id: &str) {
        let backup_dir = self.backup_dir(task_id);
        if backup_dir.is_dir() {
            let _ = fs::remove_dir_all(&backup_dir);
        }
    }

    fn task_session_ids(&self, task: &Task) -> Vec<String> {
        let mut ids: Vec<String> = task.session.iter().cloned().collect();
        for session in self.session_manager().list_sessions() {
            if session.task_id == task.id && !ids.contains(&session.id) {
                ids.push(session.id);
            }
        }
        ids
    }

    fn clear_task_logs_and_sessions(&self, task: &Task) {
        let kanban_dir = self.data_root().join(".kanban");
        for session_id in self.task_session_ids(task) {
            if SessionManager::validate_session_id(&session_id).is_err() {
                continue;
            }
            self.session_manager().unlink_session(&session_id);
            let _ = fs::remove_file(kanban_dir.join("logs").join(format!("{session_id}.log")));
        }
        self.clear_task_detached(&task.id);
    }

    /// Remove `.kanban/detached/` log/status files recorded for this task's
    /// detached jobs (named `<task_id>-<stamp>.*`).
    fn clear_task_detached(&self, task_id: &str) {
        let detached_dir = self.data_root().join(".kanban").join("detached");
        let Ok(entries) = fs::read_dir(&detached_dir) else {
            return;
        };
        let prefix = format!("{task_id}-");
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(&prefix) && entry.path().is_file() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Delete pasted images referenced by the task description (only ever
    /// touches files inside `.kanban/assets/`).
    fn clear_task_assets(&self, task: &Task) {
        let assets_dir = self.data_root().join(".kanban").join("assets");
        let Ok(assets_dir) = assets_dir.canonicalize() else {
            return;
        };

        for raw_path in asset_paths_from_description(&task.description) {
            let Ok(asset_path) = self.data_root().join(&raw_path).canonicalize() else {
                continue;
            };
            if !asset_path.starts_with(&assets_dir) {
                continue;
            }
            if asset_path.is_file() {
                let _ = fs::remove_file(&asset_path);
                if let Some(parent) = asset_path.parent() {
                    remove_empty_asset_dirs(parent, &assets_dir);
                }
            }
        }
    }

    // ------------------------------------------------------ legacy migration

    fn load_task_and_prepare_thread(&self, task_id: &str) -> Result<Option<(Task, ThreadManager)>> {
        let tm = self.thread_manager()?;
        let Some(task) = self.storage.load_task(task_id)? else {
            return Ok(None);
        };
        let task = self.migrate_legacy_questions_if_needed(task, &tm)?;
        Ok(Some((task, tm)))
    }

    /// One-shot migration of a legacy `## Questions` description block into
    /// thread messages (with answers), stripping the block from the description.
    fn migrate_legacy_questions_if_needed(
        &self,
        mut task: Task,
        tm: &ThreadManager,
    ) -> Result<Task> {
        let Some(block) = legacy_questions_block(&task.description) else {
            return Ok(task);
        };

        let thread = tm.load(&task.id)?;
        let has_migrated = thread
            .messages
            .iter()
            .any(|m| matches!(m.kind, MessageKind::Question | MessageKind::Suggestion));
        if has_migrated {
            task.description = strip_legacy_questions_block(&task.description);
            task.has_questions = tm.has_open_questions(&task.id)?;
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            return Ok(task);
        }

        let question_re =
            Regex::new(r"^- \[[^\]]*\] \*\*([^*]+)\*\* \(([^)]+)\): (.*)$").expect("static regex");
        let answer_re = Regex::new(r"^\s+- Answer:\s*(.*)$").expect("static regex");

        let lines: Vec<&str> = block.body.lines().collect();
        let mut index = 0;
        while index < lines.len() {
            let line = lines[index].trim_end();
            let Some(caps) = question_re.captures(line) else {
                index += 1;
                continue;
            };
            let source = caps.get(2).map_or("", |m| m.as_str()).trim().to_string();
            let kind = if source.eq_ignore_ascii_case("suggestion") {
                MessageKind::Suggestion
            } else {
                MessageKind::Question
            };
            let message = tm.post(
                &task.id,
                role_for_source(&source),
                kind,
                caps.get(3).map_or("", |m| m.as_str()).trim(),
                None,
                vec![],
                Some(source),
            )?;

            if index + 1 < lines.len()
                && let Some(answer_caps) = answer_re.captures(lines[index + 1].trim_end())
            {
                let answer = answer_caps.get(1).map_or("", |m| m.as_str()).trim();
                if !answer.is_empty() && answer != "_(pending)_" {
                    tm.answer(&task.id, &message.id, answer, MessageRole::Human)?;
                }
                index += 1;
            }
            index += 1;
        }

        task.description = strip_legacy_questions_block(&task.description);
        let mut thread = tm.load(&task.id)?;
        if let Some(message) = thread
            .messages
            .iter_mut()
            .find(|m| m.kind == MessageKind::Task)
        {
            let body = task.description.trim();
            message.body = if body.is_empty() {
                "(no description provided)".to_string()
            } else {
                body.to_string()
            };
            message.updated_at = timefmt::now();
        }
        tm.save(&task.id, &mut thread)?;
        task.has_questions = tm.has_open_questions(&task.id)?;
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        Ok(task)
    }
}

fn safe_session_component(value: &str) -> String {
    let safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        "agent".to_string()
    } else {
        safe
    }
}

pub fn sort_tasks(tasks: &mut [Task], by: &str, order: &str) {
    if by == "completed" {
        let descending = order == "desc";
        tasks.sort_by(|a, b| {
            match (a.completed_at, b.completed_at) {
                (Some(a_completed), Some(b_completed)) => {
                    if descending {
                        b_completed.cmp(&a_completed)
                    } else {
                        a_completed.cmp(&b_completed)
                    }
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| compare_task_ids(&a.id, &b.id))
        });
        return;
    }
    if by == "updated" {
        let descending = order == "desc";
        tasks.sort_by(|a, b| {
            let by_updated = if descending {
                b.updated_at.cmp(&a.updated_at)
            } else {
                a.updated_at.cmp(&b.updated_at)
            };
            by_updated.then_with(|| compare_task_ids(&a.id, &b.id))
        });
        return;
    }
    match by {
        "id" => tasks.sort_by(|a, b| compare_task_ids(&a.id, &b.id)),
        "title" => tasks.sort_by_key(|t| t.title.to_lowercase()),
        _ => tasks.sort_by_key(|t| t.created_at),
    }
    if order == "desc" {
        tasks.reverse();
    }
}

fn compare_task_ids(a: &str, b: &str) -> std::cmp::Ordering {
    match (task_number(a), task_number(b)) {
        (Some(a_number), Some(b_number)) => a_number.cmp(&b_number).then_with(|| a.cmp(b)),
        _ => a.cmp(b),
    }
}

fn task_number(task_id: &str) -> Option<u64> {
    task_id.strip_prefix("TASK-")?.parse().ok()
}

struct LegacyBlock<'a> {
    start: usize,
    end: usize,
    body: &'a str,
}

/// Locate a `## Questions` section: heading at line start, body running until
/// the next `## <something>` heading or end of text.
fn legacy_questions_block(description: &str) -> Option<LegacyBlock<'_>> {
    const HEADING: &str = "## Questions";
    let bytes = description.as_bytes();
    let mut search_from = 0;

    while let Some(found) = description[search_from..].find(HEADING) {
        let idx = search_from + found;
        let at_line_start = idx == 0 || bytes[idx - 1] == b'\n';
        if !at_line_start {
            search_from = idx + 1;
            continue;
        }
        let head_end = idx + HEADING.len();
        let Some(nl_rel) = description[head_end..].find('\n') else {
            return None; // heading with no following newline — no block body
        };
        let nl = head_end + nl_rel;
        if !description[head_end..nl].trim().is_empty() {
            search_from = nl + 1;
            continue;
        }
        let body_start = nl + 1;

        // Body ends at the next "\n## <content>" heading, else end of text.
        let mut end = description.len();
        let mut pos = body_start;
        while let Some(rel) = description[pos..].find("\n## ") {
            let abs = pos + rel;
            let line_start = abs + 1;
            let line_end = description[line_start..]
                .find('\n')
                .map(|p| line_start + p)
                .unwrap_or(description.len());
            if !description[line_start + 3..line_end].trim().is_empty() {
                end = abs;
                break;
            }
            pos = abs + 1;
        }

        return Some(LegacyBlock {
            start: idx,
            end,
            body: &description[body_start..end],
        });
    }
    None
}

fn strip_legacy_questions_block(description: &str) -> String {
    let Some(block) = legacy_questions_block(description) else {
        return description.to_string();
    };
    let before = description[..block.start].trim_end();
    let after = description[block.end..].trim_start_matches('\n');
    if !before.is_empty() && !after.is_empty() {
        format!("{before}\n\n{after}")
    } else if !before.is_empty() {
        before.to_string()
    } else {
        after.to_string()
    }
}

/// `![alt](.kanban/assets/...)` image references in a description.
fn asset_paths_from_description(description: &str) -> Vec<PathBuf> {
    let re = Regex::new(r"!\[[^\]]*\]\(([^)]+)\)").expect("static regex");
    let mut paths = Vec::new();
    for caps in re.captures_iter(description) {
        let raw = caps.get(1).map_or("", |m| m.as_str());
        let raw = raw.split_whitespace().next().unwrap_or("");
        let raw = raw.trim_matches(|c| c == '\'' || c == '"');
        let path = PathBuf::from(raw);
        let mut components = path.components();
        let first_two: Vec<_> = components.by_ref().take(2).collect();
        if first_two.len() == 2
            && first_two[0].as_os_str() == ".kanban"
            && first_two[1].as_os_str() == "assets"
            && !paths.contains(&path)
        {
            paths.push(path);
        }
    }
    paths
}

fn remove_empty_asset_dirs(mut directory: &Path, assets_dir: &Path) {
    while directory != assets_dir && directory.starts_with(assets_dir) {
        if fs::remove_dir(directory).is_err() {
            return;
        }
        match directory.parent() {
            Some(parent) => directory = parent,
            None => return,
        }
    }
}

fn dir_has_files(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() {
            return true;
        }
        if path.is_dir() && dir_has_files(&path) {
            return true;
        }
    }
    false
}
