//! Slot census and queue dispatch for the orchestration queue.

mod common;

use std::fs;
use std::thread;

use kanban4ai::agent::{resolve_launch_settings, upcoming_run_plan};
use kanban4ai::core::config::Config;
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::models::{RunPhase, SessionStatus, TaskStatus};
use kanban4ai::core::operations::{AgentExitOutcome, Operations};
use kanban4ai::core::scheduler::Slots;
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::storage::NewTask;
use kanban4ai::core::timefmt;

use common::{RecordingLauncher, ops_with_recorder};

/// A board whose config carries the given orchestration overrides.
fn ops_with_config(orchestration_yaml: &str) -> (tempfile::TempDir, Operations) {
    let dir = tempfile::tempdir().unwrap();
    kanban4ai::core::storage::Storage::new(dir.path())
        .init_board()
        .unwrap();
    fs::write(
        dir.path().join(".kanban/config.yaml"),
        format!(
            "notifications:\n  enabled: false\nauto_launch:\n  enabled: false\n{orchestration_yaml}"
        ),
    )
    .unwrap();
    let ops = Operations::new(dir.path());
    (dir, ops)
}

/// Board with auto-launch on (recording launcher) and the given orchestration
/// body (the contents of the `orchestration:` mapping).
fn dispatch_board(orch_body: &str) -> (tempfile::TempDir, Operations, RecordingLauncher) {
    let (dir, ops, recorder) = ops_with_recorder(true);
    fs::write(
        dir.path().join(".kanban/config.yaml"),
        format!(
            "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n{orch_body}"
        ),
    )
    .unwrap();
    (dir, ops, recorder)
}

/// An In Progress task with a live session on the given backend/model.
fn live_task(ops: &Operations, title: &str, backend: &str, model: Option<&str>) -> String {
    live_task_with_phase(ops, title, backend, model, None)
}

fn live_task_with_phase(
    ops: &Operations,
    title: &str,
    backend: &str,
    model: Option<&str>,
    phase: Option<RunPhase>,
) -> String {
    let session_mgr = SessionManager::new(&ops.storage.project_path);
    let task = ops.create_task(NewTask::titled(title)).unwrap();
    let session_id = format!("ses-{}", title.replace(' ', "-").to_lowercase());
    session_mgr.link_session(&task.id, &session_id).unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.status = TaskStatus::InProgress;
    current.session = Some(session_id.clone());
    current.agent_backend = Some(backend.to_string());
    current.ai_model = model.map(str::to_string);
    current.run_phase = phase;
    ops.storage.save_task(&current).unwrap();
    session_id
}

fn queued_task(ops: &Operations, title: &str, backend: &str) -> String {
    let task = ops.create_task(NewTask::titled(title)).unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.status = TaskStatus::InProgress;
    current.run_phase = Some(RunPhase::Queued);
    current.agent_backend = Some(backend.to_string());
    ops.storage.save_task(&current).unwrap();
    task.id
}

#[test]
fn census_counts_live_sessions_by_backend_and_model() {
    let (_dir, ops) = ops_with_config("");
    live_task(&ops, "Alpha", "claude", Some("opus"));
    live_task(&ops, "Beta", "claude", None); // resolves to the claude default
    // A closed session frees its slot even though the task is In Progress.
    let gamma_session = live_task(&ops, "Gamma", "opencode", None);
    let mut session = SessionManager::new(&ops.storage.project_path)
        .load_session(&gamma_session)
        .unwrap();
    session.status = SessionStatus::Closed;
    SessionManager::new(&ops.storage.project_path)
        .save_session(&session)
        .unwrap();

    let slots = Slots::measure(&ops).unwrap();
    assert_eq!(slots.total, 2);
    assert_eq!(slots.per_backend.get("claude"), Some(&2));
    assert!(!slots.per_backend.contains_key("opencode"));
    // Inherited default and an explicit override share the resolved key.
    assert_eq!(slots.per_backend_model.get("claude/opus"), Some(&1));
    assert_eq!(slots.per_backend_model.get("claude/sonnet"), Some(&1));
    assert_eq!(slots.per_role.get("executor"), Some(&2));
}

#[test]
fn waiting_session_still_occupies_a_slot() {
    let (_dir, ops) = ops_with_config("");
    let session_id = live_task(&ops, "Waiter", "claude", Some("opus"));
    let session_mgr = SessionManager::new(&ops.storage.project_path);
    session_mgr
        .set_wait(
            &session_id,
            timefmt::now() + chrono::Duration::hours(1),
            Some("compiling".into()),
        )
        .unwrap();

    let slots = Slots::measure(&ops).unwrap();
    assert_eq!(slots.total, 1, "a waiting agent still holds its slot");
    assert_eq!(slots.per_backend.get("claude"), Some(&1));
}

