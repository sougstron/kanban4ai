//! Slot accounting and queue dispatch for the orchestration queue.
//!
//! The census counts every In Progress task whose session is `Live`. A
//! declared wait is a pause: the agent process is normally already gone, so
//! the pause releases its slot, and the task re-enters the queue when the
//! wait ends. Buckets follow the *resolved* launch settings (task override,
//! else the backend's configured default), so a task that inherits the
//! backend default is counted under the same key as one that names the model
//! explicitly.

use std::collections::HashMap;

use chrono::NaiveDateTime;

use crate::agent::{resolve_launch_settings, upcoming_run_plan};
use crate::core::config::{BoardConfig, OrchestrationSettings};
use crate::core::error::Result;
use crate::core::models::{Role, RunPhase, TaskStatus};
use crate::core::operations::{Operations, safe_session_component, sort_tasks};
use crate::core::session::{SessionManager, SessionState};
use crate::core::timefmt;

/// The role an agent runs under, derived from the task's run phase: a design
/// phase is the designer's work, a review phase the reviewer's, everything
/// else (including legacy boards that carry no phase at all) executes.
pub fn role_for_phase(phase: Option<RunPhase>) -> Role {
    Role::from_phase(phase)
}

/// Which cap refused one more running agent. Only the global total blocks the
/// head of the queue; every other cap merely skips its own candidate so a
/// full claude quota never holds back an opencode task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapBlock {
    Total,
    Backend,
    BackendModel,
    Role,
}

/// What the dispatcher does with one candidate after consulting the caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchDecision {
    Launch,
    /// Exhausted a per-backend / per-model / per-role cap: try the next task.
    Skip,
    /// The global total is full: stop walking the queue.
    Stop,
}

/// How the dispatcher treats a cap refusal. Only the global total is
/// head-of-line blocking.
pub fn dispatch_decision(block: Option<CapBlock>) -> DispatchDecision {
    match block {
        None => DispatchDecision::Launch,
        Some(CapBlock::Total) => DispatchDecision::Stop,
        Some(_) => DispatchDecision::Skip,
    }
}

#[derive(Debug, Default, Clone)]
pub struct Slots {
    pub total: usize,
    pub per_backend: HashMap<String, usize>,
    /// Keyed by the canonical `<backend>/<model>` string
    /// ([`OrchestrationSettings::backend_model_key`]).
    pub per_backend_model: HashMap<String, usize>,
    pub per_role: HashMap<String, usize>,
}

impl Slots {
    /// Measure the board's current agent occupancy.
    pub fn measure(ops: &Operations) -> Result<Self> {
        let heartbeat_timeout = ops.config.get_threshold("session_heartbeat_timeout")?;
        let states: HashMap<String, SessionState> = SessionManager::new(&ops.storage.project_path)
            .list_sessions_with_state(heartbeat_timeout)
            .into_iter()
            .map(|(session, state)| (session.id, state))
            .collect();
        let config = ops.config.load()?;
        let mut slots = Slots::default();
        for task in ops.storage.list_tasks(Some("in_progress"))? {
            // A queued task has no live session by design; it occupies nothing.
            if task.run_phase == Some(RunPhase::Queued) {
                continue;
            }
            let Some(session_id) = task.session.as_deref() else {
                continue;
            };
            let Some(state) = states.get(session_id) else {
                continue;
            };
            if !matches!(state, SessionState::Live) {
                continue;
            }
            let settings = resolve_launch_settings(&config, &task)?;
            slots.bump(
                &settings.backend,
                settings.model.as_deref(),
                role_for_phase(task.run_phase).as_str(),
            );
        }
        Ok(slots)
    }

    fn bump(&mut self, backend: &str, model: Option<&str>, role: &str) {
        self.total += 1;
        *self.per_backend.entry(backend.to_string()).or_default() += 1;
        // Only a resolved model gets a `<backend>/<model>` bucket. A run with
        // no model at all is not attributable to any pair a user could write
        // in `max_running_per_backend_model`, and `blocking_cap` skips the
        // model cap for exactly the same reason — counting it under a
        // synthetic `<backend>/-` key would build a bucket nothing ever reads.
        if let Some(model) = model {
            let key = OrchestrationSettings::backend_model_key(backend, model);
            *self.per_backend_model.entry(key).or_default() += 1;
        }
        *self.per_role.entry(role.to_string()).or_default() += 1;
    }

