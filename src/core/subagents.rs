//! Subagents (TASK-594): child runs of a parent executor. A subagent is
//! stored as a task file (`kind: subagent`) so storage, sessions, provenance
//! and stats are shared, but it never enters a board column, the board
//! queue, Review, the bot reviewer, or landing. It is visible and controlled
//! only through its parent (`kanban sub …`):
//!
//! - it starts at once, outside the board queue, under its own caps
//!   (`orchestration.subagents`); beyond `max_parallel_per_parent` it waits
//!   FIFO in the parent's own queue;
//! - `sub done` makes it terminal and keeps its branch for the parent, which
//!   merges it into the parent's worktree (`sub merge`) or drops it
//!   (`sub discard`); the project only changes when the parent lands;
//! - a parent waiting with `sub wait` has no deadline: the last subagent to
//!   reach a terminal state wakes it directly, without the board queue and
//!   without spending `auto_resumes`. A stall watchdog replaces the ETA.

use std::fs;

use crate::core::config::{SpawnHelperModel, SpawnPolicy};
use crate::core::error::{KanbanError, Result};
use crate::core::models::{
    MessageKind, MessageRole, RunPhase, SessionStatus, SubState, Task, TaskKind, TaskStatus,
    WaitTrigger,
};
use crate::core::operations::{
    ChildState, Operations, PoolOutcome, RespawnOutcome, WaitWake, safe_session_component,
};
use crate::core::storage::NewTask;
use crate::core::timefmt;
use crate::core::vcs;

/// `kanban sub spawn`. `from_parent: None` resolves to "not readonly": a
/// writing subagent starts from a snapshot of the parent's worktree,
/// uncommitted edits included.
#[derive(Debug, Default, Clone)]
pub struct SubSpawnRequest {
    pub parent_id: String,
    pub title: String,
    pub description: String,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub readonly: bool,
    pub from_parent: Option<bool>,
}

/// Result of `kanban sub wait`.
#[derive(Debug)]
pub enum SubWait {
    /// Every target is already terminal; nothing was declared.
    Finished(Vec<ChildState>),
    /// A trigger wait is declared; the agent must end its reply.
    Waiting(Vec<ChildState>),
}

/// Grace before a running subagent whose session ended without `sub done`
/// and without a scheduled restart is failed: covers the moment between a
/// crash and its restart being scheduled.
const ORPHAN_GRACE_SECS: i64 = 60;

impl Operations {
    /// Subagents of `parent_id`, oldest first (the parent's FIFO order).
    pub fn subagents_of(&self, parent_id: &str) -> Result<Vec<Task>> {
        self.list_subagents(Some(parent_id))
    }

    /// Every subagent on the board (or of one parent), oldest first.
    pub fn list_subagents(&self, parent_id: Option<&str>) -> Result<Vec<Task>> {
        let mut subs: Vec<Task> = self
            .storage
            .list_tasks(None)?
            .into_iter()
            .filter(|task| {
                task.is_subagent()
                    && parent_id.is_none_or(|parent| task.parent_task.as_deref() == Some(parent))
            })
            .collect();
        subs.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(subs)
    }

    pub fn live_subagents(&self, parent_id: &str) -> Result<Vec<Task>> {
        Ok(self
            .subagents_of(parent_id)?
            .into_iter()
            .filter(Task::is_live_subagent)
            .collect())
    }