#[test]
fn design_phase_counts_as_the_designer_role() {
    let (_dir, ops) = ops_with_config("");
    live_task_with_phase(
        &ops,
        "Planner",
        "claude",
        Some("sonnet"),
        Some(RunPhase::Design),
    );
    live_task(&ops, "Worker", "opencode", None);

    let slots = Slots::measure(&ops).unwrap();
    assert_eq!(slots.per_role.get("designer"), Some(&1));
    assert_eq!(slots.per_role.get("executor"), Some(&1));
}

#[test]
fn queued_phase_tasks_hold_no_slot() {
    let (_dir, ops) = ops_with_config("");
    live_task(&ops, "Runner", "claude", None);
    // A queued In Progress task has no live session by design; give it a stale
    // one to prove the phase marker is what keeps it out of the census.
    let queued = ops.create_task(NewTask::titled("Queued")).unwrap();
    let mut current = ops.get_task(&queued.id).unwrap().unwrap();
    current.status = TaskStatus::InProgress;
    current.run_phase = Some(RunPhase::Queued);
    current.session = Some("ses-stale".to_string());
    current.agent_backend = Some("claude".to_string());
    ops.storage.save_task(&current).unwrap();

    let orch = ops.config.get_orchestration().unwrap();
    let slots = Slots::measure(&ops).unwrap();
    assert_eq!(slots.total, 1, "the queued task occupies nothing");
    assert!(
        slots.has_room(&orch, "claude", None, "executor"),
        "one live claude run stays under the default per-backend cap of 2"
    );
}

#[test]
fn has_room_honors_total_backend_and_model_caps() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path());
    let orch_default = config.get_orchestration().unwrap();

    // Unlimited caps admit everything (`0` means unlimited).
    let mut unlimited = orch_default.clone();
    unlimited.max_running_total = 0;
    unlimited.max_running_per_backend.clear();
    unlimited
        .max_running_per_backend_model
        .insert("claude/opus".to_string(), 0);
    assert!(Slots::default().has_room(&unlimited, "claude", Some("opus"), "executor"));

    // Total cap.
    let mut total_capped = orch_default.clone();
    total_capped.max_running_total = 3;
    total_capped.max_running_per_backend.clear();
    let full = Slots {
        total: 3,
        ..Default::default()
    };
    assert!(!full.has_room(&total_capped, "opencode", None, "executor"));
    assert!(
        Slots {
            total: 2,
            ..Default::default()
        }
        .has_room(&total_capped, "opencode", None, "executor")
    );

    // Per-backend cap: a blocked backend must not block another backend.
    let mut backend_capped = orch_default.clone();
    backend_capped.max_running_total = 0;
    backend_capped
        .max_running_per_backend
        .insert("claude".to_string(), 2);
    let busy_claude = Slots {
        per_backend: [("claude".to_string(), 2_usize)].into_iter().collect(),
        ..Default::default()
    };
    assert!(!busy_claude.has_room(&backend_capped, "claude", Some("opus"), "executor"));
    assert!(busy_claude.has_room(&backend_capped, "opencode", None, "executor"));

    // Per `<backend>/<model>` cap splits on the first slash only.
    let mut model_capped = orch_default.clone();
    model_capped.max_running_total = 0;
    model_capped
        .max_running_per_backend_model
        .insert("opencode/openai/gpt-5.5".to_string(), 1);
    let busy_model = Slots {
        per_backend_model: [("opencode/openai/gpt-5.5".to_string(), 1_usize)]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    assert!(!busy_model.has_room(
        &model_capped,
        "opencode",
        Some("openai/gpt-5.5"),
        "executor"
    ));
    assert!(busy_model.has_room(&model_capped, "opencode", Some("openai/other"), "executor"));
}

#[test]
fn dispatch_starts_a_queued_task_when_a_slot_is_free() {
    let (_dir, ops, recorder) = dispatch_board(
        "  queue_enabled: true\n  max_running_total: 3\n  max_running_per_backend: {}\n  max_running_per_role: {}\n",
    );
    let id = queued_task(&ops, "Ready", "claude");

    let started = ops.dispatch_queue().unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].task_id, id);
    assert_eq!(started[0].backend, "claude");
    assert_eq!(started[0].role, "executor");
    assert_eq!(
        recorder.calls(),
        vec![(id.clone(), started[0].session_id.clone(), false)]
    );

    let current = ops.get_task(&id).unwrap().unwrap();
    assert_eq!(current.run_phase, Some(RunPhase::Execute));
    assert_eq!(
        current.session.as_deref(),
        Some(started[0].session_id.as_str())
    );
}

