//! The single business-logic hub. Both the CLI and the TUI go through this
//! module; it owns CRUD, move/rule enforcement, questions, review edits,
//! chaining, and delegates agent process launching to an [`AgentLauncher`].

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDateTime};
use regex::Regex;
use serde_json::Value;

use crate::agent::{
    KanbanLauncher, build_agent_prompt, materialize_task_launch_settings,
    resolve_bot_launch_settings, resolve_launch_settings, resolve_task_launch_settings,
    upcoming_run_plan,
};
use crate::core::ask_form::AskForm;
use crate::core::config::{
    Config, IsolationCleanup, IsolationLand, IsolationMode, IsolationOnConflict, IsolationSeed,
    IsolationSettings, OnChangesRequested,
};
use crate::core::context::{ContextManager, role_for_source};
use crate::core::error::{KanbanError, Result};
use crate::core::limits;
use crate::core::models::{
    IntegrationState, Message, MessageKind, MessageRole, MessageStatus, Role, RunMode, RunPhase,
    Session, SessionStatus, Task, TaskStatus,
};
use crate::core::notifier::{DesktopNotifier, NotificationConfig};
use crate::core::project::{Project, Roots};
use crate::core::provenance::{
    self, ClaudeHarvester, CodexHarvester, InputManifest, OpencodeHarvester, PiFamilyHarvester,
    TranscriptHarvester,
};
use crate::core::reply;
use crate::core::scheduler::{Slots, role_for_phase};
use crate::core::session::{SessionManager, SessionState};
use crate::core::stats;
use crate::core::storage::{NewTask, Storage, atomic_write_text};
use crate::core::thread::ThreadManager;
use crate::core::timefmt;
use crate::core::vcs;

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
    pub use_designer: Option<bool>,
    pub use_reviewer: Option<bool>,
    pub chained_to: Option<Option<String>>,
    pub session: Option<Option<String>>,
}

#[derive(Debug, Clone)]
pub enum QuestionRef {
    Index(usize),
    MsgId(String),
}

/// Result of a recorded answer: the answered task plus how many questions are
/// still open and whether the last answer relaunched the agent.
#[derive(Debug, Clone)]
pub struct AnswerOutcome {
    pub task: Task,
    /// Open questions still awaiting a human answer after this one.
    pub remaining: usize,
    /// Session the agent was relaunched on, when the last answer resumed it.
    pub resumed_session: Option<String>,
    /// The paused agent was woken by the answer and parked in the dispatcher
    /// queue instead of launching (`resumed_session` is then `None`).
    pub queued: bool,
}

/// The reviewer bot's only legal exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Changes(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerdictRoute {
    HumanReview { exhausted: bool },
    Todo,
    Requeue,
}

fn apply_human_review(task: &mut Task) {
    task.status = TaskStatus::Review;
    task.run_phase = None;
    task.review_unseen = true;
    let now = timefmt::now();
    task.updated_at = now;
    task.completed_at = Some(now);
}

/// Result of landing an isolated task branch into the work folder (TASK-248).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandOutcome {
    /// The task never ran in an isolated worktree (or it was already cleaned
    /// up after an earlier landing).
    NotIsolated,
    /// Clean merge: the changed paths were written to the work folder as
    /// ordinary unstaged modifications and the integration ref advanced.
    Landed { changed: Vec<PathBuf> },
    /// Conflicting paths; nothing was written anywhere and the worktree is
    /// kept for resolution.
    Conflict { paths: Vec<String> },
    /// The landing did not run; the reason is on the thread.
    Deferred(String),
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

/// How a crashed run should be retried, read off the transcript's last error
/// event by [`Operations::crash_restart_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum CrashRestart {
    /// The normal `auto_restart.delays_minutes` ladder.
    Backoff,
    /// Stay crashed: the backend called the failure non-retryable.
    Skip,
    /// Wait for a named moment — the provider's usage window rolling over —
    /// instead of a blind backoff step.
    After(NaiveDateTime),
}

enum RespawnOutcome {
    Spawned(String),
    Noop,
    LaunchFailed(String),
}

