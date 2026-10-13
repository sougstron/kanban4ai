//! Auto mode: unattended work on every project (`auto_mode:` in the store
//! config, see [`crate::core::global::AutoMode`]).
//!
//! Questions never wait for a human. With `questions: decide` an agent
//! question is refused and any question already open is answered with
//! [`AUTO_DECIDE_ANSWER`]; with `questions: arbiter` a detached arbiter agent
//! (`kanban arbiter <TASK-ID>`) reads the task context and answers, and the
//! decide answer is the fallback whenever no arbiter can start or it runs out
//! of time. A conflicted landing is sent straight back to the same model for
//! resolution, up to `max_resolver_runs` times per task.
//!
//! The merge-conflict queue hold ([`Operations::conflict_hold`]) is not an
//! auto-mode feature: it applies on every board.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::core::error::Result;
use crate::core::global::{AutoMode, AutoQuestions};
use crate::core::models::{
    IntegrationState, Message, MessageKind, MessageRole, RunMode, RunPhase, TaskStatus,
};
use crate::core::operations::{EXECUTOR_POOL_QUESTION_SOURCE, Operations, QuestionRef};
use crate::core::scheduler::Slots;
use crate::core::timefmt;

/// The answer auto mode gives when nobody (or no arbiter) decides.
pub const AUTO_DECIDE_ANSWER: &str = "Auto mode: no human is available to answer. Decide on your \
own best judgment from the task and the project conventions, keep working without asking again, \
and record the decision with kanban suggest.";

/// Extra time past `arbiter_timeout` before a sweep treats an arbiter run
/// as lost: the arbiter process enforces the timeout itself and needs a
/// moment to post its fallback answers.
const ARBITER_GRACE_SECS: u64 = 120;

impl Operations {
    /// The machine-wide auto-mode settings this board follows. A board used
    /// in place has no store config, so auto mode is always off there.
    pub fn auto_mode(&self) -> AutoMode {
        self.config.load_global().0.auto_mode()
    }

    /// Auto mode is on and agent questions are refused.
    pub(crate) fn auto_decides_questions(&self) -> bool {
        let mode = self.auto_mode();
        mode.enabled && mode.questions == AutoQuestions::Decide
    }

    /// Whether a conflicted landing re-dispatches the resolver: the board's
    /// `on_conflict: resolver`, or auto mode while the task still has
    /// resolver budget ([`Operations::dispatch_resolver`] counts the run).
    /// Notes an exhausted budget on the thread.
    pub(crate) fn resolver_wanted(
        &self,
        task: &crate::core::models::Task,
        configured: bool,
    ) -> bool {
        let mode = self.auto_mode();
        if configured {
            return true;
        }
        if !mode.enabled {
            return false;
        }
        if task.resolver_runs >= mode.max_resolver_runs {
            self.post_queue_note(
                &task.id,
                &format!(
                    "⚠ auto mode: {} resolver run(s) did not clear the conflict — left for a human",
                    task.resolver_runs
                ),
            );
            return false;
        }
        true
    }

    /// Open questions auto mode answers: agent questions only. Human-authored
    /// questions are the human's own notes, and the executor-pool quota
    /// question resolves itself when its deadline re-runs the pool walk.
    pub fn auto_answerable_questions(&self, task_id: &str) -> Result<Vec<Message>> {
        Ok(self
            .thread_manager()?
            .open_messages(task_id, Some(MessageKind::Question))?
            .into_iter()
            .filter(|m| {
                m.role == MessageRole::Agent
                    && m.author.as_deref() != Some(EXECUTOR_POOL_QUESTION_SOURCE)
            })
            .collect())
    }

    /// Answer every auto-answerable open question with `answer` (as the
    /// system, never as the human), then resume the run. Returns how many
    /// questions were answered.
    pub fn auto_answer_open_questions(&self, task_id: &str, answer: &str) -> Result<usize> {
        let questions = self.auto_answerable_questions(task_id)?;
        for question in &questions {
            self.answer_question_as(
                task_id,
                QuestionRef::MsgId(question.id.clone()),
                answer,
                MessageRole::System,
            )?;
        }
        if !questions.is_empty() {
            self.auto_resume_after_answers(task_id)?;
        }
        Ok(questions.len())
    }

    /// Answering resumes an In Progress task by itself
    /// ([`Self::answer_question`]); a task a pre-auto-mode question parked
    /// in Review is handed back to the queue here — as a resume, so a
    /// merge-conflict hold does not park it again.
    pub fn auto_resume_after_answers(&self, task_id: &str) -> Result<()> {
        let Some(task) = self.get_task(task_id)? else {
            return Ok(());
        };
        if task.status != TaskStatus::Review
            || task.integration == IntegrationState::Conflict
            || self.thread_manager()?.has_open_questions(task_id)?
            || !self.auto_launch_enabled()?
            || task
                .session
                .as_deref()
                .is_some_and(|s| self.session_manager().is_session_active(s))
        {
            return Ok(());
        }
        if self.queue_can_dispatch()? {
            self.queue_local_run_as(task_id, false)?;
        } else {
            self.rerun_review_task(task_id, None, RunMode::Immediate)?;
        }
        self.post_queue_note(
            task_id,
            "↻ auto mode: questions answered — resuming the run",
        );
        Ok(())
    }

    /// Route freshly asked questions in auto mode. Best effort: a failure
    /// becomes a thread note and the next sweep retries.
    pub(crate) fn auto_route_questions(&self, task_id: &str) {
        let mode = self.auto_mode();
        if !mode.enabled {
            return;
        }
        let result = match mode.questions {
            AutoQuestions::Decide => self
                .auto_answer_open_questions(task_id, AUTO_DECIDE_ANSWER)
                .map(|_| ()),
            AutoQuestions::Arbiter => self.ensure_arbiter(task_id, &mode),
        };
        if let Err(err) = result {
            self.post_queue_note(task_id, &format!("⚠ auto mode could not answer: {err}"));
        }
    }