#[test]
fn dispatch_is_a_noop_when_the_queue_is_disabled() {
    let (_dir, ops, recorder) = dispatch_board("  queue_enabled: false\n");
    queued_task(&ops, "Stuck", "claude");
    assert!(ops.dispatch_queue().unwrap().is_empty());
    assert!(recorder.calls().is_empty());
}

#[test]
fn full_backend_quota_skips_to_the_next_backend() {
    // Claude is full; the next candidate is opencode and must still start.
    let (_dir, ops, recorder) = dispatch_board(
        "  queue_enabled: true\n  max_running_total: 0\n  max_running_per_backend:\n    claude: 1\n    opencode: 1\n  max_running_per_role: {}\n",
    );
    live_task(&ops, "Busy Claude", "claude", Some("opus"));
    let blocked = queued_task(&ops, "More Claude", "claude");
    let other = queued_task(&ops, "Opencode Next", "opencode");

    let started = ops.dispatch_queue().unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].task_id, other);
    assert_eq!(started[0].backend, "opencode");
    assert_eq!(recorder.calls().len(), 1);
    assert_eq!(
        ops.get_task(&blocked).unwrap().unwrap().run_phase,
        Some(RunPhase::Queued),
        "a full claude quota must not launch another claude task"
    );
}

#[test]
fn global_total_is_head_of_line_blocking() {
    let (_dir, ops, recorder) = dispatch_board(
        "  queue_enabled: true\n  max_running_total: 1\n  max_running_per_backend: {}\n  max_running_per_role: {}\n",
    );
    live_task(&ops, "Occupant", "claude", None);
    queued_task(&ops, "First", "opencode");
    queued_task(&ops, "Second", "pi");

    assert!(
        ops.dispatch_queue().unwrap().is_empty(),
        "the global total stops the walk; later backends must not sneak through"
    );
    assert!(recorder.calls().is_empty());
}

#[test]
fn two_dispatch_calls_start_a_queued_task_exactly_once() {
    let (dir, ops, recorder) = dispatch_board(
        "  queue_enabled: true\n  max_running_total: 3\n  max_running_per_backend: {}\n  max_running_per_role: {}\n",
    );
    let id = queued_task(&ops, "Once", "claude");

    let first = ops.dispatch_queue().unwrap();
    let second = ops.dispatch_queue().unwrap();
    assert_eq!(first.len(), 1);
    assert!(second.is_empty());
    assert_eq!(recorder.calls().len(), 1);

    // Concurrent pumps over the same board (two Operations, two threads)
    // must also claim exactly once. Reset by enqueueing a fresh task.
    let other = queued_task(&ops, "Race", "opencode");
    let path = dir.path().to_path_buf();
    let rec_a = recorder.clone();
    let rec_b = recorder.clone();
    let path_a = path.clone();
    let path_b = path;
    thread::scope(|scope| {
        scope.spawn(move || {
            let ops = Operations::with_launcher(&path_a, Box::new(rec_a));
            ops.dispatch_queue().unwrap()
        });
        scope.spawn(move || {
            let ops = Operations::with_launcher(&path_b, Box::new(rec_b));
            ops.dispatch_queue().unwrap()
        });
    });
    let launches_of_other = recorder
        .calls()
        .into_iter()
        .filter(|(task_id, _, _)| task_id == &other)
        .count();
    assert_eq!(launches_of_other, 1, "the locked claim starts a task once");
    assert_eq!(
        ops.get_task(&id).unwrap().unwrap().run_phase,
        Some(RunPhase::Execute)
    );
}

#[test]
fn queued_task_starts_when_a_running_one_exits() {
    let (dir, ops, recorder) = dispatch_board(
        "  queue_enabled: true\n  max_running_total: 1\n  max_running_per_backend: {}\n  max_running_per_role: {}\n",
    );
    let running = ops.create_task(NewTask::titled("Running")).unwrap();
    let taken = ops
        .take_task(&running.id, "ses-running", true)
        .unwrap()
        .unwrap();
    assert_eq!(taken.run_phase, None);
    assert_eq!(recorder.calls().len(), 1);

    let queued = ops.create_task(NewTask::titled("Waiting in line")).unwrap();
    let parked = ops
        .take_task(&queued.id, "ses-queued", true)
        .unwrap()
        .unwrap();
    assert_eq!(parked.run_phase, Some(RunPhase::Queued));
    assert_eq!(recorder.calls().len(), 1, "no slot, so no second launch");

    // The agent finishes: task goes to Review, then the process exits. The
    // reconcile pump must start the queued successor.
    ContextManager::new(dir.path())
        .append_context(&running.id, "finished the work", "agent", &ops.storage)
        .unwrap();
    ops.complete_task(&running.id, "ses-running", true)
        .unwrap()
        .unwrap();
    ops.reconcile_agent_exit(&running.id, "ses-running", 0)
        .unwrap();

    let successor = ops.get_task(&queued.id).unwrap().unwrap();
    assert_eq!(successor.run_phase, Some(RunPhase::Execute));
    assert_eq!(recorder.calls().len(), 2);
    assert_eq!(recorder.calls()[1].0, queued.id);
}