/// What [`Operations::wake_expired_waits`] did about one expired declared
/// wait: a pause releases its slot, so ending one either re-enters the
/// dispatcher queue (the normal path) or relaunches directly when the queue
/// could never drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitWake {
    /// The pause ended and the task was parked in the dispatcher queue.
    Queued { task_id: String },
    /// The queue cannot dispatch (disabled or auto-launch off); the agent
    /// was relaunched directly on `session_id`.
    Resumed { task_id: String, session_id: String },
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

    pub(crate) fn session_manager(&self) -> SessionManager {
        SessionManager::new(self.data_root())
    }

    fn context_manager(&self) -> ContextManager {
        ContextManager::new(self.data_root())
    }

    pub(crate) fn notifier(&self) -> Result<DesktopNotifier> {
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
        self.storage
            .create_task(self.materialize_new_task(new_task)?)
    }

    /// Create directly in the requested board column, without exposing an
    /// intermediate To Do task to other board users.
    pub fn create_task_in_status(&self, new_task: NewTask, status: TaskStatus) -> Result<Task> {
        let task = self
            .storage
            .create_task_in_status(self.materialize_new_task(new_task)?, status)?;
        if status == TaskStatus::Review {
            self.trigger_chained_tasks(&task.id)?;
        }
        Ok(task)
    }

    /// Copy a task's user-facing state and thread into a fresh task identity.
    ///
    /// Run/session and worktree state belongs to the original task lifecycle,
    /// so a copy never inherits a live agent, restart bookkeeping, or isolation.
    pub fn copy_task(&self, task_id: &str) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        let Some(source) = self.storage.load_task(task_id)? else {
            return Ok(None);
        };
        let source_thread = self.thread_manager()?.load(task_id)?;
        let created = self
            .storage
            .create_task_in_status(NewTask::titled(source.title.clone()), source.status)?;
        let now = timefmt::now();
        let mut copied = source;
        copied.id = created.id;
        copied.session = None;
        copied.created_at = now;
        copied.updated_at = now;
        copied.completed_at = created.completed_at;
        copied.context_file = None;
        copied.context_size = 0;
        copied.auto_resumes = 0;
        copied.review_unseen = false;
        copied.run_phase = None;
        copied.crash_restarts = 0;
        copied.restart_at = None;
        copied.review_rounds = 0;
        copied.designed = false;
        copied.worktree = None;
        copied.branch = None;
        copied.base_commit = None;
        copied.integration = IntegrationState::None;
        self.storage.save_task(&copied)?;

        if !source_thread.messages.is_empty() {
            let threads = self.thread_manager()?;
            threads.discard_thread(&copied.id)?;
            let mut copied_thread = source_thread;
            copied_thread.task_id = copied.id.clone();
            copied_thread.rev = 0;
            copied_thread.base_rev = 0;
            copied_thread.base_messages.clear();
            threads.save(&copied.id, &mut copied_thread)?;
        }

        Ok(Some(copied))
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
        if let Some(use_designer) = patch.use_designer {
            task.use_designer = use_designer;
        }
        if let Some(use_reviewer) = patch.use_reviewer {
            task.use_reviewer = use_reviewer;
        }
        if let Some(chained_to) = patch.chained_to {
            task.chained_to = chained_to;
        }
        if let Some(session) = patch.session {
            task.session = session;
        }
        self.materialize_task_defaults(&mut task)?;
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        Ok(Some(task))
    }

    /// Snapshot board defaults onto blank launch fields so a task saved as
    /// "Default" stores the concrete backend/model/effort/agent.
    fn materialize_task_defaults(&self, task: &mut Task) -> Result<()> {
        materialize_task_launch_settings(&self.config.load()?, task)
    }

    fn materialize_new_task(&self, new_task: NewTask) -> Result<NewTask> {
        let mut task = Task::new(String::new(), String::new());
        task.ai_model = new_task.ai_model;
        task.ai_effort = new_task.ai_effort;
        task.agent_backend = new_task.agent_backend;
        task.agent_name = new_task.agent_name;
        self.materialize_task_defaults(&mut task)?;
        Ok(NewTask {
            title: new_task.title,
            description: new_task.description,
            ai_model: task.ai_model,
            ai_effort: task.ai_effort,
            agent_backend: task.agent_backend,
            agent_name: task.agent_name,
            interactive: new_task.interactive,
            use_designer: new_task.use_designer,
            use_reviewer: new_task.use_reviewer,
            chained_to: new_task.chained_to,
        })
    }

    pub fn delete_task(&self, task_id: &str) -> Result<bool> {
        self.storage.delete_task(task_id)
    }

    /// Delete a task together with its worktree, branch, thread, assets,
    /// context, backups, and sessions.
    pub fn abandon_task(&self, task_id: &str) -> Result<bool> {
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(false);
        };
        // Dropping a task drops its isolated worktree and branch with it.
        // The branch is typically unmerged here — an abandon is an explicit
        // human discard, so it goes too (a Conflict worktree never does).
        self.clear_task_worktree(&mut task, true);
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

    /// Abandon in-progress tasks whose session died without leaving
    /// questions, then run the worktree GC pass: the same sweep that
    /// reclaims stalled tasks reclaims isolation artifacts whose task is
    /// already gone.
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
        self.worktree_gc_pass()?;
        Ok(abandoned)
    }

    /// The worktree GC pass (TASK-250): reconcile worktree registrations for
    /// directories deleted by hand, then remove every
    /// `.kanban/worktrees/<id>` directory and every
    /// `<branch_prefix><id>` branch whose task no longer exists — the
    /// leftovers of crashes, kills, and manual deletions. A branch only ever
    /// existed for a task that once held a worktree, so with the task gone
    /// the branch is the tail of an interrupted cleanup and would block the
    /// recycled id's next worktree. Finally, when no task holds a worktree,
    /// re-baseline the integration ref to a fresh snapshot parented on HEAD:
    /// the ref is a GC root, and without this every snapshot it ever pointed
    /// at stays alive forever.
    fn worktree_gc_pass(&self) -> Result<()> {
        if self.project_id.is_none() {
            return Ok(());
        }
        let Some(repo) = vcs::detect(self.work_path()) else {
            return Ok(());
        };
        let _guard = self.storage.lock()?;
        repo.prune_worktrees()?;

        let tasks = self.storage.list_tasks(None)?;
        let live: std::collections::HashSet<&str> =
            tasks.iter().map(|task| task.id.as_str()).collect();

        if self.storage.worktrees_dir.is_dir() {
            for entry in fs::read_dir(&self.storage.worktrees_dir)?.flatten() {
                let id = entry.file_name().to_string_lossy().into_owned();
                if live.contains(id.as_str()) || !entry.path().is_dir() {
                    continue;
                }
                if repo.remove_worktree(&entry.path(), true).is_err() {
                    // Not (or no longer) a registered worktree: the
                    // directory itself is the leftover.
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }

        let iso = self.config.get_orchestration()?.isolation;
        for (branch, id) in repo.branches_with_prefix(&iso.branch_prefix)? {
            if live.contains(id.as_str()) {
                continue;
            }
            let _ = repo.delete_branch(&branch, true);
        }

        if repo.read_ref(&iso.integration_ref)?.is_some()
            && !tasks.iter().any(|task| task.worktree.is_some())
        {
            let snap = repo.snapshot(
                "HEAD",
                "kanban: re-baseline integration ref — no task holds a worktree",
            )?;
            repo.set_ref(&iso.integration_ref, &snap)?;
        }
        Ok(())
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
        task.reset_human_restart();
        // The run phase is a sub-state of In Progress: a recovered task left
        // that lifecycle, so a stale design/review marker must not decide the
        // role of whatever runs next.
        task.run_phase = None;
        // Back at the top of the pipeline: the next run plans again.
        task.designed = false;
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

            if is_agent {
                match Role::from_phase(task.run_phase) {
                    Role::Designer => {
                        return Err(KanbanError::Permission(
                            "designer cannot move a task; finish the design phase with kanban done"
                                .to_string(),
                        ));
                    }
                    Role::Reviewer => {
                        return Err(KanbanError::Permission(
                            "reviewer cannot move a task; finish with kanban verdict".to_string(),
                        ));
                    }
                    Role::Executor => {}
                }
                if self.config.get_rule("user_only_review_to_done")? {
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
            }

            if target_status == TaskStatus::Done.as_str() {
                let done = self.move_task_to_done(&task)?;
                (self.reset_moved_if_human(done, is_agent)?, false)
            } else {
                let moved = self.storage.move_task(task_id, target_status, is_agent)?;
                let entered_review = moved.as_ref().is_some_and(|m| {
                    task.status != TaskStatus::Review && m.status == TaskStatus::Review
                });
                (self.reset_moved_if_human(moved, is_agent)?, entered_review)
            }
        };

        if entered_review && let Some(moved) = &moved {
            self.trigger_chained_tasks(&moved.id)?;
        }
        Ok(moved)
    }

    fn reset_moved_if_human(&self, moved: Option<Task>, is_agent: bool) -> Result<Option<Task>> {
        if is_agent {
            return Ok(moved);
        }
        let Some(mut task) = moved else {
            return Ok(None);
        };
        task.reset_human_restart();
        // A human move ends whatever run was in flight. Leaving `design` or
        // `review` on the task would make the next agent launch the wrong bot
        // and would refuse every later agent move as if a phase bot owned it.
        task.run_phase = None;
        // Dragging a task back to To Do restarts the work from the top, so the
        // next run plans again. Any other human move keeps the existing plan.
        if task.status == TaskStatus::Todo {
            task.designed = false;
        }
        self.storage.save_task(&task)?;
        Ok(Some(task))
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
        // Belt and braces: a task that reached Done without a landing still
        // drops its isolated worktree and branch here — Done is terminal.
        let had_isolation = current.worktree.is_some() || current.branch.is_some();
        let problems = self.clear_task_worktree(&mut current, true);
        if !problems.is_empty() {
            self.post_queue_note(
                &current.id,
                &format!("⚠ done cleanup failed: {}", problems.join("; ")),
            );
        } else if had_isolation && current.worktree.is_none() {
            self.post_queue_note(
                &current.id,
                "🧹 done — isolated worktree and branch removed",
            );
        }

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
        self.take_task_inner(task_id, session_id, is_agent, false)
    }

    /// Queue-aware delegation. With `immediate` (the human `r` Run action)
    /// the launch always happens and any queued marker is cleared; otherwise
    /// a task whose concurrency caps are exhausted lands In Progress with run
    /// phase Queued instead of launching — the dispatcher starts it later.
    fn take_task_inner(
        &self,
        task_id: &str,
        session_id: &str,
        is_agent: bool,
        immediate: bool,
    ) -> Result<Option<Task>> {
        let session_mgr = self.session_manager();
        SessionManager::validate_session_id(session_id)?;
        let (task, previous_status, queued) = {
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

            task.reset_human_restart();
            if self.config.get_rule("auto_move_on_assign")? {
                task.status = TaskStatus::InProgress;
            }

            // Decide launch vs queue before anything is persisted.
            let mut queued = false;
            // A task whose plan is already on the thread skips straight to the
            // executor, however this run was started (see `upcoming_run_plan`).
            let designer_enabled =
                self.config.get_orchestration()?.designer_enabled_for(&task) && !task.designed;
            let will_auto_launch = is_agent
                && self.config.get_rule("auto_launch_on_delegate")?
                && self.auto_launch_enabled()?
                && task.status == TaskStatus::InProgress;
            if immediate {
                // A manual start bypasses the queue, but not an enabled
                // designer: the planning pass still runs first.
                task.run_phase = if designer_enabled {
                    Some(RunPhase::Design)
                } else {
                    None
                };
            } else if will_auto_launch && self.queue_is_full(&task)? {
                queued = true;
                stats::record_enter(
                    &self.storage.project_path,
                    &task.id,
                    stats::Phase::Queued,
                    &stats::Tags::default(),
                );
                task.run_phase = Some(RunPhase::Queued);
            } else if will_auto_launch && designer_enabled {
                task.run_phase = Some(RunPhase::Design);
            } else {
                // A fresh take starts a fresh run: never inherit the phase a
                // previous run left behind, or the executor would be launched
                // (and prompted) as the designer or the reviewer.
                task.run_phase = None;
            }

            // A queued task owns no launch, so only record the session when
            // it already exists (the delegated flow's live caller); a freshly
            // minted id would otherwise strand an Active record that paints
            // the card as running for a heartbeat timeout.
            let known_session = !queued || session_mgr.load_session(session_id).is_some();
            if known_session {
                task.session = Some(session_id.to_string());
            }
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            if known_session {
                session_mgr.link_named_session(task_id, session_id, &task.title)?;
            }
            (task, previous_status, queued)
        };

        if !queued
            && is_agent
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
            upcoming_run_plan(&self.config.load()?, &task)?.0.backend
        };
        let session_id = format!("ses-{}-{}", backend, timefmt::now().format("%Y%m%d-%H%M%S"));
        if self
            .take_task_inner(task_id, &session_id, true, true)?
            .is_none()
        {
            return Err(KanbanError::Invalid(format!(
                "Agent launch failed for {task_id}"
            )));
        }
        Ok(Some(session_id))
    }

    /// Explicitly queue a task for the dispatcher (the TUI `Q` action). To Do
    /// moves to In Progress; an idle In Progress task stays put. Either way
    /// the run phase becomes Queued and no agent launches — the dispatcher
    /// starts queued tasks once a slot frees up.
    pub fn enqueue_task(&self, task_id: &str) -> Result<Option<Task>> {
        self.queue_run(task_id)
    }

    /// Put a task into the orchestration queue: the queued counterpart of
    /// [`Self::start_task`]. To Do moves to In Progress, Review folds its
    /// pending review edits into the thread first, and an idle In Progress
    /// task stays put. The run phase becomes Queued and no agent launches —
    /// the dispatcher starts queued tasks once a slot frees up. A queued task
    /// owns no session (`task.session` is left alone); the dispatcher pins
    /// one when it actually starts the run.
    pub fn queue_run(&self, task_id: &str) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(None);
        };
        match task.status {
            TaskStatus::Todo => task.status = TaskStatus::InProgress,
            TaskStatus::InProgress => {}
            TaskStatus::Review => {
                self.fold_review_edits(&mut task)?;
                task.review_edits = String::new();
                task.review_unseen = false;
                task.status = TaskStatus::InProgress;
            }
            _ => {
                return Err(KanbanError::Invalid(format!(
                    "Task {task_id} must be To Do, In Progress, or Review to queue"
                )));
            }
        }
        if task
            .session
            .as_deref()
            .is_some_and(|s| self.session_manager().is_session_active(s))
        {
            return Err(KanbanError::Invalid(format!(
                "Task {task_id} already has a running agent"
            )));
        }
        task.reset_human_restart();
        stats::record_enter(
            &self.storage.project_path,
            &task.id,
            stats::Phase::Queued,
            &stats::Tags::default(),
        );
        task.run_phase = Some(RunPhase::Queued);
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        self.post_queue_note(&task.id, "⏸ queued — waiting for a free agent slot");
        Ok(Some(task))
    }

    /// Take a queued task back out (the TUI `Q` action on a queued card):
    /// the phase marker clears, the task stays In Progress with no live
    /// session, and `r` runs it immediately again.
    pub fn dequeue_task(&self, task_id: &str) -> Result<Option<Task>> {
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(None);
        };
        if task.run_phase != Some(RunPhase::Queued) {
            return Err(KanbanError::Invalid(format!(
                "Task {task_id} is not queued"
            )));
        }
        task.reset_human_restart();
        task.run_phase = None;
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        self.post_queue_note(&task.id, "⏸ taken out of the queue — run it manually");
        Ok(Some(task))
    }

    /// Best-effort audit line on the thread for a queue phase change.
    pub(crate) fn post_queue_note(&self, task_id: &str, body: &str) {
        let _ = self.thread_manager().and_then(|tm| {
            tm.post_with_origin(
                task_id,
                MessageRole::System,
                MessageKind::AgentStep,
                body,
                None,
                vec![],
                Some("kanban".to_string()),
                Some("kanban".to_string()),
            )
        });
    }

    /// Whether the orchestration queue would hold this task back: queueing
    /// is enabled and every cap applicable to the task's resolved
    /// backend/model is already consumed by live sessions.
    fn queue_is_full(&self, task: &Task) -> Result<bool> {
        let orch = self.config.get_orchestration()?;
        if !orch.queue_enabled {
            return Ok(false);
        }
        let config = self.config.load()?;
        let (settings, phase) = upcoming_run_plan(&config, task)?;
        Ok(!Slots::measure(self)?.has_room(
            &orch,
            &settings.backend,
            settings.model.as_deref(),
            role_for_phase(Some(phase)).as_str(),
        ))
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
        let resolver;
        let task = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };

            if is_agent && task.run_phase == Some(RunPhase::Design) {
                drop(_guard);
                return self.complete_design_phase(task_id, session_id);
            }
            if is_agent && task.run_phase == Some(RunPhase::Review) {
                return Err(KanbanError::Invalid(
                    "bot reviewer must finish with kanban verdict, not done".to_string(),
                ));
            }

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

                if self.should_start_bot_review(&task)? {
                    drop(_guard);
                    return self.start_bot_review(task_id, session_id);
                }

                resolver = self.land_on_review(&mut task);

                task.status = TaskStatus::Review;
                task.run_phase = None;
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

        // `on_conflict: resolver`: a conflicted landing re-dispatches the
        // agent immediately; the conflict report is already in the thread.
        if resolver {
            self.dispatch_resolver(&task.id);
        }

        Ok(Some(task))
    }

    /// Designer `kanban done`: stay In Progress, close the design session,
    /// flip `run_phase` to Execute, and start the task's assigned bot on the
    /// same slot. Re-queueing would stall a task whose slot is already paid
    /// for. A missing plan (no context) is an error so we never hand an
    /// empty thread to the executor.
    fn complete_design_phase(&self, task_id: &str, session_id: &str) -> Result<Option<Task>> {
        {
            let _guard = self.storage.lock()?;
            let Some(task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            if task.run_phase != Some(RunPhase::Design) || task.status != TaskStatus::InProgress {
                return Ok(None);
            }
            self.require_current_agent_session(&task, session_id)?;
            let context = self.context_manager().get_context(task_id, &self.storage)?;
            if context.trim().is_empty() {
                return Err(KanbanError::Permission(
                    "Designer cannot finish without recording a plan via context".to_string(),
                ));
            }
        }
        self.session_manager().close_session(session_id)?;
        self.advance_from_design(task_id, session_id)?;
        self.storage.load_task(task_id)
    }

    /// Hand the designer's slot to the executor: claim Execute under the
    /// lock and launch outside it. Caps are not re-checked — the slot is
    /// already accounted for.
    fn advance_from_design(&self, task_id: &str, designer_session: &str) -> Result<Option<String>> {
        let session_mgr = self.session_manager();
        let new_session_id = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            if task.status != TaskStatus::InProgress || task.run_phase != Some(RunPhase::Design) {
                return Ok(None);
            }
            if task.session.as_deref() != Some(designer_session) {
                return Ok(None);
            }
            let settings = resolve_task_launch_settings(&self.config.load()?, &task)?;
            let new_session_id = self.fresh_session_id(&safe_session_component(&settings.backend));
            task.run_phase = Some(RunPhase::Execute);
            // The plan is on the thread now. Record it so a later re-queue of
            // this task (crash restart, reviewer bounce) starts the executor
            // instead of paying for a second designer pass.
            task.designed = true;
            task.auto_resumes = 0;
            task.session = Some(new_session_id.clone());
            task.updated_at = timefmt::now();
            session_mgr.link_named_session(&task.id, &new_session_id, &task.title)?;
            if let Err(err) = self.storage.save_task(&task) {
                session_mgr.unlink_session(&new_session_id);
                return Err(err);
            }
            self.post_queue_note(
                &task.id,
                &format!(
                    "▶ design finished; starting executor session {new_session_id} ({})",
                    settings.backend
                ),
            );
            new_session_id
        };
        match self.finish_launch(
            &new_session_id,
            self.launch_agent(task_id, &new_session_id, false),
        ) {
            Ok(true) => Ok(Some(new_session_id)),
            failed => {
                let _ = self.schedule_crash_restart(task_id, "executor launch failed");
                failed?;
                Ok(Some(new_session_id))
            }
        }
    }

    /// Whether the executor's `done` should launch the reviewer bot instead
    /// of moving the task to human Review. Exhausted bounce budget falls
    /// through so a human can take over.
    fn should_start_bot_review(&self, task: &Task) -> Result<bool> {
        let orch = self.config.get_orchestration()?;
        if !orch.reviewer_enabled_for(task) {
            return Ok(false);
        }
        if matches!(
            task.run_phase,
            Some(RunPhase::Design) | Some(RunPhase::Review)
        ) {
            return Ok(false);
        }
        Ok(!self.review_rounds_exhausted(task, &orch.reviewer))
    }

    fn review_rounds_exhausted(
        &self,
        task: &Task,
        reviewer: &crate::core::config::ReviewerSettings,
    ) -> bool {
        reviewer.max_rounds > 0 && i64::from(task.review_rounds) >= reviewer.max_rounds
    }

    /// Keep the task In Progress, flip `run_phase` to Review, and launch the
    /// reviewer bot with `orchestration.reviewer` (not the task assignment).
    fn start_bot_review(&self, task_id: &str, executor_session: &str) -> Result<Option<Task>> {
        self.session_manager().close_session(executor_session)?;
        let orch = self.config.get_orchestration()?;
        let settings = resolve_bot_launch_settings(&self.config.load()?, &orch.reviewer.bot())?;
        let session_mgr = self.session_manager();
        let (task, session_id) = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            if task.status != TaskStatus::InProgress {
                return Ok(None);
            }
            let session_id = self.fresh_session_id(&safe_session_component(&settings.backend));
            task.run_phase = Some(RunPhase::Review);
            task.review_rounds = task.review_rounds.saturating_add(1);
            task.session = Some(session_id.clone());
            task.updated_at = timefmt::now();
            session_mgr.link_named_session(&task.id, &session_id, &task.title)?;
            if let Err(err) = self.storage.save_task(&task) {
                session_mgr.unlink_session(&session_id);
                return Err(err);
            }
            self.post_queue_note(
                &task.id,
                &format!(
                    "⚖ bot review started session {session_id} ({}) — round {}",
                    settings.backend, task.review_rounds
                ),
            );
            (task, session_id)
        };
        if self.auto_launch_enabled()? {
            match self.finish_launch(&session_id, self.launch_agent(task_id, &session_id, false)) {
                Ok(true) => {}
                failed => {
                    let _ = self.schedule_crash_restart(task_id, "reviewer launch failed");
                    failed?;
                }
            }
        }
        Ok(self.storage.load_task(task_id)?.or(Some(task)))
    }

    /// Reviewer-only exit: approve (human Review) or request changes.
    pub fn submit_verdict(
        &self,
        task_id: &str,
        session_id: &str,
        is_agent: bool,
        decision: Verdict,
    ) -> Result<Option<Task>> {
        if !is_agent {
            return Err(KanbanError::Permission(
                "kanban verdict is the reviewer bot's exit; pass --agent".to_string(),
            ));
        }
        SessionManager::validate_session_id(session_id)?;
        let orch = self.config.get_orchestration()?;
        let (task, route, resolver) = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            if task.run_phase != Some(RunPhase::Review) || task.status != TaskStatus::InProgress {
                return Err(KanbanError::Invalid(format!(
                    "Task {task_id} is not in a bot-review phase"
                )));
            }
            self.require_current_agent_session(&task, session_id)?;

            let route = match decision {
                Verdict::Approve => VerdictRoute::HumanReview { exhausted: false },
                Verdict::Changes(text) => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        return Err(KanbanError::Invalid(
                            "kanban verdict --changes requires non-empty text".to_string(),
                        ));
                    }
                    task.review_edits = text;
                    self.fold_review_edits(&mut task)?;
                    if self.review_rounds_exhausted(&task, &orch.reviewer) {
                        VerdictRoute::HumanReview { exhausted: true }
                    } else {
                        match orch.reviewer.on_changes_requested {
                            OnChangesRequested::Todo => VerdictRoute::Todo,
                            OnChangesRequested::InProgress => VerdictRoute::Requeue,
                        }
                    }
                }
            };

            let mut resolver = false;
            match route {
                VerdictRoute::HumanReview { exhausted } => {
                    resolver = self.land_on_review(&mut task);
                    apply_human_review(&mut task);
                    if exhausted {
                        self.post_queue_note(
                            &task.id,
                            "⚖ bot review budget spent — handing to human Review",
                        );
                    } else {
                        self.post_queue_note(
                            &task.id,
                            "⚖ reviewer approved — handing to human Review",
                        );
                    }
                }
                VerdictRoute::Todo => {
                    task.status = TaskStatus::Todo;
                    task.run_phase = None;
                    task.review_unseen = false;
                    task.updated_at = timefmt::now();
                    self.post_queue_note(
                        &task.id,
                        "⚖ reviewer requested changes — returned to To Do",
                    );
                }
                VerdictRoute::Requeue => {
                    task.status = TaskStatus::InProgress;
                    stats::record_enter(
                        &self.storage.project_path,
                        &task.id,
                        stats::Phase::Queued,
                        &stats::Tags::default(),
                    );
                    task.run_phase = Some(RunPhase::Queued);
                    task.review_unseen = false;
                    task.updated_at = timefmt::now();
                    self.post_queue_note(
                        &task.id,
                        "⚖ reviewer requested changes — queued for the task bot",
                    );
                }
            }
            self.storage.save_task(&task)?;
            (task, route, resolver)
        };

        self.session_manager().close_session(session_id)?;

        match route {
            VerdictRoute::HumanReview { exhausted } => {
                self.trigger_chained_tasks(&task.id)?;
                self.notify_completion(&task);
                if exhausted && let Ok(notifier) = self.notifier() {
                    notifier.stranded(
                        &task.id,
                        &task.title,
                        "Bot review budget spent; task is in human Review.",
                    );
                }
            }
            VerdictRoute::Todo => {}
            VerdictRoute::Requeue => {
                let _ = self.dispatch_queue()?;
            }
        }
        if resolver {
            self.dispatch_resolver(task_id);
        }
        Ok(self.storage.load_task(task_id)?.or(Some(task)))
    }

    /// Fold the pending `review_edits` buffer into the thread as a permanent
    /// `review_edit` message and clear it. Shared by human re-run and the
    /// reviewer `--changes` path.
    fn fold_review_edits(&self, task: &mut Task) -> Result<()> {
        let edits = task.review_edits.trim().to_string();
        if edits.is_empty() {
            task.review_edits.clear();
            return Ok(());
        }
        self.thread_manager()?.post_with_origin(
            &task.id,
            MessageRole::Human,
            MessageKind::ReviewEdit,
            &edits,
            None,
            vec![],
            Some("user".to_string()),
            Some("human".to_string()),
        )?;
        task.review_edits.clear();
        Ok(())
    }

    /// Maximum number of characters from the verification command's combined
    /// stdout/stderr that is stored in the gate-failed `AgentStep` message.
    const VERIFICATION_OUTPUT_TAIL: usize = 2000;

    /// Run the configured verification command where the task's agent worked
    /// (its worktree when isolated, else the work folder — it builds and
    /// tests the code, not the board) and return its exit code plus
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
            .current_dir(self.task_cwd(task_id))
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
            let backend = upcoming_run_plan(&self.config.load()?, &task)?.0.backend;
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
    ) -> Result<Option<AnswerOutcome>> {
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

            // An explicit MSG-id is only answerable when it really is a
            // question: `answer` stamps `answered` plus the answer body onto
            // whatever it is handed, so without this a typo'd id silently
            // marks a task or context message answered while the actual
            // question stays open — and, with `resume_after_last_answer`,
            // is weighed for a wake that the human never unblocked.
            let is_question = tm
                .get_message(&task.id, &msg_id)?
                .is_some_and(|message| message.kind == MessageKind::Question);
            if !is_question {
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

        // Once every open question is answered, the human has unblocked the
        // run: relaunch the agent unless it is still alive and polling for
        // the answer itself. A failed resume must never fail the answer —
        // it is already durably recorded — so it becomes a thread note.
        let remaining = tm
            .open_messages(&task.id, Some(MessageKind::Question))?
            .len();
        let mut resumed_session = None;
        let mut queued = false;
        if task.status == TaskStatus::InProgress && remaining == 0 {
            match self.resume_answered_agent(&task.id, expected_session.as_deref()) {
                Ok(Some(resumed)) => {
                    resumed_session = resumed.session.clone();
                    queued =
                        resumed.run_phase == Some(RunPhase::Queued) && resumed.session.is_none();
                    task = resumed;
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = tm.post(
                        &task.id,
                        MessageRole::System,
                        MessageKind::System,
                        &format!("Answer recorded, but the agent could not be relaunched: {err}"),
                        None,
                        vec![],
                        Some("kanban".to_string()),
                    );
                }
            }
        }
        Ok(Some(AnswerOutcome {
            task,
            remaining,
            resumed_session,
            queued,
        }))
    }

    fn resume_answered_agent(
        &self,
        task_id: &str,
        expected_session: Option<&str>,
    ) -> Result<Option<Task>> {
        if !self.auto_launch_enabled()? || !self.config.get_rule("resume_after_last_answer")? {
            return Ok(None);
        }

        let session_mgr = self.session_manager();
        {
            let _guard = self.storage.lock()?;
            let Some(current) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            let tm = self.thread_manager()?;
            if current.status != TaskStatus::InProgress
                || tm.has_open_questions(task_id)?
                || current.session.as_deref() != expected_session
            {
                return Ok(None);
            }
            if let Some(session) = expected_session
                && let Some(record) = session_mgr.load_session(session)
                && record.task_id == current.id
            {
                let state = session_mgr.session_state(
                    session,
                    self.config.get_threshold("session_heartbeat_timeout")?,
                );
                match state {
                    // A live session with no declared wait is the `ask --wait`
                    // poller: it heartbeats every poll iteration and wakes
                    // itself on the answer.
                    Some(SessionState::Live) if record.wait_until.is_none() => {
                        return Ok(None);
                    }
                    // A stale heartbeat leaves the record Active long after
                    // the process died; mark it crashed so the revoke below
                    // sees "process already gone" and can replace it.
                    Some(SessionState::Crashed) if record.status == SessionStatus::Active => {
                        session_mgr.crash_session(session)?;
                    }
                    _ => {}
                }
            }
        }

        self.revoke_in_progress_task(task_id, expected_session)
    }

    /// `queued` means waiting for a slot. Any path that claims a session
    /// outside the dispatcher must replace it with the phase that run will
    /// occupy, or the card keeps saying queued while an agent is already live.
    fn claim_run_phase(&self, task: &mut Task) -> Result<()> {
        if task.run_phase != Some(RunPhase::Queued) {
            return Ok(());
        }
        stats::record_exit(&self.storage.project_path, &task.id, stats::Phase::Queued);
        task.run_phase = Some(upcoming_run_plan(&self.config.load()?, task)?.1);
        Ok(())
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

        // A paused session has already released its slot, so waking it must
        // re-acquire one through the queue (below) instead of launching past
        // the caps.
        let was_paused = expected_session
            .and_then(|session| session_mgr.load_session(session))
            .is_some_and(|record| {
                record.status == SessionStatus::Active && record.wait_until.is_some()
            });

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
                        "Wake requested while the background agent was still exiting; resume through the queue as soon as its process ends.",
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

        if was_paused && self.queue_can_dispatch()? {
            // The pause had already released its slot, so the wake re-enters
            // the queue instead of launching past the caps; the dispatcher
            // starts it in board order. `F` run now remains the direct
            // override, and a live or crashed session still wakes instantly
            // on the path below.
            self.thread_manager()?.post_with_origin(
                &task.id,
                MessageRole::System,
                MessageKind::Context,
                &format!(
                    "Session {} was revoked while paused. Queued for a free agent slot — wake \
                     immediately on the fresh session the dispatcher starts and continue from \
                     the current thread context.",
                    expected_session.unwrap_or("none")
                ),
                None,
                vec![],
                Some("kanban".to_string()),
                Some("kanban".to_string()),
            )?;
            // Same run-replacement bookkeeping as the direct wake below.
            task.reset_auto_restart();
            stats::record_enter(
                &self.storage.project_path,
                &task.id,
                stats::Phase::Queued,
                &stats::Tags::default(),
            );
            task.run_phase = Some(RunPhase::Queued);
            task.session = None;
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            if let Some(old_session) = expected_session {
                // Ownership has already moved and the old process is gone. A
                // failed archival write must not strand the published successor.
                let _ = session_mgr.close_session(old_session);
            }
            self.post_queue_note(
                &task.id,
                "⏸ revoked while paused — queued for a free agent slot",
            );
            let _ = self.dispatch_queue();
            return Ok(Some(task));
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
        // A wake replaces the session of a run that is still the same run, so
        // the reviewer bounce count survives it — see `reset_auto_restart`.
        task.reset_auto_restart();
        self.claim_run_phase(&mut task)?;
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
    pub(crate) fn fresh_session_id(&self, backend: &str) -> String {
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
    /// `review_edit` message, clear it, and relaunch the agent. With
    /// [`RunMode::Queued`] (the default everywhere) no agent launches: the
    /// task lands In Progress with run phase Queued for the dispatcher.
    pub fn rerun_review_task(
        &self,
        task_id: &str,
        session_id: Option<&str>,
        mode: RunMode,
    ) -> Result<Option<Task>> {
        if mode == RunMode::Queued {
            return self.queue_run(task_id);
        }
        if let Some(session_id) = session_id {
            SessionManager::validate_session_id(session_id)?;
        }
        let (task, session_id) = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            self.fold_review_edits(&mut task)?;

            let backend = self.resolve_backend(&task)?;
            let session_id = match session_id {
                Some(s) => s.to_string(),
                None => format!("ses-{}-{}", backend, timefmt::now().format("%Y%m%d-%H%M%S")),
            };
            task.review_edits = String::new();
            task.session = Some(session_id.clone());
            task.reset_human_restart();
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

    /// Move a stalled or questioned in-progress task to a fresh agent
    /// session. With [`RunMode::Queued`] the old session is still closed but
    /// no new one launches: the task waits in the queue and the dispatcher
    /// starts the run.
    pub fn rerun_in_progress_task(
        &self,
        task_id: &str,
        session_id: Option<&str>,
        mode: RunMode,
    ) -> Result<Option<Task>> {
        if let Some(session_id) = session_id {
            SessionManager::validate_session_id(session_id)?;
        }
        let session_mgr = self.session_manager();
        let queued = mode == RunMode::Queued;
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

            let new_session_note = if queued {
                "queued — the dispatcher starts it".to_string()
            } else {
                let backend = self.resolve_backend(&task)?;
                match session_id {
                    Some(s) => s.to_string(),
                    None => {
                        format!("ses-{}-{}", backend, timefmt::now().format("%Y%m%d-%H%M%S"))
                    }
                }
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
                    new_session_note
                ),
                None,
                vec![],
                Some("kanban".to_string()),
            )?;

            // Re-running a stranded In Progress session continues the same
            // attempt on a fresh session; the reviewer bounce count is not
            // part of what went wrong, so it is not cleared here.
            task.reset_auto_restart();
            if queued {
                // A queued run owns no session; the dispatcher mints one
                // when it starts the task.
                task.session = None;
                stats::record_enter(
                    &self.storage.project_path,
                    &task.id,
                    stats::Phase::Queued,
                    &stats::Tags::default(),
                );
                task.run_phase = Some(RunPhase::Queued);
            } else {
                task.session = Some(new_session_note.clone());
                self.claim_run_phase(&mut task)?;
            }
            task.has_questions = tm.has_open_questions(&task.id)?;
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            if !queued {
                session_mgr.link_named_session(&task.id, &new_session_note, &task.title)?;
            }
            (task, new_session_note)
        };

        if queued {
            self.post_queue_note(&task.id, "⏸ queued — waiting for a free agent slot");
            return Ok(Some(task));
        }

        if self.auto_launch_enabled()?
            && !self.finish_launch(&session_id, self.launch_agent(&task.id, &session_id, false))?
        {
            return Ok(None);
        }
        Ok(Some(task))
    }

    // --------------------------------------------------------------- launch

    pub(crate) fn auto_launch_enabled(&self) -> Result<bool> {
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
        Ok(resolve_launch_settings(&self.config.load()?, task)?.backend)
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

    pub(crate) fn launch_agent(
        &self,
        task_id: &str,
        session_id: &str,
        revert: bool,
    ) -> Result<bool> {
        let Some(task) = self.storage.load_task(task_id)? else {
            return Err(KanbanError::Invalid(format!(
                "Task {task_id} not found. Agent not started."
            )));
        };
        let task = self.record_launch_settings(task)?;
        let task = self.prepare_worktree(task)?;
        // Owned so `roots` can borrow the worktree path for its lifetime.
        let worktree = self.task_worktree_path(&task);
        let roots = match &worktree {
            Some(path) => Roots::new(self.data_root(), path, self.project_id.as_deref()),
            None => self.roots(),
        };
        self.log_launch_step(&roots, &task, session_id, revert);
        match self.launcher.launch(roots, &task, session_id, revert) {
            Ok(started) => Ok(started),
            Err(err) => {
                self.log_launch_failure(&task, session_id, &err.to_string());
                Err(err)
            }
        }
    }

    /// The task's isolated checkout, when it exists on disk.
    fn task_worktree_path(&self, task: &Task) -> Option<PathBuf> {
        let rel = task.worktree.as_deref()?;
        let path = self.storage.worktrees_dir.join(rel);
        path.is_dir().then_some(path)
    }

    /// Where a task's agent-side processes run: its worktree when isolated,
    /// else the shared work folder.
    pub(crate) fn task_cwd(&self, task_id: &str) -> PathBuf {
        self.storage
            .load_task(task_id)
            .ok()
            .flatten()
            .and_then(|task| self.task_worktree_path(&task))
            .unwrap_or_else(|| self.work_path().to_path_buf())
    }

    /// Create the task's isolated checkout before its agent launches (TASK-236).
    /// Nothing happens unless `orchestration.isolation` is on, the board is a
    /// registered project (agent callbacks resolve via `KANBAN_PROJECT`, which
    /// a worktree outside the repo cannot provide by path), and the work
    /// folder is a git repo new enough for isolation. When the task already
    /// carries a worktree it is reused as-is, so re-runs continue the same
    /// branch. `mode: auto` falls back to the shared folder with an audit
    /// note when isolation is unavailable; `mode: required` refuses the
    /// launch.
    ///
    /// The integration snapshot chains under the board lock: parent = the
    /// configured integration ref's current tip, or HEAD on the first task —
    /// so two tasks starting at once get strictly-descendant bases and their
    /// merge-base is the shared snapshot, not committed HEAD.
    fn prepare_worktree(&self, task: Task) -> Result<Task> {
        let iso = &self.config.get_orchestration()?.isolation;
        if iso.mode == IsolationMode::Off {
            return Ok(task);
        }
        if task
            .worktree
            .as_deref()
            .is_some_and(|rel| self.storage.worktrees_dir.join(rel).is_dir())
        {
            return Ok(task);
        }
        match self.create_worktree(&task, iso) {
            Ok(updated) => Ok(updated),
            Err(reason) => {
                if iso.mode == IsolationMode::Required {
                    return Err(KanbanError::Invalid(format!(
                        "worktree isolation is required but unavailable: {reason}"
                    )));
                }
                self.post_queue_note(
                    &task.id,
                    &format!(
                        "⚠ worktree isolation unavailable ({reason}) — running in the shared folder"
                    ),
                );
                Ok(task)
            }
        }
    }

    /// The availability gate plus the actual snapshot + `worktree add`. The
    /// `Err` payload is a human-readable unavailability reason. Everything
    /// from reading the integration ref to saving the task fields holds the
    /// board lock, so two tasks starting at once chain their snapshots
    /// instead of racing sibling ones.
    fn create_worktree(
        &self,
        task: &Task,
        iso: &IsolationSettings,
    ) -> std::result::Result<Task, String> {
        if self.project_id.is_none() {
            return Err("project not registered".to_string());
        }
        let repo =
            vcs::detect(self.work_path()).ok_or_else(|| "not a git repository".to_string())?;
        if !repo.has_commits().map_err(|e| e.to_string())? {
            return Err("unborn HEAD (no commits yet)".to_string());
        }

        let _guard = self.storage.lock().map_err(|e| e.to_string())?;
        let base = self
            .isolation_base_commit(&repo, task, iso)
            .map_err(|e| e.to_string())?;
        let branch = format!("{}{}", iso.branch_prefix, task.id);
        let rel = task.id.clone();
        let path = self.storage.worktrees_dir.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        repo.add_worktree(&path, &branch, &base)
            .map_err(|e| format!("git worktree add failed: {e}"))?;

        let Some(mut current) = self
            .storage
            .load_task(&task.id)
            .map_err(|e| e.to_string())?
        else {
            return Err(format!("task {} vanished during launch", task.id));
        };
        current.worktree = Some(rel);
        current.branch = Some(branch);
        current.base_commit = Some(base.as_str().to_string());
        current.updated_at = timefmt::now();
        self.storage
            .save_task(&current)
            .map_err(|e| e.to_string())?;
        self.post_queue_note(
            &task.id,
            &format!(
                "⑂ isolated checkout {} on {} (base {})",
                path.display(),
                current.branch.as_deref().unwrap_or("-"),
                base.as_str()
            ),
        );
        Ok(current)
    }

    /// The commit a task branch starts from. `seed: live` snapshots the dirty
    /// work folder onto the integration chain (parent = the ref's tip, or
    /// HEAD the first time) and advances the ref; `seed: head` branches from
    /// committed HEAD and touches no ref.
    fn isolation_base_commit(
        &self,
        repo: &vcs::GitRepo,
        task: &Task,
        iso: &IsolationSettings,
    ) -> Result<vcs::Oid> {
        match iso.seed {
            IsolationSeed::Head => repo.head_oid(),
            IsolationSeed::Live => {
                let parent = repo
                    .read_ref(&iso.integration_ref)?
                    .map(|oid| oid.as_str().to_string())
                    .unwrap_or_else(|| "HEAD".to_string());
                let message = format!("kanban: live snapshot before {}", task.id);
                let snap = repo.snapshot(&parent, &message)?;
                repo.set_ref(&iso.integration_ref, &snap)?;
                Ok(snap)
            }
        }
    }

    /// Landing entry for the move to human Review (TASK-248): merge the
    /// task's branch into the work folder without ever committing on the
    /// user's branch or staging anything. Runs under the board lock held by
    /// the caller, so two tasks landing at once serialize against the same
    /// integration tip. Every failure is reported on the thread and defers
    /// the landing; it never blocks the move to Review. Returns true when a
    /// conflicted landing must auto-dispatch a resolver run (`on_conflict:
    /// resolver`) — the caller does that after releasing the board lock.
    fn land_on_review(&self, task: &mut Task) -> bool {
        if task.branch.is_none() || task.worktree.is_none() {
            return false;
        }
        let iso = match self.config.get_orchestration() {
            Ok(orch) => orch.isolation.clone(),
            Err(err) => {
                self.post_queue_note(
                    &task.id,
                    &format!("⚠ landing deferred: config unavailable ({err})"),
                );
                return false;
            }
        };
        if iso.land == IsolationLand::Manual {
            task.integration = IntegrationState::Pending;
            self.post_queue_note(
                &task.id,
                &format!(
                    "⑂ branch {} ready — land it with \"kanban integrate {}\"",
                    task.branch.as_deref().unwrap_or("-"),
                    task.id
                ),
            );
            return false;
        }
        match self.land_task_branch(task) {
            Ok(LandOutcome::Conflict { .. }) => iso.on_conflict == IsolationOnConflict::Resolver,
            Ok(_) => false,
            Err(err) => {
                self.post_queue_note(
                    &task.id,
                    &format!(
                        "⚠ landing deferred: {err} — run \"kanban integrate {}\"",
                        task.id
                    ),
                );
                false
            }
        }
    }

    /// Auto-dispatch the resolver run for a conflicted landing
    /// (`on_conflict: resolver`): [`Self::rerun_review_task`] folds the
    /// conflict report into the thread and relaunches the agent on a fresh
    /// session. Must be called with no board lock held.
    fn dispatch_resolver(&self, task_id: &str) {
        if let Err(err) = self.rerun_review_task(task_id, None, RunMode::Immediate) {
            self.post_queue_note(
                task_id,
                &format!(
                    "⚠ resolver dispatch failed: {err} — the conflict report is \
                     in the review edits"
                ),
            );
        }
    }

    /// The landing sequence (steps 1–5 of TASK-248). The caller holds the
    /// board lock. Git-level problems come back as [`LandOutcome::Deferred`]
    /// with the reason already on the thread; board-level errors (config)
    /// propagate.
    fn land_task_branch(&self, task: &mut Task) -> Result<LandOutcome> {
        let (Some(branch), Some(wt_rel)) = (&task.branch, &task.worktree) else {
            return Ok(LandOutcome::NotIsolated);
        };
        let wt_path = self.storage.worktrees_dir.join(wt_rel);
        if !wt_path.is_dir() {
            return Ok(self.defer_landing(task, "the isolated worktree directory is gone"));
        }
        let Some(repo) = vcs::detect(self.work_path()) else {
            return Ok(self.defer_landing(task, "the work folder is no longer a git repository"));
        };
        let iso = self.config.get_orchestration()?.isolation.clone();

        // 1. Commit everything the agent left uncommitted, so it still lands.
        let tip = match repo.commit_all(
            &wt_path,
            &format!("kanban: {} uncommitted work at landing", task.id),
        ) {
            Ok(Some(oid)) => oid,
            Ok(None) => match repo.read_ref(branch)? {
                Some(oid) => oid,
                None => {
                    return Ok(self.defer_landing(task, &format!("branch {branch} does not exist")));
                }
            },
            Err(err) => return Ok(self.defer_landing(task, &format!("commit failed: {err}"))),
        };

        // 2. W = the human's work folder right now (dirty files included),
        //    parented on the integration tip so concurrent lands chain.
        let parent = repo
            .read_ref(&iso.integration_ref)?
            .map(|oid| oid.as_str().to_string())
            .unwrap_or_else(|| "HEAD".to_string());
        let w = match repo.snapshot(
            &parent,
            &format!("kanban: work folder snapshot before landing {}", task.id),
        ) {
            Ok(oid) => oid,
            Err(err) => return Ok(self.defer_landing(task, &format!("snapshot failed: {err}"))),
        };

        // 3. Preflight the merge in the object database; nothing is written.
        let pre = match repo.preflight(&w, branch) {
            Ok(pre) => pre,
            Err(err) => {
                return Ok(self.defer_landing(task, &format!("merge preflight failed: {err}")));
            }
        };

        match pre {
            vcs::Preflight::Clean { tree } => {
                // 4. Write only the differing paths as unstaged changes
                //    (race-guarded), then advance the integration ref to a
                //    commit of the merged tree with parents [integration,
                //    task branch]. Never on the user's branch.
                let changed = match repo.materialize(&w, &tree) {
                    Ok(changed) => changed,
                    Err(err) => return Ok(self.defer_landing(task, &err.to_string())),
                };
                let old = repo.read_ref(&iso.integration_ref)?;
                let mut parents: Vec<String> = Vec::new();
                if let Some(old) = old {
                    parents.push(old.as_str().to_string());
                }
                parents.push(tip.as_str().to_string());
                let parent_refs: Vec<&str> = parents.iter().map(String::as_str).collect();
                let land = match repo.commit_tree(
                    &tree,
                    &parent_refs,
                    &format!("kanban: land {}", task.id),
                ) {
                    Ok(oid) => oid,
                    Err(err) => {
                        return Ok(self.defer_landing(
                            task,
                            &format!("creating the integration commit failed: {err}"),
                        ));
                    }
                };
                if let Err(err) = repo.set_ref(&iso.integration_ref, &land) {
                    return Ok(self.defer_landing(
                        task,
                        &format!("advancing the integration ref failed: {err}"),
                    ));
                }
                task.integration = IntegrationState::Landed;
                let files = changed
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.post_queue_note(
                    &task.id,
                    &format!(
                        "⇩ landed {branch} into the work folder ({} path(s): {files}) — \
                         unstaged changes only, integration at {land}",
                        changed.len()
                    ),
                );
                self.cleanup_after_land(task, &iso);
                Ok(LandOutcome::Landed { changed })
            }
            vcs::Preflight::Conflict { paths, stages } => {
                // 5. Nothing was written anywhere. The human side is merged
                //    INTO the task's own worktree, so the conflict markers
                //    appear only in that isolated checkout (work_path stays
                //    untouched), and the resolution is routed through the
                //    review-edits buffer (TASK-249): the human edits the text
                //    and re-dispatches, or — with `on_conflict: resolver` —
                //    a resolver run is dispatched immediately.
                task.integration = IntegrationState::Conflict;
                let merged = repo.merge_into_worktree(&wt_path, w.as_str());
                // Advance the integration ref to W, so the next landing
                // snapshots on top of it. Without this the next snapshot is
                // parented on the pre-conflict integration tip again, the
                // merge base never reaches W, and the human's work-folder
                // edit keeps conflicting with the resolution — the
                // resolve-in-the-worktree-then-`done` loop the conflict
                // report describes could never converge. W is a
                // fast-forward of the ref (it was snapshotted on it) and
                // carries only the work folder's own state, so no task's
                // landed work is touched and no unlanded branch becomes an
                // ancestor of the ref.
                if let Err(err) = repo.set_ref(&iso.integration_ref, &w) {
                    self.post_queue_note(
                        &task.id,
                        &format!(
                            "⚠ could not record the conflict snapshot on {} ({err}) — \
                             resolve in the worktree and re-run \"kanban integrate {}\"",
                            iso.integration_ref, task.id
                        ),
                    );
                }
                task.review_edits = self.conflict_report(task, &wt_path, &stages, &merged);
                let list = paths.join(", ");
                let tail = match iso.on_conflict {
                    IsolationOnConflict::Resolver => "; resolver run dispatched",
                    IsolationOnConflict::Review => "; conflict report is in the review edits",
                };
                self.post_queue_note(
                    &task.id,
                    &format!(
                        "⇩ merge conflict landing {branch}: {list} — work folder untouched{tail}"
                    ),
                );
                Ok(LandOutcome::Conflict { paths })
            }
        }
    }

    /// The structured conflict report routed through the review-edits buffer
    /// (TASK-249): the conflicting paths with all three versions as blob ids,
    /// the base commit the task started from, where the isolated checkout
    /// lives, and the instruction to resolve there and finish with
    /// `kanban done` — which re-runs the landing.
    fn conflict_report(
        &self,
        task: &Task,
        wt_path: &Path,
        stages: &[vcs::Stage],
        merged: &Result<()>,
    ) -> String {
        let mut text = format!(
            "Merge conflict: landing {} into the work folder conflicts with the \
             current work-folder state; nothing was written to the work folder.\n\n\
             Conflicting paths (`git cat-file blob <id>` prints a version):\n",
            task.branch.as_deref().unwrap_or("-"),
        );
        let mut current = String::new();
        for stage in stages {
            if stage.path != current {
                current = stage.path.clone();
                text.push_str(&format!("- {}\n", stage.path));
            }
            let label = match stage.stage {
                1 => "base   (stage 1)".to_string(),
                2 => "ours   (stage 2, work folder)".to_string(),
                _ => "theirs (stage 3, task branch)".to_string(),
            };
            text.push_str(&format!("  {label}: {}\n", stage.oid));
        }
        text.push_str(&format!(
            "\nTask base commit: {}\nWorktree: {}\n\n",
            task.base_commit.as_deref().unwrap_or("unknown"),
            wt_path.display(),
        ));
        match merged {
            Ok(()) => text.push_str(
                "The work-folder side is already merged into the worktree, so the \
                 conflict markers are in that isolated checkout: run `git status` \
                 there, resolve every conflicted file, and commit.\n",
            ),
            Err(err) => text.push_str(&format!(
                "The automatic merge into the worktree failed ({err}); resolve the \
                 conflicting paths above in the worktree by hand.\n",
            )),
        }
        text.push_str(&format!(
            "Then finish with\n\n    kanban done {} --session <session> --agent\n\n\
             done re-runs the landing; a clean merge writes the resolution into \
             the work folder and cleans up the worktree.\n",
            task.id
        ));
        text
    }

    /// Record a landing failure on the thread and return [`LandOutcome::Deferred`].
    fn defer_landing(&self, task: &Task, reason: &str) -> LandOutcome {
        self.post_queue_note(
            &task.id,
            &format!(
                "⚠ landing deferred: {reason} — nothing was written; \
                 run \"kanban integrate {}\" once resolved",
                task.id
            ),
        );
        LandOutcome::Deferred(reason.to_string())
    }

    /// Default post-landing cleanup (`cleanup: on_land`): remove the
    /// isolated checkout and the task branch through the shared
    /// [`Self::clear_task_worktree`], keeping the landed gate — the branch
    /// was just merged into the integration ref, so the gate passing is the
    /// sanity check.
    fn cleanup_after_land(&self, task: &mut Task, iso: &IsolationSettings) {
        if iso.cleanup != IsolationCleanup::OnLand {
            return;
        }
        let had_isolation = task.worktree.is_some() || task.branch.is_some();
        let problems = self.clear_task_worktree(task, false);
        if !problems.is_empty() {
            self.post_queue_note(
                &task.id,
                &format!("⚠ landing cleanup failed: {}", problems.join("; ")),
            );
        } else if had_isolation && task.worktree.is_none() {
            self.post_queue_note(&task.id, "🧹 landed — isolated worktree and branch removed");
        }
    }

    /// `kanban integrate <TASK-ID>`: re-run the landing after a conflict was
    /// resolved (or for `land: manual` boards). Same sequence, same board
    /// lock, as the done-time landing. A conflicted re-land on a
    /// `on_conflict: resolver` board dispatches the resolver run like the
    /// done-time landing does.
    pub fn integrate_task(&self, task_id: &str) -> Result<Option<(Task, LandOutcome)>> {
        let (task, outcome, resolver) = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            if task.integration == IntegrationState::Landed {
                return Err(KanbanError::Invalid(format!(
                    "Task {task_id} is already landed; run it again to land new work"
                )));
            }
            if task.branch.is_none() || task.worktree.is_none() {
                return Err(KanbanError::Invalid(format!(
                    "Task {task_id} has no isolated branch to integrate"
                )));
            }
            let outcome = self.land_task_branch(&mut task)?;
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            let resolver = matches!(outcome, LandOutcome::Conflict { .. })
                && self.config.get_orchestration()?.isolation.on_conflict
                    == IsolationOnConflict::Resolver;
            (task, outcome, resolver)
        };
        if resolver {
            self.dispatch_resolver(task_id);
        }
        Ok(Some((task, outcome)))
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
    pub(crate) fn finish_launch(&self, session_id: &str, launched: Result<bool>) -> Result<bool> {
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
    fn log_launch_step(&self, roots: &Roots<'_>, task: &Task, session_id: &str, revert: bool) {
        let prompt_path = self
            .storage
            .logs_dir
            .join(format!("{session_id}.prompt.txt"));
        if let Ok(prompt) = build_agent_prompt(
            *roots,
            task,
            session_id,
            revert,
            Role::from_phase(task.run_phase),
        ) {
            let _ = atomic_write_text(&prompt_path, &prompt);
        }

        let rel_prompt_path = prompt_path
            .strip_prefix(self.data_root())
            .unwrap_or(&prompt_path)
            .display();
        let settings = resolve_launch_settings(&self.config.load().unwrap_or_default(), task).ok();
        let body = format!(
            "▶ launch session={session_id} backend={} model={} effort={} agent={} revert={revert} → prompt: {rel_prompt_path}",
            settings
                .as_ref()
                .map(|s| s.backend.as_str())
                .unwrap_or(task.agent_backend.as_deref().unwrap_or("-")),
            settings
                .as_ref()
                .and_then(|s| s.model.as_deref())
                .unwrap_or(task.ai_model.as_deref().unwrap_or("-")),
            settings
                .as_ref()
                .and_then(|s| s.effort.as_deref())
                .unwrap_or(task.ai_effort.as_deref().unwrap_or("-")),
            settings
                .as_ref()
                .and_then(|s| s.agent.as_deref())
                .unwrap_or(task.agent_name.as_deref().unwrap_or("-")),
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
        // Designer/reviewer bots must not overwrite the task's assigned
        // executor backend/model/effort/agent — those fields are what the
        // next phase launches with.
        if matches!(task.run_phase, Some(RunPhase::Design | RunPhase::Review)) {
            return Ok(task);
        }
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
            .current_dir(self.task_cwd(task_id))
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
        // Harvest before reconciliation: a clean stranded exit may launch its
        // successor inside `reconcile_agent_exit_inner`, and Codex/pi/omp need the
        // just-finished backend conversation id to reopen it. Capture the
        // reply first as well, so a fresh-launch fallback sees all prior work.
        let manifest = session_matched_task
            .then(|| self.harvest_provenance(task_id, session_id))
            .flatten();
        if session_matched_task {
            if manifest.is_some() {
                self.warn_write_overlaps(task_id, session_id);
            }
            self.record_agent_reply(task_id, session_id);
        }
        let outcome = self.reconcile_agent_exit_inner(task_id, session_id, exit_status)?;
        if session_matched_task {
            self.log_exit_step(
                task_id,
                session_id,
                exit_status,
                &outcome,
                manifest.as_ref(),
            );
        }
        // An exit freed a slot (or re-filled one via the auto-resume); pump
        // the queue so the launch wrapper starts queued work even with no
        // TUI open. Best effort — never disturbs the reconciled exit.
        let _ = self.dispatch_queue();
        Ok(outcome)
    }

    /// Harvest the backend's machine transcript into an input-provenance
    /// manifest (`.kanban/provenance/<session>.yaml`) recording what the run
    /// actually consumed — files read into context (including via Bash), files
    /// written, URLs, MCP calls. Best-effort and backend-gated: claude,
    /// codex, opencode, and the pi family emit parseable transcripts, and any
    /// failure is a soft warning
    /// that never disturbs the reconciled exit.
    fn harvest_provenance(&self, task_id: &str, session_id: &str) -> Option<InputManifest> {
        let task = self.storage.load_task(task_id).ok().flatten()?;
        let backend = resolve_launch_settings(&self.config.load().ok()?, &task)
            .ok()?
            .backend;
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
        // those live where the agent worked — its worktree when isolated, so
        // paths stay repo-relative and comparable across tasks.
        let repo_root = self
            .task_worktree_path(&task)
            .unwrap_or_else(|| self.work_path().to_path_buf());
        let harvester: Box<dyn TranscriptHarvester> = match backend.as_str() {
            "claude" => Box::new(ClaudeHarvester {
                session_id: session,
                prompt_dump,
                root: repo_root,
            }),
            "codex" => Box::new(CodexHarvester {
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

    /// Every (pair, path) where two sessions from **different** tasks, whose
    /// lifetimes overlapped, both wrote the same file. Joins the harvested
    /// manifests with their session records: a manifest whose session record
    /// is gone (task completion clears session files) cannot be attributed
    /// and is skipped. Detection only — callers decide how to report it.
    pub fn detect_write_overlaps(&self) -> Vec<provenance::WriteOverlap> {
        let sessions = self.session_manager().list_sessions();
        let now = timefmt::now();
        let Ok(entries) = fs::read_dir(&self.storage.provenance_dir) else {
            return Vec::new();
        };
        let mut joined = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(session_id) = name.to_str().and_then(|n| n.strip_suffix(".yaml")) else {
                continue;
            };
            let Some(record) = sessions.iter().find(|s| s.id == session_id) else {
                continue;
            };
            let Some(manifest) =
                provenance::load_manifest(&self.storage.provenance_dir, session_id)
            else {
                continue;
            };
            // A still-active session owns its files through the present.
            let end = if record.status == SessionStatus::Active {
                now
            } else {
                record.ended_at.unwrap_or(record.last_seen)
            };
            joined.push((manifest, record.task_id.clone(), (record.started_at, end)));
        }
        let views: Vec<provenance::SessionWrites<'_>> = joined
            .iter()
            .map(|(manifest, task_id, window)| provenance::SessionWrites {
                manifest,
                task_id,
                window: *window,
            })
            .collect();
        provenance::overlapping_writes(&views)
    }

    /// Visible warning for a provenance overlap: when the session that just
    /// exited and a session of another task ran concurrently and both wrote
    /// the same path, post a `context` message on **both** tasks' threads so
    /// the human (and the next prompt) sees that the file may have been
    /// clobbered. Deduped per task: a thread that already carries a message
    /// naming both session ids is left alone. Best-effort — any failure is
    /// silently ignored, the exit has already been reconciled.
    fn warn_write_overlaps(&self, task_id: &str, session_id: &str) {
        let mut groups: Vec<(String, String, Vec<String>)> = Vec::new();
        for overlap in self
            .detect_write_overlaps()
            .into_iter()
            .filter(|o| o.session_a == session_id || o.session_b == session_id)
        {
            let (peer_session, peer_task) = if overlap.session_a == session_id {
                (overlap.session_b, overlap.task_b)
            } else {
                (overlap.session_a, overlap.task_a)
            };
            if let Some(group) = groups.iter_mut().find(|(peer, _, _)| peer == &peer_session) {
                group.2.push(overlap.path);
            } else {
                groups.push((peer_session, peer_task, vec![overlap.path]));
            }
        }
        for (peer_session, peer_task, mut paths) in groups {
            paths.sort();
            let body = format!(
                "⚠ provenance overlap: {task_id} (session {session_id}) and {peer_task} \
                 (session {peer_session}) ran concurrently and both wrote: {}. The last \
                 writer's content wins; verify nothing was silently clobbered.",
                paths.join(", ")
            );
            for target in [task_id, peer_task.as_str()] {
                let already_warned = self
                    .thread_manager()
                    .and_then(|tm| tm.messages_of_kind(target, MessageKind::Context))
                    .map(|messages| {
                        messages.iter().any(|message| {
                            message.body.contains(session_id)
                                && message.body.contains(&peer_session)
                        })
                    })
                    .unwrap_or(false);
                if !already_warned {
                    let _ = self.context_manager().append_context_with_session(
                        target,
                        &body,
                        "system",
                        None,
                        &self.storage,
                    );
                }
            }
        }
    }

    /// Post the agent's whole session answer — every assistant text it printed
    /// during the run, in order — to the task thread as a `context` message
    /// authored by `agent-reply`. Without this the thread holds only the audit
    /// trail (launch, agent-written context, exit) while the answer itself
    /// stays buried in `.kanban/logs/<session>.log`. It is taken from the
    /// backend's own machine transcript, so it is what the agent actually said
    /// rather than prose it chose to re-type through `kanban context`.
    ///
    /// Best-effort: any failure is a soft warning — the exit has already been
    /// reconciled by the time this runs. Setting the `agent_reply_max_chars`
    /// threshold to 0 turns the whole behavior off; otherwise it caps how much
    /// of a long reply is kept, since every thread entry is replayed into the
    /// next prompt. The budget is spent from the run's last message backwards
    /// (see [`reply::compose_reply`]) so the answer the agent finished on is
    /// never the part that gets cut, and `agent_reply_message_max_chars` keeps
    /// one long mid-run message from crowding out the rest.
    fn record_agent_reply(&self, task_id: &str, session_id: &str) {
        let Ok(max_chars) = self.config.get_threshold("agent_reply_max_chars") else {
            return;
        };
        if max_chars <= 0 {
            return;
        }
        let message_max_chars = self
            .config
            .get_threshold("agent_reply_message_max_chars")
            .unwrap_or(0)
            .max(0) as usize;
        let Some(backend) = self.task_backend(task_id) else {
            return;
        };
        let transcript = self
            .storage
            .logs_dir
            .join(format!("{session_id}.transcript.jsonl"));
        let Some(messages) = reply::session_messages(&backend, &transcript) else {
            return;
        };
        let log_path = self.storage.logs_dir.join(format!("{session_id}.log"));
        let body = reply::compose_reply(
            &messages,
            max_chars as usize,
            message_max_chars,
            &log_path.display().to_string(),
        );
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

    /// Post a transcript `type: error` on the thread and decide how
    /// crash-restart should run. No error (or a plain retryable one) keeps the
    /// backoff ladder; `isRetryable: false` stays crashed so a credits/auth
    /// failure is not painted as `↻ retry`; a spent subscription quota waits
    /// for the window it named, since every earlier attempt can only 429 again.
    fn crash_restart_plan(&self, task_id: &str, session_id: &str) -> CrashRestart {
        let transcript = self
            .storage
            .logs_dir
            .join(format!("{session_id}.transcript.jsonl"));
        let Some(event) = reply::fatal_error_event(&transcript) else {
            return CrashRestart::Backoff;
        };
        let Some(err) = provenance::stream_error(&event) else {
            return CrashRestart::Backoff;
        };
        self.post_queue_note(task_id, &format!("✖ agent error: {}", err.message));
        if !err.retryable {
            self.post_queue_note(
                task_id,
                "↻ crash-restart skipped — backend error is not retryable",
            );
            return CrashRestart::Skip;
        }
        let Some(retry_at) = err.retry_at else {
            return CrashRestart::Backoff;
        };
        // The 429 carries the provider's own usage numbers; record them so the
        // board's limits row shows the spent quota now rather than at its next
        // poll. Only OpenAI (codex) sends them, and only through opencode.
        self.record_provider_usage(&event);
        let Some(at) = DateTime::from_timestamp(retry_at, 0) else {
            return CrashRestart::Backoff;
        };
        CrashRestart::After(at.with_timezone(&Local).naive_local())
    }

    /// Feed the usage headers on a rate-limit error into the limits cache.
    /// Best effort: a provider that sends none simply leaves the row alone.
    fn record_provider_usage(&self, event: &Value) {
        let Some(headers) = provenance::stream_error_data(event).and_then(|data| {
            data.get("responseHeaders")
                .filter(|headers| headers.is_object())
        }) else {
            return;
        };
        limits::record_codex_usage(
            limits::parse_codex_usage_headers(headers),
            chrono::Utc::now().timestamp(),
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
            let cause = format!("agent exited with code {exit_status}");
            match self.crash_restart_plan(task_id, session_id) {
                CrashRestart::Skip => {}
                CrashRestart::Backoff => {
                    let _ = self.schedule_crash_restart(task_id, &cause);
                }
                CrashRestart::After(at) => {
                    let _ = self.schedule_crash_restart_at(task_id, Some(at), &cause);
                }
            }
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

    /// End every declared wait whose deadline has passed while the agent
    /// process is gone. A pause releases its slot, so the normal path parks
    /// the task back into the dispatcher queue ([`Self::dispatch_queue`] is
    /// pumped right after); only a board where the queue could never drain
    /// keeps the old direct relaunch — the fresh session is told to check
    /// the awaited result, report, and either finish or declare a new wait.
    /// Called from the TUI tick, the daemon, `kanban check-sessions`, and
    /// the wait-resume monitor.
    pub fn wake_expired_waits(&self) -> Result<Vec<WaitWake>> {
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
            if self.queue_can_dispatch()? {
                // The pause held no slot, so waking re-acquires one through
                // the dispatcher instead of launching past the caps.
                if self.queue_expired_wait(&session, &note, attempt)? {
                    resumed.push(WaitWake::Queued {
                        task_id: session.task_id.clone(),
                    });
                } else {
                    // Fenced out: the task moved on without this session.
                    session_mgr.close_session(&session.id)?;
                }
                continue;
            }
            match self.respawn_session(&session.task_id, &session.id, &note, Some(attempt))? {
                RespawnOutcome::Spawned(new_session) => {
                    session_mgr.close_session(&session.id)?;
                    resumed.push(WaitWake::Resumed {
                        task_id: session.task_id.clone(),
                        session_id: new_session,
                    });
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
        if resumed
            .iter()
            .any(|wake| matches!(wake, WaitWake::Queued { .. }))
        {
            // A parked task needs a pump to actually start, and this call
            // is often the only thing awake at the deadline. Best-effort,
            // like the pump in `reconcile_agent_exit`; `dispatch_queue`
            // never calls back into this method, so there is no recursion.
            let _ = self.dispatch_queue();
        }
        Ok(resumed)
    }

    /// End one pause by parking the task in the dispatcher queue instead of
    /// launching past the caps. Fenced exactly like [`Self::respawn_session`]:
    /// `Ok(false)` when the task is gone or was handed to a different session
    /// in the meantime — the caller then closes the stale session.
    fn queue_expired_wait(&self, session: &Session, note: &str, attempt: u32) -> Result<bool> {
        let session_mgr = self.session_manager();
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(&session.task_id)? else {
            return Ok(false);
        };
        if task.status != TaskStatus::InProgress
            || task.session.as_deref() != Some(session.id.as_str())
        {
            return Ok(false);
        }
        // `context` kind so the queued run's prompt still carries the wait
        // reason, exactly like the direct respawn did.
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
        task.auto_resumes = attempt;
        stats::record_enter(
            &self.storage.project_path,
            &task.id,
            stats::Phase::Queued,
            &stats::Tags::default(),
        );
        task.run_phase = Some(RunPhase::Queued);
        task.session = None;
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        session_mgr.close_session(&session.id)?;
        self.post_queue_note(
            &task.id,
            "⏸ wait deadline passed — queued for a free agent slot",
        );
        Ok(true)
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
            self.claim_run_phase(&mut task)?;
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

    /// Remove the task's isolated worktree and its branch — the worktree
    /// counterpart of [`Self::clear_task_backups`], called when the task's
    /// lifecycle ends: after a successful landing, on Done, and on abandon.
    /// `allow_unmerged: false` keeps the landed gate (the landing path,
    /// where the branch was just merged into the integration ref); the
    /// terminal human paths pass `true`, because dropping or finishing a
    /// task discards its branch with it. A `Conflict` task is never touched
    /// here: its worktree is the one place unmerged agent work lives.
    /// Returns the git problems hit along the way; the task fields are
    /// cleared only when everything actually went away.
    fn clear_task_worktree(&self, task: &mut Task, allow_unmerged: bool) -> Vec<String> {
        if task.integration == IntegrationState::Conflict {
            return Vec::new();
        }
        let (Some(branch), Some(rel)) = (task.branch.clone(), task.worktree.clone()) else {
            return Vec::new();
        };
        let Some(repo) = vcs::detect(self.work_path()) else {
            return Vec::new();
        };
        let mut problems: Vec<String> = Vec::new();
        let wt_path = self.storage.worktrees_dir.join(&rel);
        if wt_path.is_dir() {
            // The checkout dies with the task; --force keeps modified or
            // untracked leftovers from wedging the removal.
            if let Err(err) = repo.remove_worktree(&wt_path, true) {
                problems.push(format!("worktree: {err}"));
            } else {
                // git removes what it knew about; an agent process still
                // exiting can recreate a stray file (a tool's cache dir)
                // under the path afterwards. A leftover directory would make
                // `git worktree add` refuse this task id forever, silently
                // dropping it back to the shared folder on every later run,
                // so sweep the residue. The path is always
                // `.kanban/worktrees/<TASK-ID>` — board data, never the
                // user's work folder.
                let _ = fs::remove_dir_all(&wt_path);
            }
        } else {
            // The directory is already gone (deleted by hand): drop the
            // stale registration so the branch can still be reclaimed.
            let _ = repo.prune_worktrees();
        }
        if let Err(err) = repo.delete_branch(&branch, allow_unmerged) {
            problems.push(format!("branch: {err}"));
        }
        if problems.is_empty() {
            task.worktree = None;
            task.branch = None;
            task.base_commit = None;
        }
        problems
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

pub(crate) fn safe_session_component(value: &str) -> String {
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