    /// Marker of an arbiter run in flight for one task; its mtime is the
    /// start time.
    pub fn arbiter_marker(&self, task_id: &str) -> PathBuf {
        self.storage
            .kanban_dir
            .join("arbiter")
            .join(format!("{task_id}.run"))
    }

    /// Start the arbiter for a task's open questions unless one is already
    /// running. A lost run (past timeout + grace) and an arbiter that cannot
    /// start both fall back to the decide answer, so a question never stays
    /// open in auto mode.
    fn ensure_arbiter(&self, task_id: &str, mode: &AutoMode) -> Result<()> {
        if self.auto_answerable_questions(task_id)?.is_empty() {
            return Ok(());
        }
        let marker = self.arbiter_marker(task_id);
        if let Ok(started) = fs::metadata(&marker).and_then(|meta| meta.modified()) {
            let limit = Duration::from_secs(mode.arbiter_timeout_secs + ARBITER_GRACE_SECS);
            if started.elapsed().unwrap_or_default() < limit {
                return Ok(());
            }
            let _ = fs::remove_file(&marker);
            self.post_queue_note(
                task_id,
                "⚠ auto mode: the arbiter did not answer in time — deciding without it",
            );
            self.auto_answer_open_questions(task_id, AUTO_DECIDE_ANSWER)?;
            return Ok(());
        }
        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&marker, timefmt::now().to_string())?;
        match self.launcher().spawn_arbiter(self.roots(), task_id) {
            Ok(true) => {
                self.post_queue_note(
                    task_id,
                    "⚖ auto mode: arbiter started to answer the open question(s)",
                );
                Ok(())
            }
            outcome => {
                let _ = fs::remove_file(&marker);
                let reason = match outcome {
                    Err(err) => err.to_string(),
                    _ => "no arbiter launcher".to_string(),
                };
                self.post_queue_note(
                    task_id,
                    &format!(
                        "⚠ auto mode: arbiter could not start ({reason}) — deciding without it"
                    ),
                );
                self.auto_answer_open_questions(task_id, AUTO_DECIDE_ANSWER)?;
                Ok(())
            }
        }
    }

    /// One auto-mode pass over the board, run by every queue pump: answer
    /// (or hand to the arbiter) every open agent question — including ones
    /// asked before auto mode was switched on — and send each conflicted
    /// Review task back to its own model for resolution.
    pub fn auto_mode_sweep(&self) -> Result<()> {
        let mode = self.auto_mode();
        if !mode.enabled {
            return Ok(());
        }
        let session_mgr = self.session_manager();
        for task in self.storage.get_all_tasks()? {
            if !matches!(task.status, TaskStatus::InProgress | TaskStatus::Review) {
                continue;
            }
            if task.has_questions && !self.auto_answerable_questions(&task.id)?.is_empty() {
                self.auto_route_questions(&task.id);
                continue;
            }
            if task.status == TaskStatus::Review
                && task.integration == IntegrationState::Conflict
                && task.resolver_runs < mode.max_resolver_runs
                && !task
                    .session
                    .as_deref()
                    .is_some_and(|s| session_mgr.is_session_active(s))
                && self.auto_launch_enabled()?
            {
                self.post_queue_note(
                    &task.id,
                    "⚖ auto mode: merge conflict — re-running the same model to resolve it",
                );
                self.dispatch_resolver(&task.id);
            }
        }
        Ok(())
    }

    /// The merge-conflict queue hold, on every board: while any In Progress
    /// or Review task has an unresolved conflict, new work waits in the
    /// queue instead of starting — every parallel run would only conflict
    /// again. Runs already started keep going. Once a conflict was seen the
    /// board stays in conflict mode (a marker file, see
    /// [`Self::enter_conflict_mode`]) until the conflicts are gone *and*
    /// every running agent has finished (each could still land into a new
    /// conflict), even when nothing was queued while the conflict lasted.
    ///
    /// `Some(ids)` while holding: the conflicted task ids, empty when only
    /// running agents keep the hold on.
    pub fn conflict_hold(&self) -> Result<Option<Vec<String>>> {
        let mut conflicts = Vec::new();
        let mut held = false;
        for status in ["in_progress", "review"] {
            for task in self.storage.list_tasks(Some(status))? {
                if task.integration == IntegrationState::Conflict {
                    conflicts.push(task.id.clone());
                }
                held |= task.status == TaskStatus::InProgress
                    && task.run_phase == Some(RunPhase::Queued)
                    && task.conflict_held;
            }
        }
        if !conflicts.is_empty() {
            self.enter_conflict_mode();
            return Ok(Some(conflicts));
        }
        let marker = self.conflict_mode_marker();
        if (held || marker.exists()) && Slots::measure(self)?.total > 0 {
            return Ok(Some(Vec::new()));
        }
        // No conflicts and no live agent: conflict mode is over.
        let _ = fs::remove_file(marker);
        Ok(None)
    }

    /// Latch the board into conflict mode: recorded by the landing that hits
    /// a conflict, so the hold outlives a conflict resolved while other
    /// agents are still running. Best-effort — the conflicted task's own
    /// `integration: conflict` holds the queue regardless.
    pub(crate) fn enter_conflict_mode(&self) {
        let marker = self.conflict_mode_marker();
        if !marker.exists() {
            let _ = fs::write(marker, timefmt::now().to_string());
        }
    }

    fn conflict_mode_marker(&self) -> PathBuf {
        self.storage.kanban_dir.join("conflict_mode")
    }
}