fn restart_board(
    delays: &str,
    enabled: bool,
) -> (tempfile::TempDir, Operations, RecordingLauncher) {
    dispatch_board(&format!(
        "  queue_enabled: true\n  max_running_total: 0\n  auto_restart:\n    enabled: {enabled}\n    delays_minutes: {delays}\n"
    ))
}

fn take_running(ops: &Operations, title: &str, session_id: &str) -> String {
    let task = ops.create_task(NewTask::titled(title)).unwrap();
    ops.take_task(&task.id, session_id, true).unwrap().unwrap();
    task.id
}

fn force_restart_due(ops: &Operations, task_id: &str) {
    let mut task = ops.get_task(task_id).unwrap().unwrap();
    task.restart_at = Some(timefmt::now() - chrono::Duration::seconds(1));
    ops.storage.save_task(&task).unwrap();
}

#[test]
fn crash_schedules_restart_at_configured_minutes() {
    let (_dir, ops, _recorder) = restart_board("[1, 30, 270]", true);
    let id = take_running(&ops, "Backoff", "ses-crash-1");
    let before = timefmt::now();

    let outcome = ops.reconcile_agent_exit(&id, "ses-crash-1", 1).unwrap();

    assert_eq!(outcome, AgentExitOutcome::Crashed);
    let stored = ops.get_task(&id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.run_phase, Some(RunPhase::Queued));
    assert_eq!(stored.crash_restarts, 0);
    assert_eq!(
        stored.auto_resumes, 0,
        "crash restart must not share auto_resumes"
    );
    let restart_at = stored.restart_at.expect("restart_at must be scheduled");
    let expected = before + chrono::Duration::minutes(1);
    let slack = chrono::Duration::seconds(2);
    assert!(
        restart_at >= expected - slack && restart_at <= expected + slack,
        "restart_at {restart_at} should be ~1 minute after {before}"
    );
}

#[test]
fn due_restarts_uses_the_next_configured_delay_each_crash() {
    let (_dir, ops, recorder) = restart_board("[1, 30, 270]", true);
    let id = take_running(&ops, "Sequence", "ses-seq-0");
    recorder.calls.lock().unwrap().clear();

    let mut expected_minutes = [1_i64, 30, 270].into_iter();
    for attempt in 0..3 {
        let session = format!("ses-seq-{attempt}");
        if attempt > 0 {
            let mut task = ops.get_task(&id).unwrap().unwrap();
            task.session = Some(session.clone());
            task.run_phase = Some(RunPhase::Execute);
            ops.storage.save_task(&task).unwrap();
            SessionManager::new(&ops.storage.project_path)
                .link_session(&id, &session)
                .unwrap();
        }
        let before = timefmt::now();
        ops.reconcile_agent_exit(&id, &session, 1).unwrap();
        let stored = ops.get_task(&id).unwrap().unwrap();
        let minutes = expected_minutes.next().unwrap();
        let restart_at = stored.restart_at.expect("scheduled");
        let expected = before + chrono::Duration::minutes(minutes);
        let slack = chrono::Duration::seconds(2);
        assert!(
            restart_at >= expected - slack && restart_at <= expected + slack,
            "attempt {attempt}: restart_at {restart_at} should be ~{minutes} min after {before}"
        );
        assert_eq!(stored.crash_restarts, attempt);
        assert_eq!(stored.run_phase, Some(RunPhase::Queued));

        force_restart_due(&ops, &id);
        let due = ops.due_restarts().unwrap();
        assert_eq!(due, vec![id.clone()]);
        let handed = ops.get_task(&id).unwrap().unwrap();
        assert_eq!(handed.crash_restarts, attempt + 1);
        assert_eq!(handed.restart_at, None);
        assert_eq!(handed.run_phase, Some(RunPhase::Queued));

        let started = ops.dispatch_queue().unwrap();
        assert_eq!(
            started.len(),
            1,
            "attempt {attempt} should launch via the queue"
        );
        assert_eq!(started[0].task_id, id);
    }
    assert_eq!(recorder.calls().len(), 3);
}