    /// Create a subagent of the parent whose executor session is
    /// `session_id` and start it (or queue it behind its siblings).
    pub fn sub_spawn(&self, request: SubSpawnRequest, session_id: &str) -> Result<Task> {
        let id = {
            let _guard = self.storage.lock()?;
            let parent = self
                .storage
                .load_task(&request.parent_id)?
                .ok_or_else(|| KanbanError::Invalid("parent task not found".into()))?;
            self.require_current_agent_session(&parent, session_id)?;
            if !matches!(parent.run_phase, None | Some(RunPhase::Execute)) {
                return Err(KanbanError::Permission(
                    "only the parent's executor session may spawn subagents".into(),
                ));
            }
            let effective = self.effective_spawn(&parent)?;
            if effective.policy == SpawnPolicy::Forbidden {
                return Err(KanbanError::Permission(format!(
                    "spawning subagents is forbidden for {} (from {})",
                    parent.id, effective.source
                )));
            }
            let children = self
                .storage
                .list_tasks(None)?
                .iter()
                .filter(|t| {
                    (t.spawned || t.is_subagent()) && t.parent_task.as_deref() == Some(&parent.id)
                })
                .count();
            if children >= effective.max_children {
                return Err(KanbanError::Permission(format!(
                    "{} reached orchestration.subagents.max_children ({})",
                    parent.id, effective.max_children
                )));
            }
            let explicit =
                request.backend.is_some() || request.model.is_some() || request.effort.is_some();
            let orch = self.config.get_orchestration()?;
            let profile = if explicit {
                None
            } else {
                match effective.helper_model {
                    SpawnHelperModel::Same => None,
                    SpawnHelperModel::ExecutorMiddle => Some("middle"),
                    SpawnHelperModel::ExecutorCheap => Some("cheap"),
                }
            }
            .filter(|profile| {
                if *profile == "middle" {
                    !orch.executors.middle.is_empty()
                } else {
                    !orch.executors.cheap.is_empty()
                }
            });
            let from_parent = request.from_parent.unwrap_or(!request.readonly);
            let seed = if from_parent {
                self.parent_snapshot(&parent)?
            } else {
                None
            };
            let same_backend = request.backend.is_none() || request.backend == parent.agent_backend;
            let mut task = self.create_task(NewTask {
                title: request.title,
                description: request.description,
                agent_backend: if profile.is_some() {
                    None
                } else {
                    request.backend.or(parent.agent_backend.clone())
                },
                ai_model: if profile.is_some() {
                    None
                } else {
                    request
                        .model
                        .or_else(|| same_backend.then(|| parent.ai_model.clone()).flatten())
                },
                ai_effort: if profile.is_some() {
                    None
                } else {
                    request
                        .effort
                        .or_else(|| same_backend.then(|| parent.ai_effort.clone()).flatten())
                },
                agent_name: if profile.is_none() && same_backend {
                    parent.agent_name.clone()
                } else {
                    None
                },
                explicit_assignment: profile.is_none(),
                permission: if request.readonly {
                    Some("readonly".into())
                } else {
                    parent.permission().map(str::to_owned)
                },
                // Questions are always refused: unclear points go into the
                // summary the parent reads.
                silence: true,
                parent_task: Some(parent.id.clone()),
                role_profile: profile.map(str::to_owned),
                ..NewTask::default()
            })?;
            task.kind = Some(TaskKind::Subagent);
            task.sub_state = Some(SubState::Queued);
            task.status = TaskStatus::InProgress;
            // Legacy readers (and the spawn-ancestry depth walk) still see a
            // spawned helper.
            task.spawned = true;
            task.spawn_policy = Some(SpawnPolicy::Allowed);
            task.explicit_assignment = profile.is_none();
            task.seed_commit = seed.clone();
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            let threads = self.thread_manager()?;
            threads.post_board_note(
                &parent.id,
                &format!(
                    "Subagent {}: {}. Read its result with kanban sub output {}.",
                    task.id, task.title, task.id
                ),
            )?;
            threads.post_board_note(
                &task.id,
                &match &seed {
                    Some(seed) => format!(
                        "Subagent of {} (worktree seeded from the parent's worktree at {seed}).",
                        parent.id
                    ),
                    None => format!("Subagent of {}.", parent.id),
                },
            )?;
            task.id
        };
        self.start_queued_subagents()?;
        self.storage
            .load_task(&id)?
            .ok_or_else(|| KanbanError::Invalid(format!("subagent {id} vanished")))
    }

    /// Snapshot the parent's isolated worktree (dirty files included)
    /// without touching its index or HEAD. `None` when the parent runs in
    /// the shared folder: a subagent's ordinary live snapshot of the project
    /// already sees the same files.
    fn parent_snapshot(&self, parent: &Task) -> Result<Option<String>> {
        let Some(path) = self.task_worktree_path(parent) else {
            return Ok(None);
        };
        let repo = vcs::detect(&path).ok_or_else(|| {
            KanbanError::Invalid(format!(
                "parent worktree {} is not a git checkout",
                path.display()
            ))
        })?;
        vcs::populate_worktree(&path)?;
        let snap = repo.snapshot(
            "HEAD",
            &format!("kanban: {} worktree snapshot for a subagent", parent.id),
        )?;
        Ok(Some(snap.as_str().to_string()))
    }