    /// The first cap that refuses one more agent with these resolved
    /// settings. A cap of `0` (or an absent entry) means unlimited.
    pub fn blocking_cap(
        &self,
        orch: &OrchestrationSettings,
        backend: &str,
        model: Option<&str>,
        role: &str,
    ) -> Option<CapBlock> {
        if orch.max_running_total > 0 && self.total >= orch.max_running_total as usize {
            return Some(CapBlock::Total);
        }
        if let Some(cap) = orch.max_running_per_backend.get(backend)
            && *cap > 0
            && self.per_backend.get(backend).copied().unwrap_or(0) >= *cap as usize
        {
            return Some(CapBlock::Backend);
        }
        if let Some(model) = model {
            let key = OrchestrationSettings::backend_model_key(backend, model);
            if let Some(cap) = orch.max_running_per_backend_model.get(&key)
                && *cap > 0
                && self.per_backend_model.get(&key).copied().unwrap_or(0) >= *cap as usize
            {
                return Some(CapBlock::BackendModel);
            }
        }
        if let Some(cap) = orch.max_running_per_role.get(role)
            && *cap > 0
            && self.per_role.get(role).copied().unwrap_or(0) >= *cap as usize
        {
            return Some(CapBlock::Role);
        }
        None
    }

    /// Whether launching one more agent with these resolved settings stays
    /// inside every applicable cap.
    pub fn has_room(
        &self,
        orch: &OrchestrationSettings,
        backend: &str,
        model: Option<&str>,
        role: &str,
    ) -> bool {
        self.blocking_cap(orch, backend, model, role).is_none()
    }
}

/// One task the dispatcher started.
#[derive(Debug, Clone)]
pub struct Dispatched {
    pub task_id: String,
    pub session_id: String,
    pub backend: String,
    pub role: &'static str,
}

/// Map the board's `tui.task_sort` value onto [`sort_tasks`] arguments — the
/// same mapping the board screen applies, so the queue drains in board order
/// and changing the sort changes the queue priority. Unknown and legacy
/// values degrade exactly like the board's own normalization.
fn task_sort_args(config: &BoardConfig) -> (&'static str, &'static str) {
    match config.tui.get("task_sort").and_then(|v| v.as_str()) {
        Some("updated_at_asc") => ("updated", "asc"),
        Some("updated_at_desc" | "completion_date") => ("updated", "desc"),
        Some("task_number_desc") => ("id", "desc"),
        _ => ("id", "asc"),
    }
}

/// Longest a crash may defer its own restart. Past this a provider's reported
/// reset time is more likely a bad clock or a bad payload than a real wait, so
/// the normal backoff ladder takes over.
const MAX_CRASH_RESTART_DELAY_HOURS: i64 = 24;

/// A crash-supplied restart deadline as an actual deadline: never sooner than
/// a minute out, never further than [`MAX_CRASH_RESTART_DELAY_HOURS`].
fn usable_deadline(deadline: NaiveDateTime, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let floor = now.checked_add_signed(chrono::Duration::minutes(1))?;
    let ceiling = now.checked_add_signed(chrono::Duration::hours(MAX_CRASH_RESTART_DELAY_HOURS))?;
    (deadline <= ceiling).then(|| deadline.max(floor))
}