#[test]
fn crash_restart_exhaustion_leaves_the_task_crashed() {
    let (dir, ops, recorder) = restart_board("[1, 30, 270]", true);
    let id = take_running(&ops, "Spent", "ses-ex-0");
    recorder.calls.lock().unwrap().clear();

    for attempt in 0..3 {
        let session = format!("ses-ex-{attempt}");
        if attempt > 0 {
            let mut task = ops.get_task(&id).unwrap().unwrap();
            task.session = Some(session.clone());
            task.run_phase = Some(RunPhase::Execute);
            ops.storage.save_task(&task).unwrap();
            SessionManager::new(dir.path())
                .link_session(&id, &session)
                .unwrap();
        }
        ops.reconcile_agent_exit(&id, &session, 1).unwrap();
        force_restart_due(&ops, &id);
        ops.due_restarts().unwrap();
        ops.dispatch_queue().unwrap();
    }

    let mut task = ops.get_task(&id).unwrap().unwrap();
    task.session = Some("ses-ex-3".into());
    task.run_phase = Some(RunPhase::Execute);
    ops.storage.save_task(&task).unwrap();
    SessionManager::new(dir.path())
        .link_session(&id, "ses-ex-3")
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    let outcome = ops.reconcile_agent_exit(&id, "ses-ex-3", 1).unwrap();
    assert_eq!(outcome, AgentExitOutcome::Crashed);
    assert!(
        recorder.calls().is_empty(),
        "exhausted budget must not relaunch"
    );
    let stored = ops.get_task(&id).unwrap().unwrap();
    assert_eq!(stored.restart_at, None);
    assert_ne!(stored.run_phase, Some(RunPhase::Queued));
    assert_eq!(stored.crash_restarts, 3);
    assert_eq!(stored.auto_resumes, 0);
    assert_eq!(
        SessionManager::new(dir.path()).session_state("ses-ex-3", 300),
        Some(kanban4ai::core::session::SessionState::Crashed)
    );
    assert!(ops.due_restarts().unwrap().is_empty());
}

#[test]
fn crash_restart_disabled_restores_current_behaviour() {
    let (dir, ops, recorder) = restart_board("[1, 30, 270]", false);
    let id = take_running(&ops, "Off", "ses-off");
    recorder.calls.lock().unwrap().clear();

    let outcome = ops.reconcile_agent_exit(&id, "ses-off", 1).unwrap();
    assert_eq!(outcome, AgentExitOutcome::Crashed);
    assert!(recorder.calls().is_empty());
    let stored = ops.get_task(&id).unwrap().unwrap();
    assert_eq!(stored.restart_at, None);
    assert_eq!(stored.crash_restarts, 0);
    assert_ne!(stored.run_phase, Some(RunPhase::Queued));
    assert_eq!(
        SessionManager::new(dir.path()).session_state("ses-off", 300),
        Some(kanban4ai::core::session::SessionState::Crashed)
    );
}

#[test]
fn dispatch_skips_a_queued_task_still_waiting_on_restart_at() {
    let (_dir, ops, recorder) = restart_board("[30]", true);
    let id = take_running(&ops, "Not yet", "ses-wait");
    ops.reconcile_agent_exit(&id, "ses-wait", 1).unwrap();
    recorder.calls.lock().unwrap().clear();

    assert!(ops.dispatch_queue().unwrap().is_empty());
    assert!(recorder.calls().is_empty());
    let stored = ops.get_task(&id).unwrap().unwrap();
    assert!(stored.restart_at.is_some());
    assert_eq!(stored.run_phase, Some(RunPhase::Queued));
}

#[test]
fn human_start_resets_crash_restart_bookkeeping() {
    let (_dir, ops, _recorder) = restart_board("[1, 30]", true);
    let id = take_running(&ops, "Reset me", "ses-reset");
    ops.reconcile_agent_exit(&id, "ses-reset", 1).unwrap();
    force_restart_due(&ops, &id);
    ops.due_restarts().unwrap();
    assert_eq!(ops.get_task(&id).unwrap().unwrap().crash_restarts, 1);

    ops.start_task(&id).unwrap();
    let stored = ops.get_task(&id).unwrap().unwrap();
    assert_eq!(stored.crash_restarts, 0);
    assert_eq!(stored.restart_at, None);
}

fn designer_board() -> (tempfile::TempDir, Operations, RecordingLauncher) {
    dispatch_board(
        "  queue_enabled: true\n  max_running_total: 0\n  max_running_per_backend: {}\n  max_running_per_role: {}\n  designer:\n    enabled: true\n    backend: claude\n    model: sonnet\n    effort: high\n",
    )
}