    /// Start queued subagents while their caps allow, oldest first. Returns
    /// the ids that started.
    pub fn start_queued_subagents(&self) -> Result<Vec<String>> {
        let mut started = Vec::new();
        for task in self.list_subagents(None)? {
            if task.sub_state == Some(SubState::Queued) && self.launch_subagent(&task.id)? {
                started.push(task.id);
            }
        }
        Ok(started)
    }

    /// Launch one queued subagent on a fresh session, bypassing the board
    /// queue. `false` when it is not queued, has no room under the
    /// `orchestration.subagents` caps, or its launch failed (it is then
    /// failed, which wakes the parent).
    fn launch_subagent(&self, task_id: &str) -> Result<bool> {
        let settings = self.config.get_orchestration()?.subagents;
        let session_id = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(false);
            };
            if !task.is_subagent() || task.sub_state != Some(SubState::Queued) {
                return Ok(false);
            }
            let running: Vec<Task> = self
                .list_subagents(None)?
                .into_iter()
                .filter(|t| t.sub_state == Some(SubState::Running))
                .collect();
            let siblings = running
                .iter()
                .filter(|t| t.parent_task == task.parent_task)
                .count();
            if running.len() >= settings.max_running_total
                || siblings >= settings.max_parallel_per_parent
            {
                return Ok(false);
            }
            if task.role_profile.is_some()
                && let PoolOutcome::Parked { blocked, .. } = self.apply_executor_pool(&mut task)?
            {
                drop(_guard);
                self.fail_subagent(
                    task_id,
                    &format!(
                        "every executor pool candidate is out of quota ({})",
                        blocked.join(", ")
                    ),
                )?;
                return Ok(false);
            }
            let backend = safe_session_component(&self.resolve_backend(&task)?);
            let session_id = self.fresh_session_id(&backend);
            task.status = TaskStatus::InProgress;
            task.run_phase = None;
            task.sub_state = Some(SubState::Running);
            task.sub_reason = None;
            task.restart_at = None;
            task.session = Some(session_id.clone());
            task.updated_at = timefmt::now();
            self.storage.save_task(&task)?;
            self.session_manager()
                .link_named_session(&task.id, &session_id, &task.title)?;
            session_id
        };
        match self.finish_launch(&session_id, self.launch_agent(task_id, &session_id, false)) {
            Ok(true) => Ok(true),
            Ok(false) => {
                let reason = if self.auto_launch_enabled()? {
                    "agent launch failed"
                } else {
                    "agent launch failed: auto_launch is disabled on this board"
                };
                self.fail_subagent(task_id, reason)?;
                Ok(false)
            }
            Err(err) => {
                self.fail_subagent(task_id, &format!("agent launch failed: {err}"))?;
                Ok(false)
            }
        }
    }

    /// `kanban sub done` (also what `kanban done` does from a subagent's
    /// session): record the summary as the subagent's result, commit its
    /// worktree onto its branch, and make it terminal — no Review, no bot
    /// reviewer, no landing. `session_id: None` is the human override.
    pub fn sub_done(
        &self,
        task_id: &str,
        session_id: Option<&str>,
        summary: Option<&str>,
    ) -> Result<Task> {
        let task = {
            let _guard = self.storage.lock()?;
            let mut task = self.load_subagent(task_id)?;
            if task.sub_state == Some(SubState::Done)
                && session_id.is_none_or(|s| task.session.as_deref() == Some(s))
            {
                return Ok(task);
            }
            if task.sub_state.is_some_and(SubState::is_terminal) {
                return Err(KanbanError::Invalid(format!(
                    "subagent {task_id} is already {}",
                    task.sub_state.map(|s| s.as_str()).unwrap_or("-")
                )));
            }
            if let Some(session_id) = session_id {
                self.require_current_agent_session(&task, session_id)?;
            }
            if let Some(summary) = summary.map(str::trim).filter(|s| !s.is_empty()) {
                self.context_manager().append_context_with_session(
                    task_id,
                    summary,
                    "agent",
                    session_id,
                    &self.storage,
                )?;
                if let Some(current) = self.storage.load_task(task_id)? {
                    task = current;
                }
            }
            self.commit_subagent_work(&task, "result");
            self.finish_subagent(&mut task, SubState::Done, None)?;
            task
        };
        if let Some(session_id) = session_id {
            self.session_manager().close_session(session_id)?;
        }
        self.subagent_settled(&task);
        Ok(task)
    }

    /// `kanban sub cancel`: kill a live subagent and make it terminal.
    /// Cancelling a terminal subagent is a no-op.
    pub fn sub_cancel(&self, task_id: &str, reason: &str) -> Result<Task> {
        self.stop_subagent(task_id, SubState::Cancelled, reason)
    }

    /// Mark a live subagent failed (launch failure, spent crash budget,
    /// stall watchdog) and wake its parent.
    pub fn fail_subagent(&self, task_id: &str, reason: &str) -> Result<Task> {
        self.stop_subagent(task_id, SubState::Failed, reason)
    }

    fn stop_subagent(&self, task_id: &str, state: SubState, reason: &str) -> Result<Task> {
        let (task, session) = {
            let _guard = self.storage.lock()?;
            let mut task = self.load_subagent(task_id)?;
            if task.sub_state.is_some_and(SubState::is_terminal) {
                return Ok(task);
            }
            let session = task.session.clone();
            self.finish_subagent(&mut task, state, Some(reason))?;
            (task, session)
        };
        // Its own children go with it.
        let _ = self.cancel_live_subagents(task_id, &format!("parent {task_id} {state}"));
        if let Some(session) = session {
            let sessions = self.session_manager();
            if sessions.is_session_active(&session) {
                if state == SubState::Failed {
                    sessions.crash_session(&session)?;
                } else {
                    sessions.close_session(&session)?;
                }
            }
            let _ = crate::agent::kill_session(&session);
        }
        self.commit_subagent_work(&task, state.as_str());
        if let Some(parent) = task.parent_task.as_deref() {
            let _ = self.thread_manager().and_then(|threads| {
                threads.post_board_note(parent, &format!("Subagent {} {state}: {reason}", task.id))
            });
        }
        self.subagent_settled(&task);
        Ok(task)
    }

    /// Make `task` terminal under the board lock held by the caller. The
    /// board column becomes `done` only for older readers.
    fn finish_subagent(
        &self,
        task: &mut Task,
        state: SubState,
        reason: Option<&str>,
    ) -> Result<()> {
        let now = timefmt::now();
        task.sub_state = Some(state);
        task.sub_reason = reason.map(str::to_owned);
        task.status = TaskStatus::Done;
        task.run_phase = None;
        task.restart_at = None;
        task.completed_at = Some(now);
        task.updated_at = now;
        self.storage.save_task(task)?;
        self.post_queue_note(
            &task.id,
            &match reason {
                Some(reason) => format!("■ subagent {state}: {reason}"),
                None => format!("■ subagent {state} — branch kept for the parent"),
            },
        );
        Ok(())
    }

    /// Best-effort commit of whatever the subagent left uncommitted in its
    /// worktree, so `sub output` and `sub merge` see it on the branch.
    fn commit_subagent_work(&self, task: &Task, what: &str) {
        let Some(path) = self.task_worktree_path(task) else {
            return;
        };
        let Some(repo) = vcs::detect(self.work_path()) else {
            return;
        };
        if let Err(err) = repo.commit_all(&path, &format!("kanban: subagent {} {what}", task.id)) {
            self.post_queue_note(
                &task.id,
                &format!("⚠ commit of subagent work failed: {err}"),
            );
        }
    }

    /// Cancel (and kill) every live subagent of `parent_id`. Used when the
    /// parent's run ends: back to To Do, stop, recover, a new run, a move,
    /// or `done --cancel-subagents`.
    pub fn cancel_live_subagents(&self, parent_id: &str, reason: &str) -> Result<Vec<String>> {
        let mut cancelled = Vec::new();
        for sub in self.live_subagents(parent_id)? {
            self.sub_cancel(&sub.id, reason)?;
            cancelled.push(sub.id);
        }
        Ok(cancelled)
    }

    /// Refuse when `parent_id` still has live subagents (the parent's
    /// `kanban done`).
    pub fn require_no_live_subagents(&self, parent_id: &str) -> Result<()> {
        let live = self.live_subagents(parent_id)?;
        if live.is_empty() {
            return Ok(());
        }
        Err(KanbanError::Permission(format!(
            "{parent_id} has live subagents ({}); wait with kanban sub wait, cancel them with \
             kanban sub cancel <id>, or finish with done --cancel-subagents",
            live.iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }

    /// Archive or delete a parent's subagents together with it.
    pub(crate) fn cascade_subagents(&self, parent_id: &str, to: Option<TaskStatus>) -> Result<()> {
        let _ = self.cancel_live_subagents(parent_id, &format!("parent {parent_id} closed"));
        for sub in self.subagents_of(parent_id)? {
            match to {
                Some(status) => {
                    let _guard = self.storage.lock()?;
                    if let Some(mut current) = self.storage.load_task(&sub.id)? {
                        let _ = self.clear_task_worktree(&mut current, true);
                        current.status = status;
                        current.updated_at = timefmt::now();
                        self.storage.save_task(&current)?;
                    }
                }
                None => {
                    self.abandon_task(&sub.id)?;
                }
            }
        }
        Ok(())
    }

    /// Drop the worktrees and branches of a parent's subagents: the parent
    /// is Done, so nothing can merge them any more.
    pub(crate) fn clear_subagent_worktrees(&self, parent_id: &str) -> Result<()> {
        let _ = self.cancel_live_subagents(parent_id, &format!("parent {parent_id} done"));
        for sub in self.subagents_of(parent_id)? {
            let _guard = self.storage.lock()?;
            if let Some(mut current) = self.storage.load_task(&sub.id)?
                && (current.worktree.is_some() || current.branch.is_some())
            {
                let _ = self.clear_task_worktree(&mut current, true);
                self.storage.save_task(&current)?;
            }
        }
        Ok(())
    }

    /// `kanban sub merge`: merge a finished subagent's branch into the
    /// parent's worktree (its uncommitted edits are committed first so the
    /// merge cannot be refused over them). The project folder is untouched;
    /// it changes only when the parent lands. On conflict the markers are
    /// left in the parent's worktree and the subagent's branch is kept.
    pub fn sub_merge(&self, task_id: &str) -> Result<String> {
        let _guard = self.storage.lock()?;
        let mut sub = self.load_subagent(task_id)?;
        if !sub.sub_state.is_some_and(SubState::is_terminal) {
            return Err(KanbanError::Invalid(format!(
                "subagent {task_id} is still {}; wait for it first",
                sub.sub_state.map(|s| s.as_str()).unwrap_or("-")
            )));
        }
        let Some(branch) = sub.branch.clone() else {
            return Ok(format!(
                "{task_id} ran in the shared folder; its edits are already there"
            ));
        };
        let parent_id = sub.parent_task.clone().unwrap_or_default();
        let parent = self
            .storage
            .load_task(&parent_id)?
            .ok_or_else(|| KanbanError::Invalid(format!("parent {parent_id} not found")))?;
        let parent_wt = self.task_worktree_path(&parent).ok_or_else(|| {
            KanbanError::Invalid(format!(
                "parent {parent_id} has no isolated worktree; merge {branch} by hand"
            ))
        })?;
        let repo = vcs::detect(self.work_path()).ok_or_else(|| {
            KanbanError::Invalid("the work folder is not a git repository".into())
        })?;
        self.commit_subagent_work(&sub, "work at merge");
        repo.commit_all(
            &parent_wt,
            &format!("kanban: {parent_id} work before merging subagent {task_id}"),
        )?;
        let conflicts = repo.merge_branch_into_worktree(
            &parent_wt,
            &branch,
            &format!("kanban: merge subagent {task_id} into {parent_id}"),
        )?;
        if !conflicts.is_empty() {
            let message = format!(
                "merge conflict merging {branch} into {}: {} — resolve the markers there and commit",
                parent_wt.display(),
                conflicts.join(", ")
            );
            self.post_queue_note(task_id, &format!("⇩ {message}"));
            return Err(KanbanError::Invalid(message));
        }
        let problems = self.clear_task_worktree(&mut sub, true);
        sub.integration = crate::core::models::IntegrationState::Landed;
        sub.updated_at = timefmt::now();
        self.storage.save_task(&sub)?;
        let message = format!("merged {branch} into {parent_id}'s worktree");
        self.post_queue_note(task_id, &format!("⇩ {message}"));
        if !problems.is_empty() {
            self.post_queue_note(
                task_id,
                &format!("⚠ cleanup after merge failed: {}", problems.join("; ")),
            );
        }
        Ok(message)
    }

    /// `kanban sub discard`: drop a finished subagent's worktree and branch.
    pub fn sub_discard(&self, task_id: &str) -> Result<()> {
        let _guard = self.storage.lock()?;
        let mut sub = self.load_subagent(task_id)?;
        if !sub.sub_state.is_some_and(SubState::is_terminal) {
            return Err(KanbanError::Invalid(format!(
                "subagent {task_id} is still running; cancel it first"
            )));
        }
        let problems = self.clear_task_worktree(&mut sub, true);
        if !problems.is_empty() {
            return Err(KanbanError::Invalid(problems.join("; ")));
        }
        sub.updated_at = timefmt::now();
        self.storage.save_task(&sub)?;
        self.post_queue_note(task_id, "🧹 discarded — worktree and branch removed");
        Ok(())
    }

    /// `kanban sub send`: run a finished subagent again with a follow-up
    /// from the parent. It re-enters the parent's queue and continues its
    /// own branch and conversation.
    pub fn sub_send(&self, task_id: &str, message: &str) -> Result<Task> {
        {
            let _guard = self.storage.lock()?;
            let mut sub = self.load_subagent(task_id)?;
            if !sub.sub_state.is_some_and(SubState::is_terminal) {
                return Err(KanbanError::Invalid(format!(
                    "subagent {task_id} is still running; wait for it first"
                )));
            }
            let parent = sub.parent_task.clone().unwrap_or_default();
            self.thread_manager()?.post_with_origin(
                task_id,
                MessageRole::System,
                MessageKind::Context,
                &format!("Follow-up from parent {parent}: {}", message.trim()),
                None,
                vec![],
                Some("kanban".to_string()),
                Some("kanban".to_string()),
            )?;
            sub.sub_state = Some(SubState::Queued);
            sub.sub_reason = None;
            sub.status = TaskStatus::InProgress;
            sub.integration = crate::core::models::IntegrationState::None;
            sub.reset_auto_restart();
            sub.updated_at = timefmt::now();
            self.storage.save_task(&sub)?;
        }
        self.start_queued_subagents()?;
        self.load_subagent(task_id)
    }

    /// Board-wide states of `ids` for `sub wait` / `sub list`.
    pub fn sub_states(&self, ids: &[String]) -> Result<Vec<ChildState>> {
        self.wait_poll(ids)
    }

    /// `kanban sub wait`: with no ids, every live subagent of the parent.
    /// Declares a wait with no deadline (`wait_trigger: subagents`) unless
    /// every target is already terminal.
    pub fn sub_wait(&self, parent_id: &str, session_id: &str, ids: &[String]) -> Result<SubWait> {
        let parent = self
            .storage
            .load_task(parent_id)?
            .ok_or_else(|| KanbanError::Invalid(format!("task {parent_id} not found")))?;
        self.require_current_agent_session(&parent, session_id)?;
        let subs = self.subagents_of(parent_id)?;
        let targets: Vec<String> = if ids.is_empty() {
            subs.iter()
                .filter(|t| t.is_live_subagent())
                .map(|t| t.id.clone())
                .collect()
        } else {
            for id in ids {
                if !subs.iter().any(|t| &t.id == id) {
                    return Err(KanbanError::Invalid(format!(
                        "{id} is not a subagent of {parent_id}"
                    )));
                }
            }
            ids.to_vec()
        };
        if targets.is_empty() {
            let all: Vec<String> = subs.iter().map(|t| t.id.clone()).collect();
            return Ok(SubWait::Finished(self.sub_states(&all)?));
        }
        let states = self.sub_states(&targets)?;
        if states.iter().all(|s| s.terminal) {
            return Ok(SubWait::Finished(states));
        }
        let declared = self.session_manager().set_trigger_wait(
            session_id,
            WaitTrigger::Subagents,
            targets.clone(),
            Some(format!("subagents {}", targets.join(", "))),
        )?;
        if !declared {
            return Err(KanbanError::Permission(format!(
                "Session {session_id} is no longer active"
            )));
        }
        Ok(SubWait::Waiting(states))
    }

    /// One subagent became terminal (or its slot freed): start the next
    /// queued ones and wake a parent whose targets are now all terminal —
    /// right away, not on the next tick. Best effort.
    fn subagent_settled(&self, _task: &Task) {
        let _ = self.start_queued_subagents();
        let _ = self.wake_subagent_waits();
    }

    /// Wake every parent session in a `sub wait` whose targets are all
    /// terminal and whose process is gone. The parent relaunches directly —
    /// its family already held the slot — and the wake is not an
    /// auto-resume, so `auto_resumes` does not grow. Fenced on the task's
    /// current session, so concurrent callers wake it exactly once.
    pub fn wake_subagent_waits(&self) -> Result<Vec<WaitWake>> {
        let session_mgr = self.session_manager();
        let heartbeat_timeout = self.config.get_threshold("session_heartbeat_timeout")?;
        let now = timefmt::now();
        let mut woken = Vec::new();
        for session in session_mgr.list_active_sessions() {
            if session.wait_trigger != Some(WaitTrigger::Subagents) {
                continue;
            }
            let states = self.sub_states(&session.wait_for)?;
            if !states.iter().all(|s| s.terminal) {
                continue;
            }
            let process_gone =
                session.wait_exited || (now - session.last_seen).num_seconds() > heartbeat_timeout;
            if !process_gone {
                continue;
            }
            let Some(task) = self.storage.load_task(&session.task_id)? else {
                session_mgr.close_session(&session.id)?;
                continue;
            };
            let note = format!(
                "Subagents finished: {}. Read each with kanban sub output <id>, then merge it \
                 into your worktree with kanban sub merge <id> or drop it with kanban sub \
                 discard <id>, and continue.",
                states
                    .iter()
                    .map(|s| format!("{} {}", s.id, s.state))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            match self.respawn_session(&session.task_id, &session.id, &note, None)? {
                RespawnOutcome::Spawned(new_session) => {
                    session_mgr.close_session(&session.id)?;
                    woken.push(WaitWake::Resumed {
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
                                "Subagents finished, but relaunch failed; {new_session} was marked crashed. Re-run or recover the task manually."
                            ),
                        );
                    }
                }
                RespawnOutcome::Noop => {
                    let still_owned = self.storage.load_task(&session.task_id)?.is_some_and(|t| {
                        t.status == TaskStatus::InProgress
                            && t.session.as_deref() == Some(session.id.as_str())
                    });
                    if still_owned && self.auto_launch_enabled()? {
                        continue;
                    }
                    if still_owned {
                        session_mgr.crash_session(&session.id)?;
                        if let Ok(notifier) = self.notifier() {
                            notifier.stranded(
                                &task.id,
                                &task.title,
                                "Subagents finished, but auto-launch is off. Re-run the task.",
                            );
                        }
                    } else {
                        session_mgr.close_session(&session.id)?;
                    }
                }
            }
        }
        Ok(woken)
    }

    /// The subagent pump, run from every dispatcher tick: restart crashed
    /// subagents whose backoff has passed, fail stalled or orphaned ones
    /// (the watchdog that replaces a wait ETA), start queued ones, and wake
    /// parents whose subagents all finished.
    pub fn pump_subagents(&self) -> Result<Vec<String>> {
        let settings = self.config.get_orchestration()?.subagents;
        let session_mgr = self.session_manager();
        let now = timefmt::now();
        for task in self.list_subagents(None)? {
            if task.sub_state != Some(SubState::Running) {
                continue;
            }
            if let Some(at) = task.restart_at {
                if at <= now {
                    let _guard = self.storage.lock()?;
                    if let Some(mut current) = self.storage.load_task(&task.id)?
                        && current.sub_state == Some(SubState::Running)
                        && current.restart_at.is_some()
                    {
                        current.crash_restarts = current.crash_restarts.saturating_add(1);
                        current.restart_at = None;
                        current.sub_state = Some(SubState::Queued);
                        current.updated_at = now;
                        self.storage.save_task(&current)?;
                        self.post_queue_note(
                            &current.id,
                            &format!("↻ crash-restart {}", current.crash_restarts),
                        );
                    }
                }
                continue;
            }
            let session = task
                .session
                .as_deref()
                .and_then(|id| session_mgr.load_session(id));
            match session {
                Some(session) if session.status == SessionStatus::Active => {
                    if session.is_waiting() {
                        continue;
                    }
                    let progress = self.session_progress(&session.id).max(session.started_at);
                    let idle = (now - progress).num_seconds();
                    if idle > settings.stall_timeout {
                        self.fail_subagent(
                            &task.id,
                            &format!(
                                "stalled: no progress for {idle}s (stall_timeout {}s)",
                                settings.stall_timeout
                            ),
                        )?;
                    }
                }
                Some(session) => {
                    let ended = session.ended_at.unwrap_or(session.last_seen);
                    if (now - ended).num_seconds() > ORPHAN_GRACE_SECS {
                        self.fail_subagent(
                            &task.id,
                            &format!("session {} ended without sub done", session.id),
                        )?;
                    }
                }
                None => {
                    self.fail_subagent(&task.id, "no agent session")?;
                }
            }
        }
        let started = self.start_queued_subagents()?;
        self.wake_subagent_waits()?;
        Ok(started)
    }

    /// Latest progress of a session: the newest write to its transcript or
    /// log. Heartbeats do not count — the launch wrapper heartbeats for as
    /// long as the process lives, hung or not.
    fn session_progress(&self, session_id: &str) -> chrono::NaiveDateTime {
        ["transcript.jsonl", "log"]
            .iter()
            .filter_map(|ext| {
                fs::metadata(self.storage.logs_dir.join(format!("{session_id}.{ext}")))
                    .and_then(|meta| meta.modified())
                    .ok()
            })
            .map(|modified| chrono::DateTime::<chrono::Local>::from(modified).naive_local())
            .max()
            .unwrap_or(chrono::NaiveDateTime::MIN)
    }

    /// Crash restart of a subagent: the same backoff ladder as board tasks,
    /// but the restart re-enters the parent's queue, never the board queue.
    /// A spent (or disabled) ladder fails the subagent, which wakes the
    /// parent.
    pub(crate) fn schedule_subagent_restart(
        &self,
        task_id: &str,
        deadline: Option<chrono::NaiveDateTime>,
        cause: &str,
    ) -> Result<bool> {
        let orch = self.config.get_orchestration()?;
        let spent = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(false);
            };
            if task.sub_state != Some(SubState::Running) || task.restart_at.is_some() {
                return Ok(false);
            }
            let delays = &orch.auto_restart_delays_minutes;
            let idx = task.crash_restarts as usize;
            if !orch.auto_restart_enabled || idx >= delays.len() {
                true
            } else {
                let now = timefmt::now();
                let at = deadline
                    .filter(|at| *at > now)
                    .unwrap_or_else(|| now + chrono::Duration::minutes(delays[idx]));
                task.restart_at = Some(at);
                task.updated_at = now;
                self.storage.save_task(&task)?;
                self.post_queue_note(
                    task_id,
                    &format!(
                        "↻ subagent crash ({cause}); restart at {} (attempt {}/{})",
                        timefmt::format(&at),
                        idx + 1,
                        delays.len()
                    ),
                );
                false
            }
        };
        if spent {
            self.fail_subagent(task_id, &format!("{cause}; crash-restart budget spent"))?;
            return Ok(false);
        }
        Ok(true)
    }

    fn load_subagent(&self, task_id: &str) -> Result<Task> {
        let task = self
            .storage
            .load_task(task_id)?
            .ok_or_else(|| KanbanError::Invalid(format!("task {task_id} not found")))?;
        if !task.is_subagent() {
            return Err(KanbanError::Invalid(format!("{task_id} is not a subagent")));
        }
        Ok(task)
    }
}