impl Operations {
    /// Start queued In Progress tasks while the concurrency caps have room.
    ///
    /// Candidate order is the board's own sort (`tui.task_sort`); the global
    /// total is the only head-of-line-blocking cap, any other exhausted cap
    /// skips just its candidate. Each start claims its task under the board
    /// lock (re-read, still queued?, flip the phase) before launching outside
    /// the lock, so concurrent pumps — TUI tick plus daemon — can never
    /// double-start a task. Plain library call: no TUI or terminal
    /// assumptions, safe to loop over projects from a headless daemon.
    pub fn dispatch_queue(&self) -> Result<Vec<Dispatched>> {
        // Reap sessions whose process is gone before counting, exactly like
        // `kanban check-sessions`: otherwise a dead session would hold its
        // slot until the heartbeat timeout. Newly crashed sessions enter the
        // crash-restart backoff (or stay crashed when that budget is spent).
        let heartbeat_timeout = self.config.get_threshold("session_heartbeat_timeout")?;
        for session in self.session_manager().check_sessions(heartbeat_timeout)? {
            let _ = self.schedule_crash_restart(&session.task_id);
        }
        let _ = self.due_restarts()?;

        let orch = self.config.get_orchestration()?;
        if !orch.queue_enabled || !self.auto_launch_enabled()? {
            return Ok(Vec::new());
        }

        let config = self.config.load()?;
        let mut slots = Slots::measure(self)?;
        let now = timefmt::now();
        let mut candidates = self.storage.list_tasks(Some("in_progress"))?;
        candidates.retain(|task| {
            task.run_phase == Some(RunPhase::Queued) && task.restart_at.is_none_or(|at| at <= now)
        });
        let (by, order) = task_sort_args(&config);
        sort_tasks(&mut candidates, by, order);

        let mut dispatched = Vec::new();
        for candidate in candidates {
            let Ok((settings, next_phase)) = upcoming_run_plan(&config, &candidate) else {
                continue;
            };
            // Designer-enabled boards leave the queue as `design` and occupy
            // a designer slot; otherwise the claim flips straight to execute.
            let role = role_for_phase(Some(next_phase));
            match dispatch_decision(slots.blocking_cap(
                &orch,
                &settings.backend,
                settings.model.as_deref(),
                role.as_str(),
            )) {
                DispatchDecision::Stop => break,
                DispatchDecision::Skip => continue,
                DispatchDecision::Launch => {}
            }
            let Some(session_id) = self.claim_queued_task(
                &candidate.id,
                &settings.backend,
                settings.model.as_deref(),
                role.as_str(),
                next_phase,
            )?
            else {
                continue;
            };
            match self.finish_launch(
                &session_id,
                self.launch_agent(&candidate.id, &session_id, false),
            ) {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    // Launch failed: the session is crashed. Hand it to the
                    // crash-restart backoff so a subscription-limit crash
                    // retries through the same caps instead of hot-looping.
                    let _ = self.schedule_crash_restart(&candidate.id);
                    continue;
                }
            }
            slots.bump(&settings.backend, settings.model.as_deref(), role.as_str());
            dispatched.push(Dispatched {
                task_id: candidate.id.clone(),
                session_id,
                backend: settings.backend.clone(),
                role: role.as_str(),
            });
        }
        Ok(dispatched)
    }

    /// Flip a queued task to running under the board lock: re-read it, verify
    /// it is still In Progress with phase `queued` and no live session, mint
    /// the session id, and persist the claim — all inside one locked
    /// read-modify-write. Returns `None` when another pump won the race, the
    /// task went away, or a concurrent claim just filled the last slot. The
    /// launch itself happens outside the lock.
    fn claim_queued_task(
        &self,
        task_id: &str,
        backend: &str,
        model: Option<&str>,
        role: &str,
        phase: RunPhase,
    ) -> Result<Option<String>> {
        let session_mgr = self.session_manager();
        let new_session_id = {
            let _guard = self.storage.lock()?;
            let Some(mut task) = self.storage.load_task(task_id)? else {
                return Ok(None);
            };
            if task.status != TaskStatus::InProgress || task.run_phase != Some(RunPhase::Queued) {
                return Ok(None);
            }
            if task.restart_at.is_some_and(|at| at > timefmt::now()) {
                return Ok(None);
            }
            if task
                .session
                .as_deref()
                .is_some_and(|s| session_mgr.is_session_active(s))
            {
                return Ok(None);
            }
            // Concurrent pumps (TUI tick + daemon) serialize here: re-measure
            // so a claim that just filled the last slot is visible.
            let orch = self.config.get_orchestration()?;
            if Slots::measure(self)?
                .blocking_cap(&orch, backend, model, role)
                .is_some()
            {
                return Ok(None);
            }
            let new_session_id = self.fresh_session_id(&safe_session_component(backend));
            task.run_phase = Some(phase);
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
                    "▶ dispatcher started session {new_session_id} ({role}) — a slot freed up"
                ),
            );
            new_session_id
        };
        Ok(Some(new_session_id))
    }

    /// Whether the dispatcher could actually start a queued task: the queue
    /// and auto-launch both have to be on, since [`Self::dispatch_queue`] is
    /// what turns a queue entry back into a running agent. The crash-restart
    /// path uses this to avoid scheduling retries nothing would ever drain,
    /// and the TUI uses it to decide between queueing a run and launching
    /// directly (the fallback for boards where the queue is switched off).
    pub fn queue_can_dispatch(&self) -> Result<bool> {
        Ok(self.config.get_orchestration()?.queue_enabled && self.auto_launch_enabled()?)
    }

    /// Hand every In Progress task whose `restart_at` has passed back to the
    /// normal queue: increment `crash_restarts`, clear the deadline, keep
    /// phase `queued`. Dispatch (not this method) starts them, so a
    /// subscription-limit crash is gated by the same caps as any other run.
    pub fn due_restarts(&self) -> Result<Vec<String>> {
        let orch = self.config.get_orchestration()?;
        if !orch.auto_restart_enabled || !self.queue_can_dispatch()? {
            // Keep the deadline on the task instead of parking it in a queue
            // nothing drains; it restarts when the dispatcher is back on.
            return Ok(Vec::new());
        }
        let now = timefmt::now();
        let _guard = self.storage.lock()?;
        let mut due = Vec::new();
        for mut task in self.storage.list_tasks(Some("in_progress"))? {
            let Some(restart_at) = task.restart_at else {
                continue;
            };
            if restart_at > now {
                continue;
            }
            task.crash_restarts = task.crash_restarts.saturating_add(1);
            task.restart_at = None;
            task.run_phase = Some(RunPhase::Queued);
            task.updated_at = now;
            self.storage.save_task(&task)?;
            self.post_queue_note(
                &task.id,
                &format!(
                    "↻ crash-restart {} handed to the queue",
                    task.crash_restarts
                ),
            );
            due.push(task.id);
        }
        Ok(due)
    }

    /// After a session reaches `Crashed`, either schedule the next backoff
    /// (`restart_at = now + delays[crash_restarts]`, phase `queued`) or leave
    /// the task crashed and notify when that schedule is spent.
    pub(crate) fn schedule_crash_restart(&self, task_id: &str) -> Result<bool> {
        self.schedule_crash_restart_at(task_id, None)
    }

    /// [`Self::schedule_crash_restart`] with an explicit deadline: the moment
    /// the crash itself named, which is the provider's usage window rolling
    /// over. A blind backoff step would only 429 again before then, and one
    /// far shorter than the window would burn the whole retry budget doing it.
    /// The deadline still costs a budget step, so a backend that keeps failing
    /// cannot retry forever. It is floored at a minute out (a reset already
    /// past is clock skew, not an instant retry) and ignored beyond
    /// [`MAX_CRASH_RESTART_DELAY_HOURS`], where a nonsense timestamp would
    /// otherwise park the task for days.
    pub(crate) fn schedule_crash_restart_at(
        &self,
        task_id: &str,
        deadline: Option<NaiveDateTime>,
    ) -> Result<bool> {
        let orch = self.config.get_orchestration()?;
        // Crash restart runs *through* the queue, so promising a retry the
        // dispatcher can never honour would strand the task with a "↻ retry"
        // badge forever. With the queue (or auto-launch) off the task stays
        // crashed and recoverable, exactly as it did before this feature.
        if !orch.auto_restart_enabled || !self.queue_can_dispatch()? {
            return Ok(false);
        }
        let _guard = self.storage.lock()?;
        let Some(mut task) = self.storage.load_task(task_id)? else {
            return Ok(false);
        };
        if task.status != TaskStatus::InProgress {
            return Ok(false);
        }
        // Only skip when a backoff is already pending. A leftover `queued`
        // phase (session claimed outside the dispatcher, or a previous
        // restart already handed back to the queue) must still get a
        // deadline — otherwise `dispatch_queue` immediately relaunches and
        // a lone queued task hot-loops into retry.
        if task.restart_at.is_some() {
            return Ok(false);
        }
        let delays = &orch.auto_restart_delays_minutes;
        let idx = task.crash_restarts as usize;
        if idx >= delays.len() {
            if let Ok(notifier) = self.notifier() {
                notifier.stranded(
                    &task.id,
                    &task.title,
                    &format!(
                        "Agent crashed after {} restart attempt(s); crash-restart budget is spent. \
                         Re-run or recover the task manually.",
                        delays.len()
                    ),
                );
            }
            return Ok(false);
        }
        let minutes = delays[idx];
        let now = timefmt::now();
        let Some(backoff_at) = now.checked_add_signed(chrono::Duration::minutes(minutes)) else {
            return Ok(false);
        };
        let (restart_at, reason) = match deadline.and_then(|at| usable_deadline(at, now)) {
            Some(at) => (at, "provider quota resets then".to_string()),
            None => (backoff_at, format!("backoff {minutes} min")),
        };
        task.restart_at = Some(restart_at);
        task.run_phase = Some(RunPhase::Queued);
        task.updated_at = timefmt::now();
        self.storage.save_task(&task)?;
        self.post_queue_note(
            &task.id,
            &format!(
                "↻ crash restart scheduled at {} ({reason}, attempt {}/{})",
                timefmt::format(&restart_at),
                idx + 1,
                delays.len()
            ),
        );
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{BotSettings, OnChangesRequested, ReviewerSettings};

    /// A crash-supplied deadline is used as-is inside the sane range, floored
    /// to a minute out, and refused (falling back to the ladder) when it is so
    /// far out that only a bad clock or a bad payload could have produced it.
    #[test]
    fn crash_deadlines_are_floored_and_capped() {
        let now = timefmt::now();
        let reset = now + chrono::Duration::hours(2);
        assert_eq!(usable_deadline(reset, now), Some(reset));

        let past = now - chrono::Duration::minutes(5);
        assert_eq!(
            usable_deadline(past, now),
            Some(now + chrono::Duration::minutes(1))
        );

        let far = now + chrono::Duration::hours(MAX_CRASH_RESTART_DELAY_HOURS + 1);
        assert_eq!(usable_deadline(far, now), None);
    }

    fn orch(
        total: i64,
        backend: &[(&str, i64)],
        model: &[(&str, i64)],
        role: &[(&str, i64)],
    ) -> OrchestrationSettings {
        OrchestrationSettings {
            queue_enabled: true,
            max_running_total: total,
            max_running_per_backend: backend.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            max_running_per_backend_model: model.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            max_running_per_role: role.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            auto_restart_enabled: false,
            auto_restart_delays_minutes: Vec::new(),
            designer: BotSettings {
                enabled: false,
                backend: None,
                model: None,
                effort: None,
                agent: None,
            },
            reviewer: ReviewerSettings {
                enabled: false,
                backend: None,
                model: None,
                effort: None,
                agent: None,
                on_changes_requested: OnChangesRequested::InProgress,
                max_rounds: 3,
            },
            isolation: OrchestrationSettings::default().isolation,
        }
    }

    fn slots(
        total: usize,
        backend: &[(&str, usize)],
        model: &[(&str, usize)],
        role: &[(&str, usize)],
    ) -> Slots {
        Slots {
            total,
            per_backend: backend.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            per_backend_model: model.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            per_role: role.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    #[test]
    fn empty_slots_admit_everything() {
        let o = orch(
            3,
            &[("claude", 1)],
            &[("claude/opus", 1)],
            &[("executor", 1)],
        );
        let s = Slots::default();
        assert_eq!(s.blocking_cap(&o, "claude", Some("opus"), "executor"), None);
    }

    #[test]
    fn total_cap_blocks() {
        let o = orch(2, &[], &[], &[]);
        let s = slots(2, &[("claude", 1), ("opencode", 1)], &[], &[]);
        assert_eq!(
            s.blocking_cap(&o, "pi", Some("m"), "executor"),
            Some(CapBlock::Total)
        );
    }

    #[test]
    fn zero_cap_means_unlimited() {
        let o = orch(
            0,
            &[("claude", 0)],
            &[("claude/opus", 0)],
            &[("executor", 0)],
        );
        let s = slots(
            99,
            &[("claude", 50)],
            &[("claude/opus", 50)],
            &[("executor", 99)],
        );
        assert_eq!(s.blocking_cap(&o, "claude", Some("opus"), "executor"), None);
    }

    #[test]
    fn absent_cap_entry_means_unlimited() {
        let o = orch(0, &[], &[], &[]);
        let s = slots(
            5,
            &[("claude", 5)],
            &[("claude/opus", 5)],
            &[("executor", 5)],
        );
        assert_eq!(s.blocking_cap(&o, "claude", Some("opus"), "executor"), None);
        // …but a cap that exists is enforced even when the bucket is empty.
        let o = orch(10, &[("claude", 1)], &[], &[]);
        assert_eq!(
            s.blocking_cap(&o, "claude", None, "executor"),
            Some(CapBlock::Backend)
        );
    }

    #[test]
    fn backend_cap_only_blocks_that_backend() {
        let o = orch(10, &[("claude", 1)], &[], &[]);
        let s = slots(1, &[("claude", 1)], &[], &[]);
        assert_eq!(
            s.blocking_cap(&o, "claude", None, "executor"),
            Some(CapBlock::Backend)
        );
        assert_eq!(s.blocking_cap(&o, "opencode", None, "executor"), None);
    }

    #[test]
    fn model_cap_key_includes_the_backend() {
        // opencode/openai/gpt-5.5 carries a slash in the model id: the key
        // splits on the FIRST slash only.
        let o = orch(10, &[], &[("opencode/openai/gpt-5.5", 1)], &[]);
        let s = slots(
            1,
            &[("opencode", 1)],
            &[("opencode/openai/gpt-5.5", 1)],
            &[],
        );
        assert_eq!(
            s.blocking_cap(&o, "opencode", Some("openai/gpt-5.5"), "executor"),
            Some(CapBlock::BackendModel)
        );
        // Same bare model id under another backend is untouched…
        assert_eq!(
            s.blocking_cap(&o, "claude", Some("openai/gpt-5.5"), "executor"),
            None
        );
        // …and inheriting the default (no explicit model) is not capped.
        assert_eq!(s.blocking_cap(&o, "opencode", None, "executor"), None);
    }

    #[test]
    fn role_cap_only_blocks_that_role() {
        let o = orch(10, &[], &[], &[("designer", 1)]);
        let s = slots(1, &[], &[], &[("designer", 1)]);
        assert_eq!(
            s.blocking_cap(&o, "claude", None, "designer"),
            Some(CapBlock::Role)
        );
        assert_eq!(s.blocking_cap(&o, "claude", None, "executor"), None);
    }

    #[test]
    fn skip_vs_block_only_total_stops_the_queue() {
        assert_eq!(dispatch_decision(None), DispatchDecision::Launch);
        assert_eq!(
            dispatch_decision(Some(CapBlock::Total)),
            DispatchDecision::Stop
        );
        assert_eq!(
            dispatch_decision(Some(CapBlock::Backend)),
            DispatchDecision::Skip
        );
        assert_eq!(
            dispatch_decision(Some(CapBlock::BackendModel)),
            DispatchDecision::Skip
        );
        assert_eq!(
            dispatch_decision(Some(CapBlock::Role)),
            DispatchDecision::Skip
        );
    }

    #[test]
    fn role_follows_the_run_phase() {
        assert_eq!(role_for_phase(Some(RunPhase::Design)), Role::Designer);
        assert_eq!(role_for_phase(Some(RunPhase::Review)), Role::Reviewer);
        assert_eq!(role_for_phase(Some(RunPhase::Queued)), Role::Executor);
        assert_eq!(role_for_phase(Some(RunPhase::Execute)), Role::Executor);
        assert_eq!(role_for_phase(None), Role::Executor);
    }

    #[test]
    fn board_sort_maps_to_sort_tasks_arguments() {
        let mapping = |raw: &str| {
            let mut config = BoardConfig::default();
            config.tui.insert(
                serde_yaml_ng::Value::String("task_sort".to_string()),
                serde_yaml_ng::Value::String(raw.to_string()),
            );
            task_sort_args(&config)
        };
        assert_eq!(mapping("task_number"), ("id", "asc"));
        assert_eq!(mapping("task_number_desc"), ("id", "desc"));
        assert_eq!(mapping("updated_at_asc"), ("updated", "asc"));
        assert_eq!(mapping("updated_at_desc"), ("updated", "desc"));
        assert_eq!(mapping("completion_date"), ("updated", "desc"));
        assert_eq!(mapping("whatever"), ("id", "asc"));
        let empty = BoardConfig::default();
        assert_eq!(task_sort_args(&empty), ("id", "asc"));
    }
}