#[test]
fn dispatch_starts_designer_when_enabled() {
    let (_dir, ops, recorder) = designer_board();
    let id = queued_task(&ops, "Needs a plan", "opencode");
    let mut task = ops.get_task(&id).unwrap().unwrap();
    task.ai_model = Some("openai/gpt-5.5".to_string());
    task.ai_effort = Some("low".to_string());
    ops.storage.save_task(&task).unwrap();

    let started = ops.dispatch_queue().unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].task_id, id);
    assert_eq!(
        started[0].backend, "claude",
        "designer bot, not the task assignment"
    );
    assert_eq!(started[0].role, "designer");
    assert_eq!(recorder.calls().len(), 1);
    assert!(
        started[0].session_id.starts_with("ses-claude-"),
        "designer session prefix: {}",
        started[0].session_id
    );

    let current = ops.get_task(&id).unwrap().unwrap();
    assert_eq!(current.status, TaskStatus::InProgress);
    assert_eq!(current.run_phase, Some(RunPhase::Design));
    assert_eq!(current.agent_backend.as_deref(), Some("opencode"));
    assert_eq!(current.ai_model.as_deref(), Some("openai/gpt-5.5"));
    assert_eq!(current.ai_effort.as_deref(), Some("low"));

    let config = ops.config.load().unwrap();
    let settings = resolve_launch_settings(&config, &current).unwrap();
    assert_eq!(settings.backend, "claude");
    assert_eq!(settings.model.as_deref(), Some("sonnet"));
    assert_eq!(settings.effort.as_deref(), Some("high"));
}

#[test]
fn dispatch_starts_executor_when_designer_disabled() {
    let (_dir, ops, recorder) = dispatch_board(
        "  queue_enabled: true\n  max_running_total: 0\n  max_running_per_backend: {}\n  max_running_per_role: {}\n  designer:\n    enabled: false\n    backend: claude\n    model: sonnet\n",
    );
    let id = queued_task(&ops, "No designer", "opencode");

    let started = ops.dispatch_queue().unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].backend, "opencode");
    assert_eq!(started[0].role, "executor");
    assert_eq!(recorder.calls().len(), 1);
    assert_eq!(
        ops.get_task(&id).unwrap().unwrap().run_phase,
        Some(RunPhase::Execute)
    );
}

#[test]
fn designer_done_hands_the_slot_to_the_executor() {
    let (dir, ops, recorder) = designer_board();
    let id = queued_task(&ops, "Plan then run", "opencode");
    let mut task = ops.get_task(&id).unwrap().unwrap();
    task.ai_model = Some("openai/gpt-5.5".to_string());
    ops.storage.save_task(&task).unwrap();

    let started = ops.dispatch_queue().unwrap();
    let designer_session = started[0].session_id.clone();
    assert_eq!(started[0].role, "designer");

    ContextManager::new(dir.path())
        .append_context(
            &id,
            "1. inspect auth\n2. fix the bug",
            "agent",
            &ops.storage,
        )
        .unwrap();
    let after_done = ops
        .complete_task(&id, &designer_session, true)
        .unwrap()
        .unwrap();
    assert_eq!(after_done.status, TaskStatus::InProgress);
    assert_eq!(after_done.run_phase, Some(RunPhase::Execute));
    assert_eq!(after_done.agent_backend.as_deref(), Some("opencode"));
    assert_eq!(after_done.ai_model.as_deref(), Some("openai/gpt-5.5"));
    assert_ne!(
        after_done.session.as_deref(),
        Some(designer_session.as_str())
    );

    assert_eq!(
        recorder.calls().len(),
        2,
        "executor launched without re-queueing"
    );
    assert_eq!(recorder.calls()[1].0, id);
    let executor_session = &recorder.calls()[1].1;
    assert!(
        executor_session.starts_with("ses-opencode-"),
        "executor session prefix: {executor_session}"
    );
    assert_eq!(
        after_done.session.as_deref(),
        Some(executor_session.as_str())
    );
    assert!(!SessionManager::new(dir.path()).is_session_active(&designer_session));
    assert!(SessionManager::new(dir.path()).is_session_active(executor_session));

    // Still queued? No — the slot was handed over, not re-queued.
    assert_ne!(after_done.run_phase, Some(RunPhase::Queued));

    let config = ops.config.load().unwrap();
    let exec = resolve_launch_settings(&config, &after_done).unwrap();
    assert_eq!(exec.backend, "opencode");
    assert_eq!(exec.model.as_deref(), Some("openai/gpt-5.5"));
}

#[test]
fn designer_done_without_a_plan_is_refused() {
    let (_dir, ops, recorder) = designer_board();
    let id = queued_task(&ops, "Forgot the plan", "opencode");
    let started = ops.dispatch_queue().unwrap();
    let designer_session = &started[0].session_id;

    match ops.complete_task(&id, designer_session, true) {
        Err(kanban4ai::core::error::KanbanError::Permission(msg)) => {
            assert!(msg.contains("plan"), "{msg}");
        }
        other => panic!("expected permission error, got {other:?}"),
    }
    let current = ops.get_task(&id).unwrap().unwrap();
    assert_eq!(current.status, TaskStatus::InProgress);
    assert_eq!(current.run_phase, Some(RunPhase::Design));
    assert_eq!(
        recorder.calls().len(),
        1,
        "executor must not start without a plan"
    );
}

#[test]
fn designer_crash_does_not_fall_through_to_execute() {
    let (_dir, ops, recorder) = designer_board();
    let id = queued_task(&ops, "Designer boom", "opencode");
    ops.dispatch_queue().unwrap();
    let designer_session = recorder.calls()[0].1.clone();

    let outcome = ops.reconcile_agent_exit(&id, &designer_session, 1).unwrap();
    assert_eq!(outcome, AgentExitOutcome::Crashed);
    let current = ops.get_task(&id).unwrap().unwrap();
    assert_eq!(current.status, TaskStatus::InProgress);
    assert_ne!(current.run_phase, Some(RunPhase::Execute));
    assert_eq!(
        recorder.calls().len(),
        1,
        "crash must not launch the executor"
    );
}

#[test]
fn designer_stranded_exit_resumes_designer_not_executor() {
    let (_dir, ops, recorder) = designer_board();
    let id = queued_task(&ops, "Designer wandered off", "opencode");
    ops.dispatch_queue().unwrap();
    let designer_session = recorder.calls()[0].1.clone();

    let outcome = ops.reconcile_agent_exit(&id, &designer_session, 0).unwrap();
    match outcome {
        AgentExitOutcome::Resumed(session) => {
            assert!(session.starts_with("ses-claude-"), "{session}");
        }
        other => panic!("expected designer auto-resume, got {other:?}"),
    }
    let current = ops.get_task(&id).unwrap().unwrap();
    assert_eq!(current.run_phase, Some(RunPhase::Design));
    assert_eq!(current.agent_backend.as_deref(), Some("opencode"));
    assert_eq!(recorder.calls().len(), 2);
    assert!(recorder.calls()[1].1.starts_with("ses-claude-"));
}

#[test]
fn upcoming_run_plan_follows_the_designer_flag() {
    let (dir, ops, _recorder) = designer_board();
    let id = queued_task(&ops, "Plan me", "opencode");
    let task = ops.get_task(&id).unwrap().unwrap();
    let config = ops.config.load().unwrap();
    let (settings, phase) = upcoming_run_plan(&config, &task).unwrap();
    assert_eq!(phase, RunPhase::Design);
    assert_eq!(settings.backend, "claude");
    assert_eq!(settings.model.as_deref(), Some("sonnet"));

    // Flip only the designer flag: the same queued task should execute as itself.
    let raw = fs::read_to_string(dir.path().join(".kanban/config.yaml")).unwrap();
    fs::write(
        dir.path().join(".kanban/config.yaml"),
        raw.replace(
            "designer:\n    enabled: true",
            "designer:\n    enabled: false",
        ),
    )
    .unwrap();
    let config = ops.config.load_fresh().unwrap();
    let (settings, phase) = upcoming_run_plan(&config, &task).unwrap();
    assert_eq!(phase, RunPhase::Execute);
    assert_eq!(settings.backend, "opencode");
}

#[test]
fn upcoming_run_plan_honors_per_task_designer_when_project_designer_is_off() {
    let (_dir, ops) = ops_with_config("");
    let task = ops
        .create_task(NewTask {
            title: "Just this one".into(),
            agent_backend: Some("opencode".into()),
            use_designer: true,
            ..Default::default()
        })
        .unwrap();
    let config = ops.config.load().unwrap();
    assert!(!ops.config.get_orchestration().unwrap().designer.enabled);

    let (settings, phase) = upcoming_run_plan(&config, &task).unwrap();
    assert_eq!(phase, RunPhase::Design);
    assert_eq!(settings.backend, "claude");
    assert_eq!(settings.model.as_deref(), Some("sonnet"));

    let mut designed = task.clone();
    designed.designed = true;
    let (settings, phase) = upcoming_run_plan(&config, &designed).unwrap();
    assert_eq!(phase, RunPhase::Execute);
    assert_eq!(settings.backend, "opencode");
}

#[test]
fn resolve_launch_settings_uses_project_bots_for_a_per_task_phase() {
    let (_dir, ops) = ops_with_config("");
    let mut task = ops
        .create_task(NewTask {
            title: "Per-task bots".into(),
            agent_backend: Some("opencode".into()),
            use_designer: true,
            use_reviewer: true,
            ..Default::default()
        })
        .unwrap();
    let config = ops.config.load().unwrap();

    task.run_phase = Some(RunPhase::Design);
    let settings = resolve_launch_settings(&config, &task).unwrap();
    assert_eq!(settings.backend, "claude");
    assert_eq!(settings.model.as_deref(), Some("sonnet"));

    task.run_phase = Some(RunPhase::Review);
    let settings = resolve_launch_settings(&config, &task).unwrap();
    assert_eq!(settings.backend, "claude");
    assert_eq!(settings.model.as_deref(), Some("sonnet"));
}

// ---------------------------------------------- design-gate and census keys

#[test]
fn a_run_with_no_resolved_model_gets_no_backend_model_bucket() {
    let (_dir, ops) = ops_with_config("");
    // `omp` ships no default model, so this task resolves to model `None`.
    live_task(&ops, "Modelless", "omp", None);

    let slots = Slots::measure(&ops).unwrap();
    assert_eq!(slots.total, 1);
    assert_eq!(slots.per_backend.get("omp"), Some(&1));
    // `blocking_cap` skips the model cap entirely when the model is unknown,
    // so a synthetic `omp/-` bucket here would be counted and never enforced.
    assert!(
        slots.per_backend_model.is_empty(),
        "unattributable run must not build a bucket nothing reads: {:?}",
        slots.per_backend_model
    );
}

#[test]
fn a_designed_task_re_queues_to_the_executor_not_the_designer() {
    let (_dir, ops, _rec) = designer_board();
    let id = queued_task(&ops, "Already planned", "opencode");
    let mut task = ops.get_task(&id).unwrap().unwrap();
    task.designed = true;
    ops.storage.save_task(&task).unwrap();

    let config = ops.config.load().unwrap();
    let (settings, phase) = upcoming_run_plan(&config, &task).unwrap();
    assert_eq!(
        phase,
        RunPhase::Execute,
        "a task whose plan is on the thread must not be re-planned"
    );
    assert_eq!(settings.backend, "opencode", "the task's own bot");
}

#[test]
fn an_undesigned_task_still_starts_with_the_designer() {
    let (_dir, ops, _rec) = designer_board();
    let id = queued_task(&ops, "Needs planning", "opencode");
    let task = ops.get_task(&id).unwrap().unwrap();
    assert!(!task.designed);

    let config = ops.config.load().unwrap();
    let (settings, phase) = upcoming_run_plan(&config, &task).unwrap();
    assert_eq!(phase, RunPhase::Design);
    assert_eq!(settings.backend, "claude", "the designer bot");
}

#[test]
fn a_review_bounce_skips_the_designer_even_when_no_design_ran() {
    let (_dir, ops, _rec) = designer_board();
    // The designer was switched on after this task had already executed, so
    // `designed` is false while the reviewer has already bounced it once.
    let id = queued_task(&ops, "Bounced", "opencode");
    let mut task = ops.get_task(&id).unwrap().unwrap();
    task.review_rounds = 1;
    ops.storage.save_task(&task).unwrap();

    let config = ops.config.load().unwrap();
    let (settings, phase) = upcoming_run_plan(&config, &task).unwrap();
    assert_eq!(phase, RunPhase::Execute);
    assert_eq!(settings.backend, "opencode");
}

#[test]
fn a_crash_restart_after_design_resumes_the_executor() {
    let (dir, ops, recorder) = designer_board();
    let id = queued_task(&ops, "Plan then crash", "opencode");

    let started = ops.dispatch_queue().unwrap();
    let designer_session = started[0].session_id.clone();
    assert_eq!(started[0].role, "designer");
    ContextManager::new(dir.path())
        .append_context(&id, "1. the plan", "agent", &ops.storage)
        .unwrap();
    let after_design = ops
        .complete_task(&id, &designer_session, true)
        .unwrap()
        .unwrap();
    assert!(
        after_design.designed,
        "a finished design pass must record itself"
    );

    // Crash the executor and let the restart pump re-queue it.
    let executor_session = recorder.calls()[1].1.clone();
    ops.reconcile_agent_exit(&id, &executor_session, 1).unwrap();
    let crashed = ops.get_task(&id).unwrap().unwrap();
    assert!(crashed.designed, "the plan is still on the thread");

    let config = ops.config.load().unwrap();
    let (settings, phase) = upcoming_run_plan(&config, &crashed).unwrap();
    assert_eq!(
        phase,
        RunPhase::Execute,
        "a crash restart must not pay for a second designer pass"
    );
    assert_eq!(settings.backend, "opencode");
}
