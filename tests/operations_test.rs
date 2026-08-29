//! Compatibility tests for agent rules, questions, review edits, and chaining.

mod common;

use common::ops_with_recorder;
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::error::KanbanError;
use kanban4ai::core::models::{
    MessageKind, MessageRole, MessageStatus, Role, RunMode, RunPhase, SessionStatus, Task,
    TaskStatus,
};
use kanban4ai::core::operations::{
    AgentExitOutcome, AgentLauncher, NoopLauncher, Operations, QuestionRef, TaskPatch, Verdict,
    WaitWake, sort_tasks,
};
use kanban4ai::core::project::{ProjectStore, Roots};
use kanban4ai::core::session::{SessionManager, SessionState};
use kanban4ai::core::storage::NewTask;
use kanban4ai::core::thread::ThreadManager;
use kanban4ai::core::timefmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[test]
fn targeted_creation_writes_directly_to_requested_status() {
    let (_dir, ops, _recorder) = ops_with_recorder(false);
    let task = ops
        .create_task_in_status(NewTask::titled("Direct review"), TaskStatus::Review)
        .unwrap();

    assert_eq!(task.status, TaskStatus::Review);
    assert!(
        ops.list_tasks(Some("todo"), None, "created", "asc")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ops.list_tasks(Some("review"), None, "created", "asc")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn exact_bulk_move_refuses_a_changed_source_set() {
    let (_dir, ops, _recorder) = ops_with_recorder(false);
    let first = ops.create_task(NewTask::titled("First")).unwrap();
    ops.move_task(&first.id, "in_progress", false).unwrap();
    let confirmed = vec![first.id.clone()];

    let second = ops.create_task(NewTask::titled("Second")).unwrap();
    ops.move_task(&second.id, "in_progress", false).unwrap();
    assert!(
        ops.bulk_move_exact(TaskStatus::InProgress, TaskStatus::Review, &confirmed)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        ops.list_tasks(Some("in_progress"), None, "created", "asc")
            .unwrap()
            .len(),
        2
    );

    let moved = ops
        .bulk_move_exact(
            TaskStatus::InProgress,
            TaskStatus::Review,
            &[first.id, second.id],
        )
        .unwrap()
        .unwrap();
    assert_eq!(moved.len(), 2);
}

#[test]
fn agent_take_moves_to_in_progress_and_links_session() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Delegate me")).unwrap();

    let taken = ops.take_task(&task.id, "ses-1", true).unwrap().unwrap();
    assert_eq!(taken.status, TaskStatus::InProgress);
    assert_eq!(taken.session.as_deref(), Some("ses-1"));
    let session_mgr = SessionManager::new(dir.path());
    assert!(session_mgr.is_session_active("ses-1"));
    assert_eq!(
        session_mgr.load_session("ses-1").unwrap().name,
        Some(task.title.clone())
    );
    // auto_launch_on_delegate fired the launcher
    assert_eq!(
        recorder.calls(),
        vec![(task.id, "ses-1".to_string(), false)]
    );
}

struct FailingLauncher;

impl AgentLauncher for FailingLauncher {
    fn launch(
        &self,
        _roots: Roots<'_>,
        _task: &Task,
        _session_id: &str,
        _revert: bool,
    ) -> kanban4ai::core::error::Result<bool> {
        Ok(false)
    }
}

struct MoveThenFailLauncher {
    project: PathBuf,
}

impl AgentLauncher for MoveThenFailLauncher {
    fn launch(
        &self,
        _roots: Roots<'_>,
        task: &Task,
        _session_id: &str,
        _revert: bool,
    ) -> kanban4ai::core::error::Result<bool> {
        Operations::with_launcher(&self.project, Box::new(NoopLauncher))
            .move_task(&task.id, "review", false)
            .unwrap();
        Ok(false)
    }
}

struct ErrLauncher;

impl AgentLauncher for ErrLauncher {
    fn launch(
        &self,
        _roots: Roots<'_>,
        _task: &Task,
        _session_id: &str,
        _revert: bool,
    ) -> kanban4ai::core::error::Result<bool> {
        Err(KanbanError::Invalid(
            "tmux new-session failed for ses-x (exit 1): open terminal failed: not a terminal"
                .to_string(),
        ))
    }
}

#[test]
fn start_task_surfaces_spawn_error_on_thread_and_status() {
    let (dir, _storage) = common::quiet_board(true);
    let ops = Operations::with_launcher(dir.path(), Box::new(ErrLauncher));
    let task = ops.create_task(NewTask::titled("Will not spawn")).unwrap();

    let err = ops.start_task(&task.id).unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("open terminal failed: not a terminal"),
        "{text}"
    );

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Todo);
    assert!(stored.session.is_some());

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread.messages.iter().any(|message| {
            message.kind == MessageKind::AgentStep
                && message.body.contains("✖ launch")
                && message.body.contains("open terminal failed")
        }),
        "launch error must be posted on the thread: {:?}",
        thread.messages
    );
}

#[test]
fn failed_agent_launch_rolls_back_take_assignment() {
    let (dir, _storage) = common::quiet_board(true);
    let ops = Operations::with_launcher(dir.path(), Box::new(FailingLauncher));
    let task = ops.create_task(NewTask::titled("Will fail")).unwrap();

    assert!(ops.take_task(&task.id, "ses-fail", true).unwrap().is_none());

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Todo);
    // The status rolls back; the crashed session stays as the last one tried.
    assert_eq!(stored.session.as_deref(), Some("ses-fail"));
    assert!(!SessionManager::new(dir.path()).is_session_active("ses-fail"));
}

#[test]
fn failed_review_rerun_rolls_back_to_review_without_live_session() {
    let (dir, _storage) = common::quiet_board(true);
    let ops = Operations::with_launcher(dir.path(), Box::new(FailingLauncher));
    let task = ops.create_task(NewTask::titled("Review retry")).unwrap();
    ops.move_task(&task.id, "review", false).unwrap();
    ops.set_review_edits(&task.id, "Fix the failed edge case")
        .unwrap();

    assert!(
        ops.rerun_review_task(&task.id, None, RunMode::Immediate)
            .unwrap()
            .is_none()
    );

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Review);
    assert!(stored.session.is_some());
    assert!(stored.review_edits.is_empty());
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|message| message.body == "Fix the failed edge case")
    );
    assert!(
        SessionManager::new(dir.path())
            .list_active_sessions()
            .is_empty()
    );
}

#[test]
fn one_task_per_instance_blocks_second_take() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let first = ops.create_task(NewTask::titled("First")).unwrap();
    let second = ops.create_task(NewTask::titled("Second")).unwrap();

    ops.take_task(&first.id, "ses-same", true).unwrap().unwrap();
    assert!(
        ops.take_task(&second.id, "ses-same", true)
            .unwrap()
            .is_none()
    );
    // a different session may take it
    assert!(
        ops.take_task(&second.id, "ses-other", true)
            .unwrap()
            .is_some()
    );
}

#[test]
fn take_task_rejects_unsafe_session_id_before_persisting() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Unsafe session")).unwrap();

    assert!(matches!(
        ops.take_task(&task.id, "../escape", true),
        Err(KanbanError::Invalid(_))
    ));

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.session, None);
    assert_eq!(stored.status, TaskStatus::Todo);
}

#[test]
fn review_rerun_rejects_unsafe_session_id_before_mutation() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = ops
        .create_task(NewTask::titled("Unsafe review rerun"))
        .unwrap();
    ops.move_task(&task.id, "review", false).unwrap();
    ops.set_review_edits(&task.id, "do not fold on invalid session")
        .unwrap();

    assert!(matches!(
        ops.rerun_review_task(&task.id, Some("../escape"), RunMode::Immediate),
        Err(KanbanError::Invalid(_))
    ));

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Review);
    assert_eq!(stored.session, None);
    assert_eq!(stored.review_edits, "do not fold on invalid session");
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        !thread
            .messages
            .iter()
            .any(|message| message.kind == MessageKind::ReviewEdit)
    );
}

#[test]
fn in_progress_rerun_rejects_unsafe_session_id_before_mutation() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = ops
        .create_task(NewTask::titled("Unsafe progress rerun"))
        .unwrap();
    ops.take_task(&task.id, "ses-old", true).unwrap().unwrap();
    SessionManager::new(dir.path())
        .crash_session("ses-old")
        .unwrap();

    assert!(matches!(
        ops.rerun_in_progress_task(&task.id, Some("../escape"), RunMode::Immediate),
        Err(KanbanError::Invalid(_))
    ));

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.session.as_deref(), Some("ses-old"));
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        !thread
            .messages
            .iter()
            .any(|message| message.body.contains("Task was re-run from In Progress"))
    );
}

#[test]
fn agent_exit_does_not_close_unrelated_session() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let owner = ops.create_task(NewTask::titled("Owner")).unwrap();
    let other = ops.create_task(NewTask::titled("Other")).unwrap();
    ops.take_task(&owner.id, "ses-owned", true)
        .unwrap()
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    let outcome = ops.reconcile_agent_exit(&other.id, "ses-owned", 0).unwrap();

    assert_eq!(outcome, AgentExitOutcome::Closed);
    assert!(SessionManager::new(dir.path()).is_session_active("ses-owned"));
    assert!(recorder.calls().is_empty());
}

#[test]
fn agent_cannot_move_to_done_or_from_review() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Guarded")).unwrap();

    match ops.move_task(&task.id, "done", true) {
        Err(KanbanError::Permission(msg)) => assert!(msg.contains("to Done")),
        other => panic!("expected permission error, got {other:?}"),
    }

    ops.move_task(&task.id, "review", false).unwrap();
    match ops.move_task(&task.id, "todo", true) {
        Err(KanbanError::Permission(msg)) => assert!(msg.contains("from Review")),
        other => panic!("expected permission error, got {other:?}"),
    }

    // human can do anything
    let done = ops.move_task(&task.id, "done", false).unwrap().unwrap();
    assert_eq!(done.status, TaskStatus::Done);
}

#[test]
fn designer_phase_agent_cannot_move() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Plan me")).unwrap();
    ops.take_task(&task.id, "ses-design", true).unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.run_phase = Some(RunPhase::Design);
    ops.storage.save_task(&current).unwrap();

    match ops.move_task(&task.id, "review", true) {
        Err(KanbanError::Permission(msg)) => {
            assert!(msg.contains("designer"), "{msg}");
            assert!(msg.contains("kanban done"), "{msg}");
        }
        other => panic!("expected designer move refusal, got {other:?}"),
    }
    match ops.move_task(&task.id, "todo", true) {
        Err(KanbanError::Permission(msg)) => assert!(msg.contains("designer"), "{msg}"),
        other => panic!("expected designer move refusal, got {other:?}"),
    }
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.run_phase, Some(RunPhase::Design));

    let human = ops.move_task(&task.id, "todo", false).unwrap().unwrap();
    assert_eq!(human.status, TaskStatus::Todo);
}

#[test]
fn reviewer_phase_agent_cannot_move() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Check me")).unwrap();
    ops.take_task(&task.id, "ses-review", true).unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.run_phase = Some(RunPhase::Review);
    ops.storage.save_task(&current).unwrap();

    match ops.move_task(&task.id, "done", true) {
        Err(KanbanError::Permission(msg)) => {
            assert!(msg.contains("reviewer"), "{msg}");
            assert!(msg.contains("kanban verdict"), "{msg}");
        }
        other => panic!("expected reviewer move refusal, got {other:?}"),
    }
    match ops.move_task(&task.id, "review", true) {
        Err(KanbanError::Permission(msg)) => assert!(msg.contains("verdict"), "{msg}"),
        other => panic!("expected reviewer move refusal, got {other:?}"),
    }
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.run_phase, Some(RunPhase::Review));

    let human = ops.move_task(&task.id, "review", false).unwrap().unwrap();
    assert_eq!(human.status, TaskStatus::Review);
}

#[test]
fn move_to_invalid_status_lists_valid_ones() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Mover")).unwrap();
    match ops.move_task(&task.id, "bogus", false) {
        Err(KanbanError::Invalid(msg)) => {
            assert!(msg.contains("Invalid status 'bogus'"));
            assert!(msg.contains("todo"));
            assert!(msg.contains("archive"));
        }
        other => panic!("expected invalid error, got {other:?}"),
    }
}

#[test]
fn recover_task_moves_to_todo_and_keeps_last_session() {
    let (_dir, ops, _recorder) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Recover me")).unwrap();
    ops.take_task(&task.id, "ses-stale", false)
        .unwrap()
        .unwrap();

    let recovered = ops.recover_task(&task.id).unwrap().unwrap();
    assert_eq!(recovered.status, TaskStatus::Todo);
    // The stale session is no longer running, but it stays on the task as the
    // record of the last session that worked it.
    assert_eq!(recovered.session.as_deref(), Some("ses-stale"));
    assert!(ops.recover_task("TASK-999").unwrap().is_none());
}

#[test]
fn agent_done_requires_context_then_moves_to_review() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Agent flow")).unwrap();
    ops.take_task(&task.id, "ses-flow", true).unwrap();

    match ops.complete_task(&task.id, "ses-flow", true) {
        Err(KanbanError::Permission(msg)) => assert!(msg.contains("without recording context")),
        other => panic!("expected permission error, got {other:?}"),
    }

    ContextManager::new(dir.path())
        .append_context(&task.id, "implemented and tested", "agent", &ops.storage)
        .unwrap();
    let reviewed = ops
        .complete_task(&task.id, "ses-flow", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.status, TaskStatus::Review);
    assert!(reviewed.completed_at.is_some());
    assert_eq!(reviewed.session.as_deref(), Some("ses-flow"));
    assert!(!SessionManager::new(dir.path()).is_session_active("ses-flow"));

    // a second agent done from Review is refused
    assert!(
        ops.complete_task(&task.id, "ses-flow", true)
            .unwrap()
            .is_none()
    );
}

fn write_verification_config(project: &Path, command: &str, block_on_failure: bool) {
    let mut config = fs::read_to_string(project.join(".kanban/config.yaml")).unwrap();
    config.push_str(&format!(
        "verification:\n  command: {command:?}\n  block_on_failure: {block_on_failure}\n"
    ));
    fs::write(project.join(".kanban/config.yaml"), config).unwrap();
}

#[test]
fn agent_done_with_passing_verification_gate_moves_to_review() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    write_verification_config(dir.path(), "true", true);
    ops.config.load_fresh().unwrap();

    let task = ops.create_task(NewTask::titled("Gate pass")).unwrap();
    ops.take_task(&task.id, "ses-gate", true).unwrap();
    ContextManager::new(dir.path())
        .append_context(&task.id, "implemented and tested", "agent", &ops.storage)
        .unwrap();

    let reviewed = ops
        .complete_task(&task.id, "ses-gate", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.status, TaskStatus::Review);

    let tm = ThreadManager::new(dir.path()).unwrap();
    let steps = tm
        .load(&reviewed.id)
        .unwrap()
        .messages
        .into_iter()
        .filter(|m| m.kind == MessageKind::AgentStep)
        .collect::<Vec<_>>();
    assert!(steps.iter().any(|m| m.body.contains("✓ gate passed")));
}

#[test]
fn agent_done_with_failing_verification_gate_stops_in_progress() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    write_verification_config(dir.path(), "echo 'bad output'; false", true);
    ops.config.load_fresh().unwrap();

    let task = ops.create_task(NewTask::titled("Gate fail")).unwrap();
    ops.take_task(&task.id, "ses-gate", true).unwrap();
    ContextManager::new(dir.path())
        .append_context(&task.id, "implemented and tested", "agent", &ops.storage)
        .unwrap();

    let stopped = ops
        .complete_task(&task.id, "ses-gate", true)
        .unwrap()
        .unwrap();
    assert_eq!(stopped.status, TaskStatus::InProgress);

    let tm = ThreadManager::new(dir.path()).unwrap();
    let steps = tm
        .load(&stopped.id)
        .unwrap()
        .messages
        .into_iter()
        .filter(|m| m.kind == MessageKind::AgentStep)
        .collect::<Vec<_>>();
    let failed = steps
        .iter()
        .find(|m| m.body.contains("✗ gate failed"))
        .expect("gate-failed agent_step should be posted");
    assert!(failed.body.contains("code=1"));
    assert!(failed.body.contains("bad output"));

    assert!(!SessionManager::new(dir.path()).is_session_active("ses-gate"));
}

#[test]
fn completion_sort_is_latest_first_and_keeps_unfinished_tasks_last() {
    let (_dir, ops, _recorder) = ops_with_recorder(false);
    let first = ops.create_task(NewTask::titled("First")).unwrap();
    let second = ops.create_task(NewTask::titled("Second")).unwrap();
    let third = ops.create_task(NewTask::titled("Third")).unwrap();

    let mut first = ops.get_task(&first.id).unwrap().unwrap();
    first.completed_at = Some(timefmt::parse("2026-07-17T10:00:00").unwrap());
    ops.storage.save_task(&first).unwrap();
    let mut second = ops.get_task(&second.id).unwrap().unwrap();
    second.completed_at = Some(timefmt::parse("2026-07-18T10:00:00").unwrap());
    ops.storage.save_task(&second).unwrap();

    let by_completion = ops.list_tasks(None, None, "completed", "desc").unwrap();
    assert_eq!(
        by_completion
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec![second.id.as_str(), first.id.as_str(), third.id.as_str()]
    );

    let by_number = ops.list_tasks(None, None, "id", "asc").unwrap();
    assert_eq!(
        by_number
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec![first.id.as_str(), second.id.as_str(), third.id.as_str()]
    );
}

#[test]
fn task_number_sort_is_numeric_past_the_zero_padding_boundary() {
    let mut tasks = vec![
        Task::new("TASK-1000", "Thousand"),
        Task::new("TASK-999", "Nine hundred ninety-nine"),
    ];

    sort_tasks(&mut tasks, "id", "asc");

    assert_eq!(tasks[0].id, "TASK-999");
    assert_eq!(tasks[1].id, "TASK-1000");
}

#[test]
fn updated_sort_supports_both_directions_with_stable_id_ties() {
    let mut oldest = Task::new("TASK-003", "Oldest");
    oldest.updated_at = timefmt::parse("2026-07-17T10:00:00").unwrap();
    let mut tied_first = Task::new("TASK-001", "First tied task");
    tied_first.updated_at = timefmt::parse("2026-07-18T10:00:00").unwrap();
    let mut tied_second = Task::new("TASK-002", "Second tied task");
    tied_second.updated_at = tied_first.updated_at;
    let mut tasks = vec![tied_second, oldest, tied_first];

    sort_tasks(&mut tasks, "updated", "asc");
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec!["TASK-003", "TASK-001", "TASK-002"]
    );

    sort_tasks(&mut tasks, "updated", "desc");
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec!["TASK-001", "TASK-002", "TASK-003"]
    );
}

#[test]
fn repeated_done_without_a_transition_preserves_completion_timestamp() {
    let (_dir, ops, _recorder) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Already done")).unwrap();
    ops.move_task(&task.id, "done", false).unwrap();

    let original_completion = timefmt::parse("2020-01-01T00:00:00").unwrap();
    let mut done = ops.get_task(&task.id).unwrap().unwrap();
    done.completed_at = Some(original_completion);
    ops.storage.save_task(&done).unwrap();

    let repeated = ops.move_task(&task.id, "done", false).unwrap().unwrap();
    assert_eq!(repeated.status, TaskStatus::Done);
    assert_eq!(repeated.completed_at, Some(original_completion));
}

#[test]
fn rerun_completion_replaces_the_previous_completion_timestamp() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Complete twice")).unwrap();
    assert!(task.completed_at.is_none());
    let todo_path = dir
        .path()
        .join(".kanban/tasks/todo")
        .join(format!("{}.md", task.id));
    assert!(
        !fs::read_to_string(todo_path)
            .unwrap()
            .contains("completed_at:")
    );

    ops.take_task(&task.id, "ses-first", true).unwrap();
    ContextManager::new(dir.path())
        .append_context(&task.id, "first pass", "agent", &ops.storage)
        .unwrap();
    let first_completion = ops
        .complete_task(&task.id, "ses-first", true)
        .unwrap()
        .unwrap();
    assert!(first_completion.completed_at.is_some());

    let rerun = ops
        .rerun_review_task(&task.id, Some("ses-rerun"), RunMode::Immediate)
        .unwrap()
        .unwrap();
    assert_eq!(rerun.completed_at, first_completion.completed_at);

    let old = timefmt::parse("2020-01-01T00:00:00").unwrap();
    let mut stored = ops.get_task(&task.id).unwrap().unwrap();
    stored.completed_at = Some(old);
    ops.storage.save_task(&stored).unwrap();
    let second_completion = ops
        .complete_task(&task.id, "ses-rerun", true)
        .unwrap()
        .unwrap();
    assert!(second_completion.completed_at.unwrap() > old);
    let review_path = dir
        .path()
        .join(".kanban/tasks/review")
        .join(format!("{}.md", task.id));
    let raw = fs::read_to_string(review_path).unwrap();
    assert!(raw.contains("completed_at: '"));
}

#[test]
fn human_done_completes_and_cleans_artifacts() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Cleanup")).unwrap();
    ops.take_task(&task.id, "ses-clean", true).unwrap();
    ContextManager::new(dir.path())
        .append_context(&task.id, "work done", "agent", &ops.storage)
        .unwrap();

    let done = ops
        .complete_task(&task.id, "ses-clean", false)
        .unwrap()
        .unwrap();
    assert_eq!(done.status, TaskStatus::Done);
    // The session's files go, but the task keeps naming the session that did
    // the work.
    assert_eq!(done.session.as_deref(), Some("ses-clean"));
    // context cleared with the move to done
    let context = ContextManager::new(dir.path())
        .get_context(&task.id, &ops.storage)
        .unwrap();
    assert!(context.is_empty());
    // session file removed
    assert!(!dir.path().join(".kanban/sessions/ses-clean.yaml").exists());
}

#[test]
fn ask_question_flags_task_and_answer_clears_it() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Q&A")).unwrap();

    let asked = ops
        .ask_question(&task.id, "JWT or cookies?", "agent", vec!["JWT".into()])
        .unwrap()
        .unwrap();
    assert!(asked.has_questions);
    // questions_go_to_review defaults to false: stays in todo
    assert_eq!(asked.status, TaskStatus::Todo);

    let open = ops.list_open_messages(&task.id).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].variants, vec!["JWT"]);

    let answered = ops
        .answer_question(&task.id, QuestionRef::Index(0), "JWT")
        .unwrap()
        .unwrap();
    assert!(!answered.task.has_questions);
    assert!(ops.list_open_messages(&task.id).unwrap().is_empty());
}

#[test]
fn ask_form_posts_one_question_per_entry_with_variants() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Form")).unwrap();

    let form = kanban4ai::core::ask_form::AskForm::parse(
        "questions:\n  - prompt: Which backend?\n    options: [OAuth2, API key]\n  - prompt: Any constraints?\n",
    )
    .unwrap();
    let (updated, count) = ops
        .ask_form(&task.id, &form, "agent", None)
        .unwrap()
        .unwrap();

    assert_eq!(count, 2);
    assert!(updated.has_questions);
    // questions_go_to_review defaults to false: stays in todo.
    assert_eq!(updated.status, TaskStatus::Todo);

    let open = ops.list_open_messages(&task.id).unwrap();
    assert_eq!(open.len(), 2);
    assert!(open.iter().all(|m| m.kind == MessageKind::Question));
    assert_eq!(open[0].variants, vec!["OAuth2", "API key"]);
    assert!(open[1].variants.is_empty());
}

#[test]
fn ask_form_missing_task_returns_none() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let form = kanban4ai::core::ask_form::AskForm::parse("questions:\n  - prompt: Hi?\n").unwrap();
    assert!(
        ops.ask_form("TASK-999", &form, "agent", None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn answer_by_msg_id_and_bad_index() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Refs")).unwrap();
    ops.ask_question(&task.id, "Which one?", "agent", vec![])
        .unwrap();
    let open = ops.list_open_messages(&task.id).unwrap();
    let msg_id = open[0].id.clone();

    assert!(
        ops.answer_question(&task.id, QuestionRef::Index(5), "nope")
            .unwrap()
            .is_none()
    );
    assert!(
        ops.answer_question(&task.id, QuestionRef::MsgId("MSG-999".into()), "nope")
            .unwrap()
            .is_none()
    );
    assert!(
        ops.answer_question(&task.id, QuestionRef::MsgId(msg_id), "this one")
            .unwrap()
            .is_some()
    );
}

#[test]
fn answer_refuses_a_msg_id_that_is_not_a_question() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Refs")).unwrap();
    ops.ask_question(&task.id, "Which one?", "agent", vec![])
        .unwrap();

    // MSG-002 is the task body message; answering it would stamp an answer
    // onto a non-question and leave the real question open.
    assert!(
        ops.answer_question(&task.id, QuestionRef::MsgId("MSG-002".into()), "bogus")
            .unwrap()
            .is_none()
    );
    let open = ops.list_open_messages(&task.id).unwrap();
    assert!(
        open.iter()
            .any(|message| message.kind == MessageKind::Question),
        "the real question stays open"
    );
    let task = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert!(task.has_questions, "the task still has an open question");
}

#[test]
fn answering_last_question_resumes_interactive_agent() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            title: "Interactive".into(),
            interactive: true,
            ..Default::default()
        })
        .unwrap();
    // put it in progress with no live session (agent exited after asking)
    ops.update_task(
        &task.id,
        TaskPatch {
            status: Some("in_progress".into()),
            ..Default::default()
        },
    )
    .unwrap();
    ops.ask_question(&task.id, "Blocking question?", "agent", vec![])
        .unwrap();
    assert!(recorder.calls().is_empty());

    let answered = ops
        .answer_question(&task.id, QuestionRef::Index(0), "Go ahead")
        .unwrap()
        .unwrap();

    assert_eq!(answered.remaining, 0);
    let calls = recorder.calls();
    assert_eq!(calls.len(), 1, "agent must be relaunched");
    assert_eq!(calls[0].0, task.id);
    assert!(calls[0].1.starts_with("ses-opencode-"));
    assert!(answered.task.session.is_some());
    assert_eq!(
        answered.resumed_session.as_deref(),
        Some(calls[0].1.as_str())
    );
    assert_eq!(
        SessionManager::new(_dir.path())
            .load_session(&calls[0].1)
            .unwrap()
            .name,
        Some(task.title)
    );
}

#[test]
fn answering_last_question_resumes_plain_task() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Plain asker")).unwrap();
    // put it in progress with no live session (agent exited after asking)
    ops.update_task(
        &task.id,
        TaskPatch {
            status: Some("in_progress".into()),
            ..Default::default()
        },
    )
    .unwrap();
    ops.ask_question(&task.id, "Blocking question?", "agent", vec![])
        .unwrap();
    assert!(recorder.calls().is_empty());

    let outcome = ops
        .answer_question(&task.id, QuestionRef::Index(0), "Go ahead")
        .unwrap()
        .unwrap();

    assert_eq!(outcome.remaining, 0);
    let calls = recorder.calls();
    assert_eq!(
        calls.len(),
        1,
        "a plain task must resume after its last answer"
    );
    assert_eq!(calls[0].0, task.id);
    assert_eq!(
        outcome.resumed_session.as_deref(),
        Some(calls[0].1.as_str())
    );
    assert_eq!(outcome.task.session.as_deref(), Some(calls[0].1.as_str()));
}

#[test]
fn answering_a_question_with_others_open_does_not_resume() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Two questions")).unwrap();
    ops.update_task(
        &task.id,
        TaskPatch {
            status: Some("in_progress".into()),
            ..Default::default()
        },
    )
    .unwrap();
    ops.ask_question(&task.id, "First?", "agent", vec![])
        .unwrap();
    ops.ask_question(&task.id, "Second?", "agent", vec![])
        .unwrap();

    let first = ops
        .answer_question(&task.id, QuestionRef::Index(0), "One")
        .unwrap()
        .unwrap();
    assert_eq!(first.remaining, 1);
    assert!(first.resumed_session.is_none());
    assert!(recorder.calls().is_empty());

    let last = ops
        .answer_question(&task.id, QuestionRef::Index(0), "Two")
        .unwrap()
        .unwrap();
    assert_eq!(last.remaining, 0);
    let calls = recorder.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(last.resumed_session.as_deref(), Some(calls[0].1.as_str()));
}

#[test]
fn stale_session_is_replaced_after_the_last_answer() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Stale asker")).unwrap();
    ops.take_task(&task.id, "ses-answer-stale", true)
        .unwrap()
        .unwrap();
    ops.ask_question(&task.id, "Anyone there?", "agent", vec![])
        .unwrap();
    let sessions = SessionManager::new(dir.path());
    let mut record = sessions.load_session("ses-answer-stale").unwrap();
    record.last_seen = timefmt::now() - chrono::Duration::seconds(3600);
    sessions.save_session(&record).unwrap();
    recorder.calls.lock().unwrap().clear();

    let outcome = ops
        .answer_question(&task.id, QuestionRef::Index(0), "Here")
        .unwrap()
        .unwrap();

    assert_eq!(outcome.remaining, 0);
    assert_eq!(
        recorder.calls().len(),
        1,
        "a stale session must not block the resume"
    );
    assert_ne!(outcome.task.session.as_deref(), Some("ses-answer-stale"));
    assert!(!sessions.is_session_active("ses-answer-stale"));
}

#[test]
fn resume_after_answer_can_be_disabled() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let config = "columns:\n- name: To Do\n  id: todo\n- name: In Progress\n  id: in_progress\n- name: Review\n  id: review\n- name: Done\n  id: done\nnotifications:\n  enabled: false\nauto_launch:\n  enabled: true\nrules:\n  resume_after_last_answer: false\n";
    fs::write(dir.path().join(".kanban/config.yaml"), config).unwrap();
    let task = ops.create_task(NewTask::titled("Manual resume")).unwrap();
    ops.update_task(
        &task.id,
        TaskPatch {
            status: Some("in_progress".into()),
            ..Default::default()
        },
    )
    .unwrap();
    ops.ask_question(&task.id, "Anyone?", "agent", vec![])
        .unwrap();

    let outcome = ops
        .answer_question(&task.id, QuestionRef::Index(0), "Yes")
        .unwrap()
        .unwrap();

    assert_eq!(outcome.remaining, 0);
    assert!(outcome.resumed_session.is_none());
    assert!(recorder.calls().is_empty());
    assert_eq!(outcome.task.status, TaskStatus::InProgress);
}

#[test]
fn answering_last_question_revokes_future_declared_wait() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            title: "Waiting for an answer".into(),
            interactive: true,
            ..Default::default()
        })
        .unwrap();
    ops.take_task(&task.id, "ses-answer-wait", true)
        .unwrap()
        .unwrap();
    ops.ask_question(&task.id, "Continue now?", "agent", vec![])
        .unwrap();
    ops.declare_waiting(&task.id, "ses-answer-wait", Some(60), Some("later result"))
        .unwrap();
    assert_eq!(
        ops.reconcile_agent_exit(&task.id, "ses-answer-wait", 0)
            .unwrap(),
        AgentExitOutcome::Waiting
    );
    recorder.calls.lock().unwrap().clear();

    let answered = ops
        .answer_question(&task.id, QuestionRef::Index(0), "Continue")
        .unwrap()
        .unwrap();

    assert!(
        answered.queued,
        "answering a paused task must queue the wake, not launch past the caps"
    );
    assert!(answered.resumed_session.is_none());
    assert_eq!(answered.task.run_phase, Some(RunPhase::Queued));
    let sessions = SessionManager::new(dir.path());
    assert!(!sessions.is_session_active("ses-answer-wait"));
    // The wake pumps the queue, and an idle board starts the run on the spot.
    assert_eq!(recorder.calls().len(), 1);
    assert_eq!(recorder.calls()[0].0, task.id);
    let started = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(
        started.session.as_deref(),
        Some(recorder.calls()[0].1.as_str())
    );
    assert!(sessions.is_session_active(recorder.calls()[0].1.as_str()));
}

#[test]
fn answering_last_question_expires_wait_while_background_wrapper_exits() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            title: "Background wait exit".into(),
            interactive: true,
            ..Default::default()
        })
        .unwrap();
    ops.take_task(&task.id, "ses-answer-exiting", true)
        .unwrap()
        .unwrap();
    ops.ask_question(&task.id, "Wake after exit?", "agent", vec![])
        .unwrap();
    ops.declare_waiting(
        &task.id,
        "ses-answer-exiting",
        Some(60),
        Some("original timer"),
    )
    .unwrap();
    recorder.calls.lock().unwrap().clear();

    let answered = ops
        .answer_question(&task.id, QuestionRef::Index(0), "Wake")
        .unwrap()
        .unwrap();

    assert_eq!(answered.task.session.as_deref(), Some("ses-answer-exiting"));
    let manager = SessionManager::new(dir.path());
    assert!(
        manager
            .load_session("ses-answer-exiting")
            .unwrap()
            .wait_until
            .is_some_and(|deadline| deadline < timefmt::now())
    );
    assert!(recorder.calls().is_empty());

    let outcome = ops
        .reconcile_agent_exit(&task.id, "ses-answer-exiting", 0)
        .unwrap();
    assert!(matches!(outcome, AgentExitOutcome::Resumed(_)));
    assert_eq!(recorder.calls().len(), 1);
}

#[test]
fn answering_question_leaves_live_polling_session_in_place() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            title: "Polling for answer".into(),
            interactive: true,
            ..Default::default()
        })
        .unwrap();
    ops.take_task(&task.id, "ses-answer-live", true)
        .unwrap()
        .unwrap();
    ops.ask_question(&task.id, "Ready?", "agent", vec![])
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    let answered = ops
        .answer_question(&task.id, QuestionRef::Index(0), "Ready")
        .unwrap()
        .unwrap();

    assert_eq!(answered.task.session.as_deref(), Some("ses-answer-live"));
    assert_eq!(answered.remaining, 0);
    assert!(recorder.calls().is_empty());
}

#[test]
fn failed_interactive_resume_preserves_concurrent_status_change() {
    let (dir, _storage) = common::quiet_board(true);
    let ops = Operations::with_launcher(
        dir.path(),
        Box::new(MoveThenFailLauncher {
            project: dir.path().to_path_buf(),
        }),
    );
    let task = ops
        .create_task(NewTask {
            title: "Interactive race".into(),
            interactive: true,
            ..Default::default()
        })
        .unwrap();
    ops.update_task(
        &task.id,
        TaskPatch {
            status: Some("in_progress".into()),
            ..Default::default()
        },
    )
    .unwrap();
    ops.ask_question(&task.id, "Continue?", "agent", vec![])
        .unwrap();

    ops.answer_question(&task.id, QuestionRef::Index(0), "Yes")
        .unwrap();

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Review);
    assert!(stored.session.is_some());
    assert!(
        SessionManager::new(dir.path())
            .list_active_sessions()
            .is_empty(),
        "the resume session crashed and must not be left running"
    );
}

#[test]
fn ask_and_wait_times_out_with_system_answer() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Waiter")).unwrap();

    let message = ops
        .ask_and_wait(&task.id, "Anyone there?", None, vec![], Some(0), Some(0))
        .unwrap()
        .unwrap();
    assert_eq!(message.status, MessageStatus::Answered);
    assert_eq!(
        message.answer.as_deref(),
        Some("(timeout - no answer received)")
    );
    assert_eq!(message.answered_by_role, Some(MessageRole::System));

    let task = ops.get_task(&task.id).unwrap().unwrap();
    assert!(!task.has_questions);
}

#[test]
fn chained_task_launches_when_target_enters_review() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let target = ops.create_task(NewTask::titled("Target")).unwrap();
    let chained = ops
        .create_task(NewTask {
            title: "Chained".into(),
            chained_to: Some(target.id.clone()),
            ..Default::default()
        })
        .unwrap();

    ops.take_task(&target.id, "ses-target", true).unwrap();
    ContextManager::new(dir.path())
        .append_context(&target.id, "done", "agent", &ops.storage)
        .unwrap();
    ops.complete_task(&target.id, "ses-target", true).unwrap();

    let launched: Vec<_> = recorder
        .calls()
        .into_iter()
        .filter(|(id, _, _)| id == &chained.id)
        .collect();
    assert_eq!(launched.len(), 1, "chained task must auto-launch");

    let chained_now = ops.get_task(&chained.id).unwrap().unwrap();
    assert_eq!(chained_now.status, TaskStatus::InProgress);
    assert!(chained_now.session.is_some());
    assert_eq!(
        SessionManager::new(dir.path())
            .load_session(chained_now.session.as_deref().unwrap())
            .unwrap()
            .name,
        Some(chained.title)
    );
}

#[test]
fn chained_task_not_launched_when_not_todo_or_launch_disabled() {
    // already in progress → skipped
    let (dir, ops, recorder) = ops_with_recorder(true);
    let target = ops.create_task(NewTask::titled("Target")).unwrap();
    let chained = ops
        .create_task(NewTask {
            title: "Busy chained".into(),
            chained_to: Some(target.id.clone()),
            ..Default::default()
        })
        .unwrap();
    ops.update_task(
        &chained.id,
        TaskPatch {
            status: Some("in_progress".into()),
            ..Default::default()
        },
    )
    .unwrap();
    ops.take_task(&target.id, "ses-t", true).unwrap();
    ContextManager::new(dir.path())
        .append_context(&target.id, "ok", "agent", &ops.storage)
        .unwrap();
    ops.complete_task(&target.id, "ses-t", true).unwrap();
    assert!(
        !recorder.calls().iter().any(|(id, _, _)| id == &chained.id),
        "non-todo chained task must not launch"
    );

    // auto_launch disabled → nothing launches at all
    let (dir2, ops2, recorder2) = ops_with_recorder(false);
    let target2 = ops2.create_task(NewTask::titled("Target2")).unwrap();
    ops2.create_task(NewTask {
        title: "Chained2".into(),
        chained_to: Some(target2.id.clone()),
        ..Default::default()
    })
    .unwrap();
    ops2.take_task(&target2.id, "ses-t2", true).unwrap();
    ContextManager::new(dir2.path())
        .append_context(&target2.id, "ok", "agent", &ops2.storage)
        .unwrap();
    ops2.complete_task(&target2.id, "ses-t2", true).unwrap();
    assert!(recorder2.calls().is_empty());
}

#[test]
fn review_edits_fold_into_thread_on_rerun() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Review me")).unwrap();
    ops.move_task(&task.id, "review", false).unwrap();

    ops.set_review_edits(&task.id, "Please handle expired tokens too")
        .unwrap()
        .unwrap();
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.review_edits, "Please handle expired tokens too");

    let rerun = ops
        .rerun_review_task(&task.id, None, RunMode::Immediate)
        .unwrap()
        .unwrap();
    assert_eq!(rerun.status, TaskStatus::InProgress);
    assert_eq!(rerun.review_edits, "");
    assert!(rerun.session.is_some());
    assert_eq!(
        SessionManager::new(dir.path())
            .load_session(rerun.session.as_deref().unwrap())
            .unwrap()
            .name,
        Some(task.title.clone())
    );
    assert_eq!(recorder.calls().len(), 1);

    let tm = ThreadManager::new(dir.path()).unwrap();
    let edits = tm
        .messages_of_kind(&task.id, MessageKind::ReviewEdit)
        .unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].body, "Please handle expired tokens too");
}

#[test]
fn rerun_in_progress_restarts_stalled_session() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Stalled")).unwrap();
    ops.take_task(&task.id, "ses-dead", true).unwrap();
    // the session dies
    SessionManager::new(dir.path())
        .crash_session("ses-dead")
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    let rerun = ops
        .rerun_in_progress_task(&task.id, None, RunMode::Immediate)
        .unwrap()
        .unwrap();
    assert_ne!(rerun.session.as_deref(), Some("ses-dead"));
    assert_eq!(
        SessionManager::new(dir.path())
            .load_session(rerun.session.as_deref().unwrap())
            .unwrap()
            .name,
        Some(task.title.clone())
    );
    assert_eq!(recorder.calls().len(), 1);

    let tm = ThreadManager::new(dir.path()).unwrap();
    let systems = tm.messages_of_kind(&task.id, MessageKind::System).unwrap();
    assert!(
        systems
            .iter()
            .any(|m| m.body.contains("re-run from In Progress")),
        "system message about the re-run must be recorded"
    );
}

#[test]
fn rerun_in_progress_refuses_healthy_session() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Healthy")).unwrap();
    ops.take_task(&task.id, "ses-alive", true).unwrap();
    recorder.calls.lock().unwrap().clear();

    assert!(
        ops.rerun_in_progress_task(&task.id, None, RunMode::Immediate)
            .unwrap()
            .is_none()
    );
    assert!(recorder.calls().is_empty());
}

#[test]
fn revoke_in_progress_replaces_exited_wait_and_fences_stale_request() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Wake now")).unwrap();
    ops.take_task(&task.id, "ses-revoke-old", true)
        .unwrap()
        .unwrap();
    ops.declare_waiting(&task.id, "ses-revoke-old", Some(60), Some("wake me"))
        .unwrap();
    ops.reconcile_agent_exit(&task.id, "ses-revoke-old", 0)
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    let revoked = ops
        .revoke_in_progress_task(&task.id, Some("ses-revoke-old"))
        .unwrap()
        .unwrap();
    assert_eq!(
        revoked.run_phase,
        Some(RunPhase::Queued),
        "revoking a paused task parks it in the queue"
    );
    assert_eq!(revoked.session, None);

    // The revoke pumps the queue, so the idle board starts a fresh session.
    assert_eq!(recorder.calls().len(), 1);
    let new_session = recorder.calls()[0].1.clone();
    let sessions = SessionManager::new(dir.path());
    assert!(!sessions.is_session_active("ses-revoke-old"));
    assert!(sessions.is_session_active(&new_session));
    assert_eq!(
        ops.get_task(&task.id).unwrap().unwrap().session.as_deref(),
        Some(new_session.as_str())
    );
    assert!(
        ops.revoke_in_progress_task(&task.id, Some("ses-revoke-old"))
            .unwrap()
            .is_none(),
        "a stale snapshot must not replace the successor session"
    );
    assert_eq!(recorder.calls().len(), 1);

    let stale_completion = ops.complete_task(&task.id, "ses-revoke-old", true);
    assert!(
        matches!(stale_completion, Err(KanbanError::Permission(_))),
        "the revoked process must not be able to complete its successor's task"
    );
}

#[test]
fn revoke_in_progress_refuses_unhosted_live_process() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Still live")).unwrap();
    ops.take_task(&task.id, "ses-revoke-live", true)
        .unwrap()
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    let result = ops.revoke_in_progress_task(&task.id, Some("ses-revoke-live"));

    assert!(matches!(result, Err(KanbanError::Invalid(_))));
    assert_eq!(recorder.calls().len(), 0);
    assert_eq!(
        ops.get_task(&task.id).unwrap().unwrap().session.as_deref(),
        Some("ses-revoke-live")
    );
}

#[test]
fn revoke_does_not_touch_session_id_reused_by_another_task() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let old_task = ops.create_task(NewTask::titled("Old owner")).unwrap();
    ops.take_task(&old_task.id, "ses-reused", true)
        .unwrap()
        .unwrap();
    SessionManager::new(dir.path())
        .close_session("ses-reused")
        .unwrap();
    let new_task = ops.create_task(NewTask::titled("New owner")).unwrap();
    ops.take_task(&new_task.id, "ses-reused", true)
        .unwrap()
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    let result = ops
        .revoke_in_progress_task(&old_task.id, Some("ses-reused"))
        .unwrap();

    assert!(result.is_none());
    let manager = SessionManager::new(dir.path());
    assert!(manager.is_session_active("ses-reused"));
    assert_eq!(
        manager.load_session("ses-reused").unwrap().task_id,
        new_task.id
    );
    assert!(recorder.calls().is_empty());
}

#[test]
fn stale_agent_session_cannot_ask_after_revoke() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask::titled("No stale questions"))
        .unwrap();
    ops.take_task(&task.id, "ses-ask-old", true)
        .unwrap()
        .unwrap();
    SessionManager::new(_dir.path())
        .mark_wait_exited("ses-ask-old")
        .unwrap();
    ops.revoke_in_progress_task(&task.id, Some("ses-ask-old"))
        .unwrap()
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    let result = ops.ask_question_for_session(
        &task.id,
        "Stale question?",
        "agent",
        Some("ses-ask-old"),
        vec![],
    );

    assert!(matches!(result, Err(KanbanError::Permission(_))));
    assert!(!ops.get_task(&task.id).unwrap().unwrap().has_questions);
}

#[test]
fn revoke_in_progress_starts_sessionless_task() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask::titled("Wake sessionless"))
        .unwrap();
    ops.update_task(
        &task.id,
        TaskPatch {
            status: Some("in_progress".into()),
            ..Default::default()
        },
    )
    .unwrap();

    let revoked = ops
        .revoke_in_progress_task(&task.id, None)
        .unwrap()
        .unwrap();

    assert!(revoked.session.is_some());
    assert_eq!(recorder.calls().len(), 1);
}

#[test]
fn abandon_task_removes_the_sidecar_thread() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Doomed")).unwrap();
    ops.ask_question(&task.id, "Still needed?", "agent", vec![])
        .unwrap();
    let thread_file = dir
        .path()
        .join(".kanban/threads")
        .join(format!("{}.yaml", task.id));
    assert!(thread_file.is_file());

    assert!(ops.abandon_task(&task.id).unwrap());
    assert!(
        !thread_file.exists(),
        "a deleted task must not leave its thread for the next task on this id"
    );
}

#[test]
fn abandon_stalled_tasks_skips_questioned_ones() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let stalled = ops.create_task(NewTask::titled("Stalled")).unwrap();
    let questioned = ops.create_task(NewTask::titled("Questioned")).unwrap();
    ops.take_task(&stalled.id, "ses-s", true).unwrap();
    ops.take_task(&questioned.id, "ses-q", true).unwrap();

    let session_mgr = SessionManager::new(dir.path());
    session_mgr.crash_session("ses-s").unwrap();
    session_mgr.crash_session("ses-q").unwrap();
    ops.ask_question(&questioned.id, "What now?", "agent", vec![])
        .unwrap();

    let abandoned = ops.abandon_stalled_tasks().unwrap();
    assert_eq!(abandoned.len(), 1);
    assert_eq!(abandoned[0].id, stalled.id);
    assert!(ops.get_task(&stalled.id).unwrap().is_none());
    assert!(ops.get_task(&questioned.id).unwrap().is_some());
}

#[test]
fn archive_done_and_mark_review_done() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let a = ops.create_task(NewTask::titled("A")).unwrap();
    let b = ops.create_task(NewTask::titled("B")).unwrap();
    ops.move_task(&a.id, "done", false).unwrap();
    ops.move_task(&b.id, "review", false).unwrap();

    let marked = ops.mark_review_tasks_done().unwrap();
    assert_eq!(marked.len(), 1);
    assert_eq!(marked[0].status, TaskStatus::Done);

    let archived = ops.archive_done_tasks().unwrap();
    assert_eq!(archived.len(), 2);
    assert!(
        ops.list_tasks(Some("archive"), None, "created", "asc")
            .unwrap()
            .len()
            == 2
    );
}

#[test]
fn check_sessions_crashes_stale_heartbeats() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Heartbeat")).unwrap();
    let session_mgr = SessionManager::new(dir.path());
    let mut session = session_mgr.link_session(&task.id, "ses-old").unwrap();

    session.last_seen = timefmt::now() - chrono::Duration::seconds(1000);
    session_mgr.save_session(&session).unwrap();

    let crashed = session_mgr.check_sessions(300).unwrap();
    assert_eq!(crashed.len(), 1);
    assert!(!session_mgr.is_session_active("ses-old"));

    // the task's thread records the crash
    let tm = ThreadManager::new(dir.path()).unwrap();
    let systems = tm.messages_of_kind(&task.id, MessageKind::System).unwrap();
    assert!(systems.iter().any(|m| m.body.contains("marked crashed")));
}

#[test]
fn declared_wait_exempts_stale_session_until_deadline() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Wait safely")).unwrap();
    let session_mgr = SessionManager::new(dir.path());
    let mut session = session_mgr.link_session(&task.id, "ses-wait").unwrap();
    session.last_seen = timefmt::now() - chrono::Duration::seconds(1000);
    session.wait_until = Some(timefmt::now() + chrono::Duration::seconds(60));
    session.wait_note = Some("external query".to_string());
    session_mgr.save_session(&session).unwrap();

    let crashed = session_mgr.check_sessions(300).unwrap();

    assert!(crashed.is_empty());
    assert!(session_mgr.is_session_active("ses-wait"));
    assert_eq!(
        session_mgr.session_state("ses-wait", 300),
        Some(SessionState::Waiting)
    );
}

#[test]
fn clean_agent_exit_auto_resumes_stranded_in_progress_task() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Resume me")).unwrap();
    ops.take_task(&task.id, "ses-old", true).unwrap().unwrap();
    recorder.calls.lock().unwrap().clear();

    let outcome = ops.reconcile_agent_exit(&task.id, "ses-old", 0).unwrap();

    let AgentExitOutcome::Resumed(new_session) = outcome else {
        panic!("expected auto-resume, got {outcome:?}");
    };
    assert_ne!(new_session, "ses-old");
    assert_eq!(
        recorder.calls(),
        vec![(task.id.clone(), new_session.clone(), false)]
    );
    let session_mgr = SessionManager::new(dir.path());
    assert!(!session_mgr.is_session_active("ses-old"));
    assert!(session_mgr.is_session_active(&new_session));
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.session.as_deref(), Some(new_session.as_str()));
    assert_eq!(stored.auto_resumes, 1);

    let contexts = ThreadManager::new(dir.path())
        .unwrap()
        .messages_of_kind(&task.id, MessageKind::Context)
        .unwrap();
    assert!(contexts.iter().any(|message| {
        message.body.contains("ended without completing")
            && message.body.contains("auto-resuming (attempt 1/3)")
    }));
}

#[test]
fn auto_resume_budget_exhaustion_crashes_stranded_task() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Budget spent")).unwrap();
    ops.take_task(&task.id, "ses-loop", true).unwrap().unwrap();
    recorder.calls.lock().unwrap().clear();
    let mut stored = ops.get_task(&task.id).unwrap().unwrap();
    stored.auto_resumes = 3;
    ops.storage.save_task(&stored).unwrap();

    let outcome = ops.reconcile_agent_exit(&task.id, "ses-loop", 0).unwrap();

    assert_eq!(outcome, AgentExitOutcome::ResumeExhausted);
    assert!(recorder.calls().is_empty());
    assert_eq!(
        SessionManager::new(dir.path()).session_state("ses-loop", 300),
        Some(SessionState::Crashed)
    );
}

#[test]
fn auto_resume_launch_failure_is_reported_and_crashes_new_session() {
    let (dir, setup_ops, _recorder) = ops_with_recorder(true);
    let task = setup_ops
        .create_task(NewTask::titled("Launch failure"))
        .unwrap();
    setup_ops
        .take_task(&task.id, "ses-before-fail", true)
        .unwrap()
        .unwrap();
    let ops = Operations::with_launcher(dir.path(), Box::new(FailingLauncher));

    let outcome = ops
        .reconcile_agent_exit(&task.id, "ses-before-fail", 0)
        .unwrap();

    let AgentExitOutcome::LaunchFailed(new_session) = outcome else {
        panic!("expected launch failure, got {outcome:?}");
    };
    assert_eq!(
        SessionManager::new(dir.path()).session_state(&new_session, 300),
        Some(SessionState::Crashed)
    );
    assert_eq!(
        ops.get_task(&task.id).unwrap().unwrap().session.as_deref(),
        Some(new_session.as_str())
    );
}

#[test]
fn clean_agent_exit_during_declared_wait_keeps_session_for_deadline() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Declared wait")).unwrap();
    ops.take_task(&task.id, "ses-waiting", true)
        .unwrap()
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    let deadline = ops
        .declare_waiting(&task.id, "ses-waiting", Some(10), Some("analytics query"))
        .unwrap();
    let outcome = ops
        .reconcile_agent_exit(&task.id, "ses-waiting", 0)
        .unwrap();

    assert_eq!(outcome, AgentExitOutcome::Waiting);
    assert!(recorder.calls().is_empty());
    let session = SessionManager::new(dir.path())
        .load_session("ses-waiting")
        .unwrap();
    assert_eq!(session.wait_until, Some(deadline));
    assert_eq!(session.wait_note.as_deref(), Some("analytics query"));
    assert!(session.wait_exited);
    assert_eq!(
        ops.get_task(&task.id).unwrap().unwrap().session.as_deref(),
        Some("ses-waiting")
    );
}

#[test]
fn detach_command_records_output_status_and_wait() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Detached job")).unwrap();
    ops.take_task(&task.id, "ses-detach", true)
        .unwrap()
        .unwrap();

    let job = ops
        .detach_command(
            &task.id,
            "ses-detach",
            Some(10),
            Some("demo export"),
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo detached-output; exit 7".to_string(),
            ],
        )
        .unwrap();

    let poll_deadline = Instant::now() + Duration::from_secs(10);
    while !job.status_file.exists() && Instant::now() < poll_deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        fs::read_to_string(&job.status_file).unwrap().trim(),
        "7",
        "detached job records its exit code"
    );
    assert!(
        fs::read_to_string(&job.log_file)
            .unwrap()
            .contains("detached-output")
    );

    let session = SessionManager::new(dir.path())
        .load_session("ses-detach")
        .unwrap();
    assert_eq!(session.wait_until, Some(job.deadline));
    let note = session.wait_note.expect("wait note recorded");
    assert!(note.contains("demo export"));
    assert!(note.contains(".kanban/detached/"));
    assert!(note.contains("exit code"));

    ops.abandon_task(&task.id).unwrap();
    assert!(!job.log_file.exists());
    assert!(!job.status_file.exists());
}

#[test]
fn detach_command_requires_owning_active_session_and_a_command() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Guarded detach")).unwrap();
    ops.take_task(&task.id, "ses-owner", true).unwrap().unwrap();

    let cmd = vec!["true".to_string()];
    assert!(
        ops.detach_command(&task.id, "ses-other", Some(10), None, &cmd)
            .is_err()
    );
    assert!(
        ops.detach_command(&task.id, "ses-owner", Some(10), None, &[])
            .is_err()
    );

    let detached_dir = dir.path().join(".kanban/detached");
    let leftovers = fs::read_dir(&detached_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "rejected detach must not leave artifacts");
    let session = SessionManager::new(dir.path())
        .load_session("ses-owner")
        .unwrap();
    assert!(session.wait_until.is_none(), "no wait may be declared");
}

#[test]
fn expired_declared_wait_queues_agent_to_check_result() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    fs::write(
        dir.path().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  max_running_total: 1\n",
    )
    .unwrap();
    let task = ops.create_task(NewTask::titled("Check later")).unwrap();
    ops.take_task(&task.id, "ses-wait-old", true)
        .unwrap()
        .unwrap();
    ops.declare_waiting(&task.id, "ses-wait-old", Some(10), Some("batch export"))
        .unwrap();
    ops.reconcile_agent_exit(&task.id, "ses-wait-old", 0)
        .unwrap();
    let occupant = ops.create_task(NewTask::titled("Occupant")).unwrap();
    ops.take_task(&occupant.id, "ses-occupant", true)
        .unwrap()
        .unwrap();
    recorder.calls.lock().unwrap().clear();
    let session_mgr = SessionManager::new(dir.path());
    let mut session = session_mgr.load_session("ses-wait-old").unwrap();
    session.wait_until = Some(timefmt::now() - chrono::Duration::seconds(1));
    session_mgr.save_session(&session).unwrap();

    let woken = ops.wake_expired_waits().unwrap();

    assert_eq!(
        woken,
        vec![WaitWake::Queued {
            task_id: task.id.clone()
        }]
    );
    assert!(
        recorder.calls().is_empty(),
        "the wake itself must not launch past the caps"
    );
    assert!(!session_mgr.is_session_active("ses-wait-old"));
    let parked = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(parked.run_phase, Some(RunPhase::Queued));
    assert_eq!(parked.session, None);
    let contexts = ThreadManager::new(dir.path())
        .unwrap()
        .messages_of_kind(&task.id, MessageKind::Context)
        .unwrap();
    assert!(contexts.iter().any(|message| {
        message.body.contains("Waiting deadline passed")
            && message.body.contains("batch export")
            && message.body.contains("declare waiting again")
    }));

    // The dispatcher starts the re-queued task once a slot is free.
    let mut occupant_session = session_mgr.load_session("ses-occupant").unwrap();
    occupant_session.status = SessionStatus::Closed;
    session_mgr.save_session(&occupant_session).unwrap();
    let started = ops.dispatch_queue().unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].task_id, task.id);
    assert_eq!(
        recorder.calls(),
        vec![(task.id.clone(), started[0].session_id.clone(), false)]
    );
}

#[test]
fn expired_declared_wait_launch_failure_crashes_new_session() {
    let (dir, setup_ops, _recorder) = ops_with_recorder(true);
    let task = setup_ops
        .create_task(NewTask::titled("Wait launch failure"))
        .unwrap();
    setup_ops
        .take_task(&task.id, "ses-wait-fail", true)
        .unwrap()
        .unwrap();
    setup_ops
        .declare_waiting(&task.id, "ses-wait-fail", Some(10), Some("fragile export"))
        .unwrap();
    setup_ops
        .reconcile_agent_exit(&task.id, "ses-wait-fail", 0)
        .unwrap();
    let session_mgr = SessionManager::new(dir.path());
    let mut session = session_mgr.load_session("ses-wait-fail").unwrap();
    session.wait_until = Some(timefmt::now() - chrono::Duration::seconds(1));
    session_mgr.save_session(&session).unwrap();
    let ops = Operations::with_launcher(dir.path(), Box::new(FailingLauncher));

    let woken = ops.wake_expired_waits().unwrap();

    // The wake parks the task in the queue; the dispatcher's own pump then
    // claims it, and the failing launch routes to the crash-restart backoff.
    assert_eq!(
        woken,
        vec![WaitWake::Queued {
            task_id: task.id.clone()
        }]
    );
    assert!(!session_mgr.is_session_active("ses-wait-fail"));
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    let new_session = stored.session.expect("claimed session persisted");
    assert_ne!(new_session, "ses-wait-fail");
    assert_eq!(
        session_mgr.session_state(&new_session, 300),
        Some(SessionState::Crashed)
    );
    assert_eq!(
        stored.run_phase,
        Some(RunPhase::Queued),
        "the failed dispatch hands the task back to the queue"
    );
    assert!(
        stored.restart_at.is_some(),
        "the failed launch must enter the crash-restart backoff"
    );
}

#[test]
fn expired_declared_wait_without_auto_launch_crashes_old_session() {
    let (dir, ops, recorder) = ops_with_recorder(false);
    let task = ops
        .create_task(NewTask::titled("Wait without launcher"))
        .unwrap();
    ops.take_task(&task.id, "ses-wait-disabled", true)
        .unwrap()
        .unwrap();
    ops.declare_waiting(
        &task.id,
        "ses-wait-disabled",
        Some(10),
        Some("manual export"),
    )
    .unwrap();
    ops.reconcile_agent_exit(&task.id, "ses-wait-disabled", 0)
        .unwrap();
    recorder.calls.lock().unwrap().clear();
    let session_mgr = SessionManager::new(dir.path());
    let mut session = session_mgr.load_session("ses-wait-disabled").unwrap();
    session.wait_until = Some(timefmt::now() - chrono::Duration::seconds(1));
    session_mgr.save_session(&session).unwrap();

    let resumed = ops.wake_expired_waits().unwrap();

    assert!(resumed.is_empty());
    assert!(recorder.calls().is_empty());
    assert_eq!(
        session_mgr.session_state("ses-wait-disabled", 300),
        Some(SessionState::Crashed)
    );
    assert_eq!(
        ops.get_task(&task.id).unwrap().unwrap().session.as_deref(),
        Some("ses-wait-disabled")
    );
}

#[test]
fn expired_declared_wait_respects_auto_resume_budget() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Wait budget")).unwrap();
    ops.take_task(&task.id, "ses-wait-budget", true)
        .unwrap()
        .unwrap();
    ops.declare_waiting(&task.id, "ses-wait-budget", Some(10), Some("budgeted wait"))
        .unwrap();
    ops.reconcile_agent_exit(&task.id, "ses-wait-budget", 0)
        .unwrap();
    recorder.calls.lock().unwrap().clear();
    let session_mgr = SessionManager::new(dir.path());
    let mut session = session_mgr.load_session("ses-wait-budget").unwrap();
    session.wait_until = Some(timefmt::now() - chrono::Duration::seconds(1));
    session_mgr.save_session(&session).unwrap();
    let mut stored = ops.get_task(&task.id).unwrap().unwrap();
    stored.auto_resumes = 3;
    ops.storage.save_task(&stored).unwrap();

    let resumed = ops.wake_expired_waits().unwrap();

    assert!(resumed.is_empty());
    assert!(recorder.calls().is_empty());
    assert_eq!(
        session_mgr.session_state("ses-wait-budget", 300),
        Some(SessionState::Crashed)
    );
}

#[test]
fn legacy_questions_block_migrates_to_thread() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Legacy")).unwrap();
    let description = "Base description\n\n## Questions\n- [ ] **2026-06-01T10:00:00** (agent): Should I use JWT?\n  - Answer: Yes, use JWT\n- [ ] **2026-06-01T10:05:00** (agent): Second question?\n  - Answer: _(pending)_\n\n## Notes\nKeep this section";
    ops.update_task(
        &task.id,
        TaskPatch {
            description: Some(description.into()),
            ..Default::default()
        },
    )
    .unwrap();

    let open = ops.list_open_messages(&task.id).unwrap();
    assert_eq!(open.len(), 1, "only the unanswered question stays open");
    assert_eq!(open[0].body, "Second question?");

    let migrated = ops.get_task(&task.id).unwrap().unwrap();
    assert!(!migrated.description.contains("## Questions"));
    assert!(migrated.description.contains("Base description"));
    assert!(migrated.description.contains("## Notes"));
    assert!(migrated.has_questions);
}

#[test]
fn update_task_patch_sets_and_clears_chain() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let a = ops.create_task(NewTask::titled("A")).unwrap();
    let b = ops.create_task(NewTask::titled("B")).unwrap();

    let chained = ops
        .update_task(
            &b.id,
            TaskPatch {
                chained_to: Some(Some(a.id.clone())),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(chained.chained_to.as_deref(), Some(a.id.as_str()));

    let cleared = ops
        .update_task(
            &b.id,
            TaskPatch {
                chained_to: Some(None),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(cleared.chained_to, None);
}

#[test]
fn context_append_updates_size_and_compaction_dedupes() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Ctx")).unwrap();
    let ctx = ContextManager::new(dir.path());

    ctx.append_context(&task.id, "step one", "agent", &ops.storage)
        .unwrap();
    ctx.append_context(&task.id, "step two", "agent", &ops.storage)
        .unwrap();

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert!(stored.context_size > 0);
    let gathered = ctx.get_context(&task.id, &ops.storage).unwrap();
    assert!(gathered.contains("step one") && gathered.contains("step two"));

    let text = "line\nline\nline\nWorking on a\nWorking on b\nChecking c\n## Head\n\n\n\nend";
    let compacted = kanban4ai::core::compaction::compact_text(text);
    assert_eq!(
        compacted, "line\nWorking on a\n## Head\n\nend",
        "duplicates, chatter runs, and blank runs collapse"
    );
}

#[test]
fn bulk_move_moves_column_and_triggers_chains() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let a = ops.create_task(NewTask::titled("First")).unwrap();
    let b = ops.create_task(NewTask::titled("Second")).unwrap();
    ops.move_task(&a.id, "in_progress", false).unwrap();
    ops.move_task(&b.id, "in_progress", false).unwrap();
    let chained = ops.create_task(NewTask::titled("Chained")).unwrap();
    ops.update_task(
        &chained.id,
        TaskPatch {
            chained_to: Some(Some(a.id.clone())),
            ..Default::default()
        },
    )
    .unwrap();

    let moved = ops
        .bulk_move(TaskStatus::InProgress, TaskStatus::Review)
        .unwrap();
    assert_eq!(moved.len(), 2);
    assert!(moved.iter().all(|t| t.status == TaskStatus::Review));
    assert!(
        ops.list_tasks(Some("in_progress"), None, "created", "asc")
            .unwrap()
            .iter()
            .all(|t| t.id == chained.id),
        "only the auto-launched chained task may occupy In Progress"
    );

    // Entering Review fired the task chained to `a`.
    assert!(
        recorder
            .calls()
            .iter()
            .any(|(task, _, _)| task == &chained.id)
    );

    // Re-running over the now-empty source column is a no-op.
    assert!(
        ops.bulk_move(TaskStatus::InProgress, TaskStatus::Review)
            .unwrap()
            .iter()
            .all(|t| t.id == chained.id)
    );
}

#[test]
fn bulk_move_empty_and_same_status_are_noops() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    assert!(
        ops.bulk_move(TaskStatus::InProgress, TaskStatus::Review)
            .unwrap()
            .is_empty()
    );
    let task = ops.create_task(NewTask::titled("Stay")).unwrap();
    assert!(
        ops.bulk_move(TaskStatus::Todo, TaskStatus::Todo)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ops.get_task(&task.id).unwrap().unwrap().status,
        TaskStatus::Todo
    );
}

#[test]
fn unarchive_task_restores_to_todo() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Old")).unwrap();
    assert!(
        ops.unarchive_task(&task.id).unwrap().is_none(),
        "non-archived tasks are not restored"
    );
    ops.move_task(&task.id, "archive", false).unwrap();

    let restored = ops.unarchive_task(&task.id).unwrap().unwrap();
    assert_eq!(restored.status, TaskStatus::Todo);
    assert_eq!(restored.session, None);
    assert!(ops.list_archived_tasks(None).unwrap().is_empty());
    assert!(ops.unarchive_task("TASK-999").unwrap().is_none());
}

#[test]
fn start_task_launches_fresh_session_and_blocks_double_start() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Run me")).unwrap();

    let session_id = ops.start_task(&task.id).unwrap().unwrap();
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.session.as_deref(), Some(session_id.as_str()));
    let session_mgr = SessionManager::new(dir.path());
    assert!(session_mgr.is_session_active(&session_id));
    assert_eq!(
        session_mgr.load_session(&session_id).unwrap().name,
        Some(task.title)
    );
    assert_eq!(recorder.calls().len(), 1);

    // A second run while the session is live is refused, without a launch.
    assert!(matches!(
        ops.start_task(&task.id),
        Err(KanbanError::Invalid(_))
    ));
    assert_eq!(recorder.calls().len(), 1);

    assert!(ops.start_task("TASK-999").unwrap().is_none());
}

#[test]
fn start_task_launch_failure_surfaces_error_and_rolls_back() {
    let (dir, _storage) = common::quiet_board(true);
    let ops = Operations::with_launcher(dir.path(), Box::new(FailingLauncher));
    let task = ops.create_task(NewTask::titled("Fails")).unwrap();

    assert!(matches!(
        ops.start_task(&task.id),
        Err(KanbanError::Invalid(_))
    ));
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Todo);
    assert!(stored.session.is_some());
}

#[test]
fn launch_revert_persists_session_name() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Revert me")).unwrap();
    let backup_dir = ops.backup_dir(&task.id);
    fs::create_dir_all(&backup_dir).unwrap();
    fs::write(backup_dir.join("changed.txt"), "backup").unwrap();

    assert!(ops.launch_revert(&task.id, "ses-revert-test").unwrap());

    assert_eq!(
        recorder.calls(),
        vec![(task.id.clone(), "ses-revert-test".to_string(), true)]
    );
    assert_eq!(
        SessionManager::new(dir.path())
            .load_session("ses-revert-test")
            .unwrap()
            .name,
        Some(task.title)
    );
}

#[test]
fn stop_session_closes_session_and_keeps_it_on_the_task() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Stop me")).unwrap();
    ops.take_task(&task.id, "ses-stop", true).unwrap().unwrap();

    let stopped = ops.stop_session("ses-stop").unwrap().unwrap();
    assert_eq!(
        stopped.session.as_deref(),
        Some("ses-stop"),
        "a stopped session stays on the task as its last session"
    );
    assert_eq!(
        stopped.status,
        TaskStatus::InProgress,
        "stopping a session must not change task status"
    );
    assert!(!SessionManager::new(dir.path()).is_session_active("ses-stop"));

    let tm = ThreadManager::new(dir.path()).unwrap();
    let systems = tm.messages_of_kind(&task.id, MessageKind::System).unwrap();
    assert!(
        systems
            .iter()
            .any(|m| m.body.contains("stopped by the user"))
    );

    assert!(ops.stop_session("ses-missing").unwrap().is_none());
}

#[test]
fn stop_task_requires_an_active_session() {
    let (_dir, ops, _recorder) = ops_with_recorder(true);
    let idle = ops.create_task(NewTask::titled("Idle")).unwrap();
    assert!(matches!(
        ops.stop_task(&idle.id),
        Err(KanbanError::Invalid(_))
    ));
    assert!(ops.stop_task("TASK-999").unwrap().is_none());

    let running = ops.create_task(NewTask::titled("Running")).unwrap();
    ops.take_task(&running.id, "ses-stop-task", true)
        .unwrap()
        .unwrap();
    let stopped = ops.stop_task(&running.id).unwrap().unwrap();
    assert_eq!(stopped.status, TaskStatus::InProgress);
    assert_eq!(stopped.session.as_deref(), Some("ses-stop-task"));
    assert!(matches!(
        ops.stop_task(&running.id),
        Err(KanbanError::Invalid(_))
    ));
}

#[test]
fn stop_session_then_agent_exit_does_not_resume_or_crash() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Do not resume")).unwrap();
    ops.take_task(&task.id, "ses-stop-exit", true)
        .unwrap()
        .unwrap();
    recorder.calls.lock().unwrap().clear();

    ops.stop_session("ses-stop-exit").unwrap();
    assert_eq!(
        ops.reconcile_agent_exit(&task.id, "ses-stop-exit", 0)
            .unwrap(),
        AgentExitOutcome::Closed
    );
    assert_eq!(
        ops.reconcile_agent_exit(&task.id, "ses-stop-exit", 1)
            .unwrap(),
        AgentExitOutcome::Closed
    );
    assert!(recorder.calls().is_empty());

    let session = SessionManager::new(dir.path())
        .load_session("ses-stop-exit")
        .unwrap();
    assert_eq!(session.status, SessionStatus::Closed);
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.session.as_deref(), Some("ses-stop-exit"));
}

#[test]
fn session_states_reflect_heartbeats_without_mutating() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let a = ops.create_task(NewTask::titled("Fresh")).unwrap();
    let b = ops.create_task(NewTask::titled("Stale")).unwrap();
    ops.take_task(&a.id, "ses-fresh", true).unwrap().unwrap();
    ops.take_task(&b.id, "ses-stale", true).unwrap().unwrap();

    let mgr = SessionManager::new(dir.path());
    let mut stale = mgr.load_session("ses-stale").unwrap();
    stale.last_seen = timefmt::now() - chrono::Duration::seconds(1000);
    mgr.save_session(&stale).unwrap();

    assert_eq!(
        mgr.session_state("ses-fresh", 300),
        Some(SessionState::Live)
    );
    assert_eq!(
        mgr.session_state("ses-stale", 300),
        Some(SessionState::Crashed)
    );
    // Read-only: the stale session is still Active on disk.
    assert!(mgr.is_session_active("ses-stale"));

    let states = mgr.list_sessions_with_state(300);
    assert_eq!(states.len(), 2);

    mgr.close_session("ses-fresh").unwrap();
    assert_eq!(mgr.session_state("ses-fresh", 300), None);
    assert_eq!(mgr.list_sessions_with_state(300).len(), 1);
    assert_eq!(mgr.session_state("ses-unknown", 300), None);
}

#[test]
fn first_open_question_returns_earliest_open() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Ask")).unwrap();
    assert!(ops.first_open_question(&task.id).unwrap().is_none());

    ops.ask_question(
        &task.id,
        "First?",
        "agent",
        vec!["A".to_string(), "B".to_string()],
    )
    .unwrap();
    ops.ask_question(&task.id, "Second?", "agent", vec![])
        .unwrap();

    let first = ops.first_open_question(&task.id).unwrap().unwrap();
    assert_eq!(first.body, "First?");
    assert_eq!(first.variants, vec!["A".to_string(), "B".to_string()]);

    ops.answer_question(&task.id, QuestionRef::MsgId(first.id.clone()), "A")
        .unwrap();
    assert_eq!(
        ops.first_open_question(&task.id).unwrap().unwrap().body,
        "Second?"
    );
}

#[test]
fn agent_launch_logs_agent_step_and_dumps_prompt() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            agent_backend: Some("claude".to_string()),
            ..NewTask::titled("Log launch")
        })
        .unwrap();

    ops.take_task(&task.id, "ses-log", true).unwrap().unwrap();
    assert_eq!(
        recorder.calls(),
        vec![(task.id.clone(), "ses-log".to_string(), false)]
    );

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let step = thread
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::AgentStep && m.body.starts_with("▶ launch"))
        .expect("launch step logged");
    assert!(step.body.contains("session=ses-log"));
    assert!(step.body.contains("backend=claude"));
    assert!(
        step.body
            .contains("prompt: .kanban/logs/ses-log.prompt.txt")
    );
    assert_eq!(step.author.as_deref(), Some("kanban"));
    assert_eq!(step.origin.as_deref(), Some("kanban"));

    let prompt_path = dir.path().join(".kanban/logs/ses-log.prompt.txt");
    let dumped = fs::read_to_string(prompt_path).expect("prompt dump exists");
    assert!(dumped.contains(&task.id));
}

#[test]
fn agent_context_records_its_session_origin() {
    let (dir, ops, _recorder) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Context origin")).unwrap();
    ops.take_task(&task.id, "ses-context", true)
        .unwrap()
        .unwrap();

    ContextManager::new(dir.path())
        .append_context_with_session(
            &task.id,
            "implementation detail",
            "agent",
            Some("ses-context"),
            &ops.storage,
        )
        .unwrap();

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let context = thread
        .messages
        .iter()
        .find(|message| message.kind == MessageKind::Context)
        .expect("agent context stored");
    assert_eq!(context.origin.as_deref(), Some("agent:ses-context"));

    let task = ops.storage.load_task(&task.id).unwrap().unwrap();
    let prompt = kanban4ai::agent::build_agent_prompt(
        dir.path(),
        &task,
        "ses-context",
        false,
        Role::Executor,
    )
    .unwrap();
    assert!(prompt.contains("origin=agent:ses-context"));

    ContextManager::new(dir.path())
        .append_context_with_session(
            &task.id,
            "stale process detail",
            "agent",
            Some("ses-stale"),
            &ops.storage,
        )
        .unwrap();
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let stale_context = thread
        .messages
        .iter()
        .find(|message| message.body == "stale process detail")
        .expect("stale agent context stored");
    assert_eq!(stale_context.origin.as_deref(), Some("agent"));
}

#[test]
fn agent_exit_logs_agent_step_with_code_and_outcome() {
    let (dir, ops, recorder) = ops_with_recorder(true);

    let crashed_task = ops.create_task(NewTask::titled("Exit crash")).unwrap();
    ops.take_task(&crashed_task.id, "ses-exit-crash", true)
        .unwrap()
        .unwrap();
    recorder.calls.lock().unwrap().clear();
    let outcome = ops
        .reconcile_agent_exit(&crashed_task.id, "ses-exit-crash", 1)
        .unwrap();
    assert_eq!(outcome, AgentExitOutcome::Crashed);
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&crashed_task.id)
        .unwrap();
    let step = thread
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::AgentStep && m.body.starts_with("■ exit"))
        .expect("exit step logged");
    assert!(step.body.contains("session=ses-exit-crash"));
    assert!(step.body.contains("code=1"));
    assert!(step.body.contains("outcome=Crashed"));

    let closed_task = ops.create_task(NewTask::titled("Exit closed")).unwrap();
    ops.take_task(&closed_task.id, "ses-exit-closed", true)
        .unwrap()
        .unwrap();
    recorder.calls.lock().unwrap().clear();
    ops.ask_question(&closed_task.id, "Need input?", "agent", vec![])
        .unwrap();
    let outcome = ops
        .reconcile_agent_exit(&closed_task.id, "ses-exit-closed", 0)
        .unwrap();
    assert_eq!(outcome, AgentExitOutcome::Closed);
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&closed_task.id)
        .unwrap();
    let step = thread
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::AgentStep && m.body.starts_with("■ exit"))
        .expect("exit step logged");
    assert!(step.body.contains("code=0"));
    assert!(step.body.contains("outcome=Closed"));
}

#[test]
fn agent_exit_harvests_claude_transcript_into_provenance_manifest() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            agent_backend: Some("claude".to_string()),
            ..NewTask::titled("Harvest inputs")
        })
        .unwrap();
    ops.take_task(&task.id, "ses-harvest", true)
        .unwrap()
        .unwrap();

    // Seed the machine transcript the claude wrapper would have captured.
    let transcript = dir.path().join(".kanban/logs/ses-harvest.transcript.jsonl");
    fs::write(
        &transcript,
        concat!(
            r#"{"type":"system","subtype":"init","session_id":"claude-xyz"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}},{"type":"tool_use","name":"WebFetch","input":{"url":"https://example.com"}}]}}"#,
            "\n",
        ),
    )
    .unwrap();

    let outcome = ops
        .reconcile_agent_exit(&task.id, "ses-harvest", 1)
        .unwrap();
    assert_eq!(outcome, AgentExitOutcome::Crashed);

    // Manifest written as a decoupled sidecar, not into the thread.
    let manifest_raw = fs::read_to_string(dir.path().join(".kanban/provenance/ses-harvest.yaml"))
        .expect("provenance manifest written");
    assert!(manifest_raw.contains("src/lib.rs"));
    assert!(manifest_raw.contains("https://example.com"));
    assert!(manifest_raw.contains("claude-xyz"));

    // Exit step references the manifest and carries the input summary.
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let step = thread
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::AgentStep && m.body.starts_with("■ exit"))
        .expect("exit step logged");
    assert!(step.body.contains("reads=1 writes=0 urls=1"));
    assert!(
        step.body
            .contains("provenance: .kanban/provenance/ses-harvest.yaml")
    );
    // The manifest content must never leak into the thread as a message.
    assert!(
        !thread
            .messages
            .iter()
            .any(|m| m.body.contains("https://example.com"))
    );
}

#[test]
fn agent_exit_harvests_opencode_transcript_into_provenance_manifest() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            agent_backend: Some("opencode".to_string()),
            ..NewTask::titled("Harvest opencode inputs")
        })
        .unwrap();
    ops.take_task(&task.id, "ses-oc-harvest", true)
        .unwrap()
        .unwrap();

    // Seed the machine transcript the opencode wrapper (`run --format json`)
    // would have captured on stdout.
    let transcript = dir
        .path()
        .join(".kanban/logs/ses-oc-harvest.transcript.jsonl");
    fs::write(
        &transcript,
        concat!(
            r#"{"type":"tool_use","sessionID":"ses_real","part":{"type":"tool","tool":"read","state":{"input":{"filePath":"src/lib.rs"}}}}"#,
            "\n",
            r#"{"type":"tool_use","sessionID":"ses_real","part":{"type":"tool","tool":"webfetch","state":{"input":{"url":"https://example.com"}}}}"#,
            "\n",
        ),
    )
    .unwrap();

    let outcome = ops
        .reconcile_agent_exit(&task.id, "ses-oc-harvest", 1)
        .unwrap();
    assert_eq!(outcome, AgentExitOutcome::Crashed);

    let manifest_raw =
        fs::read_to_string(dir.path().join(".kanban/provenance/ses-oc-harvest.yaml"))
            .expect("opencode provenance manifest written");
    assert!(manifest_raw.contains("backend: opencode"));
    assert!(manifest_raw.contains("src/lib.rs"));
    assert!(manifest_raw.contains("https://example.com"));
    assert!(manifest_raw.contains("ses_real"));

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let step = thread
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::AgentStep && m.body.starts_with("■ exit"))
        .expect("exit step logged");
    assert!(step.body.contains("reads=1 writes=0 urls=1"));
}

/// Two tasks whose sessions ran concurrently and both wrote the same file:
/// the later exit detects the overlap against the earlier session's manifest
/// and warns on both threads. A re-harvest of the same session must not
/// double-post (dedup by session-id pair on the thread).
#[test]
fn concurrent_provenance_overlap_warns_both_task_threads() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let claude_task = |title: &str| NewTask {
        agent_backend: Some("claude".to_string()),
        ..NewTask::titled(title)
    };
    let task_a = ops.create_task(claude_task("Writer A")).unwrap();
    let task_b = ops.create_task(claude_task("Writer B")).unwrap();
    ops.take_task(&task_a.id, "ses-ovl-a", true)
        .unwrap()
        .unwrap();
    ops.take_task(&task_b.id, "ses-ovl-b", true)
        .unwrap()
        .unwrap();

    // Both runs edit the same file; the wrapper teed each transcript to logs.
    let transcript = |session: &str| {
        let path = dir
            .path()
            .join(".kanban/logs")
            .join(format!("{session}.transcript.jsonl"));
        fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/lib.rs"}}]}}"#,
                "\n",
            ),
        )
        .unwrap();
    };
    transcript("ses-ovl-a");

    // The overlapping run was heartbeating right up to its exit, so its
    // lifetime reaches into the peer's window.
    let sessions = SessionManager::new(dir.path());
    let mut record = sessions.load_session("ses-ovl-a").unwrap();
    record.last_seen = timefmt::now();
    sessions.save_session(&record).unwrap();

    // First exit: the peer has no manifest yet, so nothing is posted.
    ops.reconcile_agent_exit(&task_a.id, "ses-ovl-a", 1)
        .unwrap();
    let thread_a = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task_a.id)
        .unwrap();
    assert!(
        !thread_a
            .messages
            .iter()
            .any(|m| m.body.contains("provenance overlap"))
    );

    // Second exit: ses-ovl-a's manifest exists, the lifetimes overlapped,
    // and both wrote src/lib.rs — both threads warn.
    transcript("ses-ovl-b");
    ops.reconcile_agent_exit(&task_b.id, "ses-ovl-b", 1)
        .unwrap();

    for task_id in [&task_a.id, &task_b.id] {
        let thread = ThreadManager::new(dir.path())
            .unwrap()
            .load(task_id)
            .unwrap();
        let warnings: Vec<_> = thread
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Context && m.body.contains("provenance overlap"))
            .collect();
        assert_eq!(warnings.len(), 1, "one overlap warning on {task_id}");
        let body = &warnings[0].body;
        assert!(body.contains("ses-ovl-a") && body.contains("ses-ovl-b"));
        assert!(body.contains(&task_a.id) && body.contains(&task_b.id));
        assert!(body.contains("src/lib.rs"));
    }

    // A stale-callback re-harvest of the same session must not double-post.
    ops.reconcile_agent_exit(&task_b.id, "ses-ovl-b", 0)
        .unwrap();
    let thread_a = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task_a.id)
        .unwrap();
    assert_eq!(
        thread_a
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Context && m.body.contains("provenance overlap"))
            .count(),
        1
    );
}

/// The agent's whole session answer must reach the thread, not just the log
/// file: claude prints the substantive answer and then a closing wrap-up
/// (repeated in the `result` event), and both texts are posted together as a
/// `context` message ahead of the exit audit line.
#[test]
fn agent_exit_appends_claude_session_reply_to_thread() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            agent_backend: Some("claude".to_string()),
            ..NewTask::titled("Report back")
        })
        .unwrap();
    ops.take_task(&task.id, "ses-reply", true).unwrap().unwrap();

    let transcript = dir.path().join(".kanban/logs/ses-reply.transcript.jsonl");
    fs::write(
        &transcript,
        concat!(
            r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"Основные разделы:\n\n- src/ — Rust code"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"tool_use","name":"Bash","input":{"command":"kanban done"}}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_3","content":[{"type":"text","text":"Task done, moved to Review."}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","result":"Task done, moved to Review."}"#,
            "\n",
        ),
    )
    .unwrap();

    ops.reconcile_agent_exit(&task.id, "ses-reply", 0).unwrap();

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let reply_index = thread
        .messages
        .iter()
        .position(|m| m.kind == MessageKind::Context && m.body.contains("Основные разделы"))
        .expect("session reply posted to the thread");
    let reply = &thread.messages[reply_index];
    assert_eq!(reply.role, MessageRole::Agent);
    assert_eq!(reply.author.as_deref(), Some("agent-reply"));
    assert_eq!(
        reply.body,
        "Основные разделы:\n\n- src/ — Rust code\n\nTask done, moved to Review."
    );
    // The reply reads before the exit audit line it belongs to.
    let exit_index = thread
        .messages
        .iter()
        .position(|m| m.kind == MessageKind::AgentStep && m.body.starts_with("■ exit"))
        .expect("exit step logged");
    assert!(reply_index < exit_index);
    // Recorded context feeds the next prompt, so its size must be accounted for.
    assert!(ops.get_task(&task.id).unwrap().unwrap().context_size > 0);
}

/// Same contract for opencode, whose answer arrives as `text` events tagged
/// with a `messageID`; a reply already posted on an earlier (stale or
/// duplicated) exit callback is not duplicated.
#[test]
fn agent_exit_appends_opencode_session_reply_without_duplicating_context() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            agent_backend: Some("opencode".to_string()),
            ..NewTask::titled("Report back too")
        })
        .unwrap();
    ops.take_task(&task.id, "ses-oc-reply", true)
        .unwrap()
        .unwrap();

    let transcript = dir
        .path()
        .join(".kanban/logs/ses-oc-reply.transcript.jsonl");
    fs::write(
        &transcript,
        concat!(
            r#"{"type":"text","sessionID":"ses_real","part":{"type":"text","messageID":"msg_a","text":"Reading files."}}"#,
            "\n",
            r#"{"type":"text","sessionID":"ses_real","part":{"type":"text","messageID":"msg_b","text":"Structure confirmed."}}"#,
            "\n",
            r#"{"type":"text","sessionID":"ses_real","part":{"type":"text","messageID":"msg_b","text":"- src/ holds the code"}}"#,
            "\n",
        ),
    )
    .unwrap();

    ops.reconcile_agent_exit(&task.id, "ses-oc-reply", 0)
        .unwrap();
    // A second reconciliation (stale/duplicated callback) must not post twice.
    ops.reconcile_agent_exit(&task.id, "ses-oc-reply", 0)
        .unwrap();

    let replies: Vec<_> = ThreadManager::new(dir.path())
        .unwrap()
        .messages_of_kind(&task.id, MessageKind::Context)
        .unwrap()
        .into_iter()
        .filter(|m| m.body.contains("Structure confirmed"))
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].body,
        "Reading files.\n\nStructure confirmed.\n\n- src/ holds the code"
    );
}

/// `agent_reply_max_chars` caps how much of a long session answer enters the
/// thread, since every entry is replayed into the next prompt.
#[test]
fn agent_reply_is_truncated_to_the_configured_budget() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    fs::write(
        dir.path().join(".kanban/config.yaml"),
        "columns:\n- name: To Do\n  id: todo\n- name: In Progress\n  id: in_progress\n\
         - name: Review\n  id: review\n- name: Done\n  id: done\n\
         notifications:\n  enabled: false\nauto_launch:\n  enabled: true\n\
         thresholds:\n  agent_reply_max_chars: 20\n",
    )
    .unwrap();
    let task = ops
        .create_task(NewTask {
            agent_backend: Some("claude".to_string()),
            ..NewTask::titled("Long answer")
        })
        .unwrap();
    ops.take_task(&task.id, "ses-long", true).unwrap().unwrap();

    let long = "x".repeat(500);
    fs::write(
        dir.path().join(".kanban/logs/ses-long.transcript.jsonl"),
        format!(r#"{{"type":"result","subtype":"success","result":"{long}"}}"#),
    )
    .unwrap();

    ops.reconcile_agent_exit(&task.id, "ses-long", 0).unwrap();

    let reply = ThreadManager::new(dir.path())
        .unwrap()
        .messages_of_kind(&task.id, MessageKind::Context)
        .unwrap()
        .into_iter()
        .find(|m| m.body.starts_with("xxxx"))
        .expect("truncated reply posted");
    assert!(reply.body.starts_with(&"x".repeat(20)));
    assert!(reply.body.ends_with("(agent reply truncated)"));
    assert!(reply.body.len() < long.len());
}

#[test]
fn agent_exit_reconciliation_skips_logging_for_unmatched_session() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Unmatched")).unwrap();

    let outcome = ops
        .reconcile_agent_exit(&task.id, "ses-never-existed", 0)
        .unwrap();
    assert_eq!(outcome, AgentExitOutcome::Closed);

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        !thread
            .messages
            .iter()
            .any(|m| m.kind == MessageKind::AgentStep)
    );
}

#[test]
fn agent_step_messages_are_excluded_from_agent_prompt() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Prompt exclude")).unwrap();
    let taken = ops
        .take_task(&task.id, "ses-prompt", true)
        .unwrap()
        .unwrap();

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|m| m.kind == MessageKind::AgentStep)
    );

    let prompt = kanban4ai::agent::build_agent_prompt(
        dir.path(),
        &taken,
        "ses-prompt",
        false,
        Role::Executor,
    )
    .unwrap();
    assert!(!prompt.contains("▶ launch"));
    assert!(!prompt.contains("agent_step"));
}

#[test]
fn rejected_context_is_excluded_from_prompt_and_gathered_context() {
    let (dir, ops, _recorder) = ops_with_recorder(false);
    let task = ops
        .create_task(NewTask::titled("Poisoned context"))
        .unwrap();

    let ctx = ContextManager::new(dir.path());
    ctx.append_context(&task.id, "trustworthy note", "agent", &ops.storage)
        .unwrap();
    ctx.append_context(&task.id, "poisoned note", "agent", &ops.storage)
        .unwrap();

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let poisoned = thread
        .messages
        .iter()
        .find(|m| m.body == "poisoned note")
        .expect("poisoned context stored")
        .id
        .clone();

    let rejected = ops.reject_message(&task.id, &poisoned).unwrap();
    assert_eq!(rejected.unwrap().status, MessageStatus::Rejected);

    let context = ctx.get_context(&task.id, &ops.storage).unwrap();
    assert!(context.contains("trustworthy note"));
    assert!(!context.contains("poisoned note"));

    let task = ops.storage.load_task(&task.id).unwrap().unwrap();
    let prompt = kanban4ai::agent::build_agent_prompt(
        dir.path(),
        &task,
        "ses-reject",
        false,
        Role::Executor,
    )
    .unwrap();
    assert!(prompt.contains("trustworthy note"));
    assert!(!prompt.contains("poisoned note"));

    // Un-reject restores it to both the gathered context and future prompts.
    let restored = ops.unreject_message(&task.id, &poisoned).unwrap();
    assert_eq!(restored.unwrap().status, MessageStatus::Open);

    let context = ctx.get_context(&task.id, &ops.storage).unwrap();
    assert!(context.contains("poisoned note"));
    let prompt = kanban4ai::agent::build_agent_prompt(
        dir.path(),
        &task,
        "ses-reject",
        false,
        Role::Executor,
    )
    .unwrap();
    assert!(prompt.contains("poisoned note"));
}

#[test]
fn reject_message_reports_missing_task_or_message() {
    let (_dir, ops, _recorder) = ops_with_recorder(false);
    assert!(ops.reject_message("TASK-999", "MSG-001").unwrap().is_none());

    let task = ops.create_task(NewTask::titled("No such message")).unwrap();
    assert!(ops.reject_message(&task.id, "MSG-999").unwrap().is_none());
}

// ------------------------------------------------ data_root / work_path split

/// A registered project whose board lives in the store, with a quiet config
/// and a recording launcher: the phase-2 shape of every real command.
fn split_project_ops(
    verification: Option<&str>,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    kanban4ai::core::project::Project,
    Operations,
    common::RecordingLauncher,
) {
    let store = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = ProjectStore::at(store.path())
        .add(work.path(), None)
        .unwrap()
        .project;
    kanban4ai::core::storage::Storage::new(&project.data_root)
        .init_board()
        .unwrap();
    common::write_quiet_config(&project.data_root, true);
    if let Some(command) = verification {
        write_verification_config(&project.data_root, command, true);
    }
    let recorder = common::RecordingLauncher::new();
    let ops = Operations::for_project_with_launcher(&project, Box::new(recorder.clone()));
    (store, work, project, ops, recorder)
}

/// The board never touches the code folder, and the launcher is handed both
/// roots plus the project id (which the wrapper exports as `KANBAN_PROJECT`).
#[test]
fn for_project_writes_the_board_to_the_store_and_launches_in_the_work_folder() {
    let (_store, work, project, ops, recorder) = split_project_ops(None);

    assert_eq!(ops.data_root(), project.data_root);
    assert_eq!(ops.work_path(), project.work_path);

    let task = ops.create_task(NewTask::titled("Split roots")).unwrap();
    assert!(project.data_root.join(".kanban/tasks").is_dir());
    assert!(
        !work.path().join(".kanban").exists(),
        "the work folder must stay clean"
    );

    ops.take_task(&task.id, "ses-split", true).unwrap().unwrap();
    assert_eq!(
        recorder.roots(),
        vec![(
            project.data_root.clone(),
            project.work_path.clone(),
            Some(project.id.clone())
        )]
    );
}

/// A detached job is the agent's own command line: it runs in the work folder,
/// while its log and status stay with the board.
#[test]
fn detached_job_runs_in_the_work_folder_and_logs_into_the_store() {
    let (_store, _work, project, ops, _recorder) = split_project_ops(None);
    let task = ops.create_task(NewTask::titled("Detached split")).unwrap();
    ops.take_task(&task.id, "ses-split", true).unwrap().unwrap();

    let job = ops
        .detach_command(
            &task.id,
            "ses-split",
            Some(10),
            Some("pwd probe"),
            &["sh".to_string(), "-c".to_string(), "pwd".to_string()],
        )
        .unwrap();

    let poll_deadline = Instant::now() + Duration::from_secs(10);
    while !job.status_file.exists() && Instant::now() < poll_deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(job.log_file.starts_with(project.data_root.join(".kanban")));
    assert_eq!(
        fs::read_to_string(&job.log_file).unwrap().trim(),
        project.work_path.display().to_string()
    );

    // The wait note is read back by the relaunched agent, whose cwd is the
    // work folder, so the board paths in it must be absolute.
    let note = SessionManager::new(&project.data_root)
        .load_session("ses-split")
        .unwrap()
        .wait_note
        .expect("wait note recorded");
    assert!(note.contains(&job.log_file.display().to_string()));
}

/// The verification gate builds and tests the user's code, so it runs in the
/// work folder — not in the store.
#[test]
fn verification_gate_runs_in_the_work_folder() {
    let (_store, work, _project, ops, _recorder) = split_project_ops(Some("test -f marker.txt"));
    ops.config.load_fresh().unwrap();
    fs::write(work.path().join("marker.txt"), "built").unwrap();

    let task = ops.create_task(NewTask::titled("Gate cwd")).unwrap();
    ops.take_task(&task.id, "ses-gate", true).unwrap().unwrap();
    ContextManager::new(ops.data_root())
        .append_context(&task.id, "implemented and tested", "agent", &ops.storage)
        .unwrap();

    let reviewed = ops
        .complete_task(&task.id, "ses-gate", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.status, TaskStatus::Review);
}

#[test]
fn agent_move_to_review_marks_unseen_but_human_move_does_not() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let agent_task = ops.create_task(NewTask::titled("Agent review")).unwrap();
    ops.move_task(&agent_task.id, "in_progress", false).unwrap();
    ops.move_task(&agent_task.id, "review", true).unwrap();
    let stored = ops.get_task(&agent_task.id).unwrap().unwrap();
    assert!(stored.review_unseen, "agent move to review sets the flag");

    let human_task = ops.create_task(NewTask::titled("Human review")).unwrap();
    ops.move_task(&human_task.id, "in_progress", false).unwrap();
    ops.move_task(&human_task.id, "review", false).unwrap();
    let stored = ops.get_task(&human_task.id).unwrap().unwrap();
    assert!(
        !stored.review_unseen,
        "human move to review does not set the flag"
    );
}

#[test]
fn agent_done_moves_to_review_and_marks_unseen() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Agent done flow")).unwrap();
    ops.take_task(&task.id, "ses-done", true).unwrap();
    ContextManager::new(dir.path())
        .append_context(&task.id, "implemented and tested", "agent", &ops.storage)
        .unwrap();
    let reviewed = ops
        .complete_task(&task.id, "ses-done", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.status, TaskStatus::Review);
    assert!(reviewed.review_unseen, "agent done sets review_unseen");
}

#[test]
fn human_move_and_done_clear_review_unseen() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Clear flag")).unwrap();
    ops.move_task(&task.id, "in_progress", false).unwrap();
    ops.move_task(&task.id, "review", true).unwrap();
    assert!(ops.get_task(&task.id).unwrap().unwrap().review_unseen);

    ops.move_task(&task.id, "in_progress", false).unwrap();
    assert!(!ops.get_task(&task.id).unwrap().unwrap().review_unseen);

    ops.move_task(&task.id, "review", true).unwrap();
    assert!(ops.get_task(&task.id).unwrap().unwrap().review_unseen);
    ops.move_task(&task.id, "done", false).unwrap();
    let done = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(done.status, TaskStatus::Done);
    assert!(!done.review_unseen, "human done clears review_unseen");
}

#[test]
fn mark_review_seen_clears_the_flag() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Seen")).unwrap();
    ops.move_task(&task.id, "in_progress", false).unwrap();
    ops.move_task(&task.id, "review", true).unwrap();
    assert!(ops.get_task(&task.id).unwrap().unwrap().review_unseen);

    assert!(
        ops.mark_review_seen(&task.id).unwrap(),
        "flag was set, now cleared"
    );
    assert!(!ops.get_task(&task.id).unwrap().unwrap().review_unseen);

    assert!(!ops.mark_review_seen(&task.id).unwrap());
}

#[test]
fn rerun_review_task_clears_review_unseen() {
    let (dir, ops, _rec) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Rerun unseen")).unwrap();
    ops.take_task(&task.id, "ses-rerun", true).unwrap();
    ContextManager::new(dir.path())
        .append_context(&task.id, "first pass done", "agent", &ops.storage)
        .unwrap();
    let reviewed = ops
        .complete_task(&task.id, "ses-rerun", true)
        .unwrap()
        .unwrap();
    assert!(reviewed.review_unseen);

    let rerun = ops
        .rerun_review_task(&task.id, None, RunMode::Immediate)
        .unwrap()
        .unwrap();
    assert_eq!(rerun.status, TaskStatus::InProgress);
    assert!(!rerun.review_unseen, "rerun clears review_unseen");
}

// ------------------------------------------------------- queued run phase

fn fill_total_slots(ops: &Operations, dir: &Path, count: usize) {
    let session_mgr = SessionManager::new(dir);
    // Spread the fillers across backends so only `max_running_total` is
    // exhausted — every per-backend cap still has room.
    const BACKENDS: [&str; 4] = ["claude", "opencode", "omp", "pi"];
    for n in 0..count {
        let filler = ops
            .create_task(NewTask::titled(format!("Filler {n}")))
            .unwrap();
        let session_id = format!("ses-fill-{n}");
        session_mgr.link_session(&filler.id, &session_id).unwrap();
        let mut current = ops.get_task(&filler.id).unwrap().unwrap();
        current.status = TaskStatus::InProgress;
        current.session = Some(session_id);
        current.agent_backend = Some(BACKENDS[n % BACKENDS.len()].to_string());
        ops.storage.save_task(&current).unwrap();
    }
}

#[test]
fn agent_take_queues_when_the_total_cap_is_exhausted() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    // Default orchestration caps: max_running_total 3.
    fill_total_slots(&ops, dir.path(), 3);

    let task = ops.create_task(NewTask::titled("Queue me")).unwrap();
    let taken = ops
        .take_task(&task.id, "ses-queued", true)
        .unwrap()
        .unwrap();

    // The task lands In Progress queued instead of launching, and no phantom
    // Active session record is minted for the fresh id.
    assert_eq!(taken.status, TaskStatus::InProgress);
    assert_eq!(taken.run_phase, Some(RunPhase::Queued));
    assert!(recorder.calls().is_empty());
    assert!(
        SessionManager::new(dir.path())
            .load_session("ses-queued")
            .is_none()
    );
    assert_eq!(taken.session.as_deref(), None);
}

#[test]
fn agent_take_launches_while_slots_remain() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    fill_total_slots(&ops, dir.path(), 2);

    let task = ops.create_task(NewTask::titled("Launch me")).unwrap();
    let taken = ops.take_task(&task.id, "ses-live", true).unwrap().unwrap();
    assert_eq!(taken.run_phase, None, "a launched task carries no phase");
    assert_eq!(
        recorder.calls(),
        vec![(task.id.clone(), "ses-live".to_string(), false)]
    );
    assert!(
        SessionManager::new(dir.path())
            .load_session("ses-live")
            .is_some()
    );
}

#[test]
fn manual_run_bypasses_the_queue_and_clears_the_marker() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    fill_total_slots(&ops, dir.path(), 3);

    let task = ops.create_task(NewTask::titled("Manual run")).unwrap();
    let session_id = ops.start_task(&task.id).unwrap().unwrap();
    assert_eq!(
        recorder.calls(),
        vec![(task.id.clone(), session_id.clone(), false)]
    );
    let current = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(current.run_phase, None);
}

#[test]
fn explicit_enqueue_and_dequeue_round_trip() {
    let (dir, ops, recorder) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Round trip")).unwrap();

    let queued = ops.enqueue_task(&task.id).unwrap().unwrap();
    assert_eq!(queued.status, TaskStatus::InProgress);
    assert_eq!(queued.run_phase, Some(RunPhase::Queued));
    assert!(recorder.calls().is_empty(), "queueing never launches");
    // The queue note is on the thread for the audit trail.
    let thread = ThreadManager::new(dir.path())
        .expect("thread manager")
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|m| m.body.contains("waiting for a free agent slot"))
    );

    let dequeued = ops.dequeue_task(&task.id).unwrap().unwrap();
    assert_eq!(dequeued.run_phase, None);
    assert_eq!(dequeued.status, TaskStatus::InProgress);
    assert!(matches!(
        ops.dequeue_task(&task.id),
        Err(KanbanError::Invalid(_))
    ));

    // Review tasks queue too: the edits fold and the dispatcher re-runs it.
    ops.move_task(&task.id, "review", false).unwrap();
    ops.set_review_edits(&task.id, "Fold on enqueue").unwrap();
    let from_review = ops.enqueue_task(&task.id).unwrap().unwrap();
    assert_eq!(from_review.status, TaskStatus::InProgress);
    assert_eq!(from_review.run_phase, Some(RunPhase::Queued));
    assert_eq!(from_review.review_edits, "");
    assert!(recorder.calls().is_empty(), "queueing never launches");
}

// ------------------------------------------------------- per-task designer / reviewer

#[test]
fn start_uses_designer_when_only_the_task_opts_in() {
    let (_dir, ops, recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            title: "Plan just me".into(),
            agent_backend: Some("opencode".into()),
            use_designer: true,
            ..Default::default()
        })
        .unwrap();

    let session = ops.start_task(&task.id).unwrap().unwrap();
    assert!(session.starts_with("ses-claude-"), "{session}");
    let started = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(started.run_phase, Some(RunPhase::Design));
    assert_eq!(started.agent_backend.as_deref(), Some("opencode"));
    assert_eq!(recorder.calls().len(), 1);
    assert!(recorder.calls()[0].1.starts_with("ses-claude-"));
}

#[test]
fn executor_done_launches_reviewer_when_only_the_task_opts_in() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops
        .create_task(NewTask {
            title: "Review just me".into(),
            agent_backend: Some("opencode".into()),
            use_reviewer: true,
            ..Default::default()
        })
        .unwrap();
    ops.take_task(&task.id, "ses-exec", true).unwrap();
    finish_executor(&ops, dir.path(), &task.id, "ses-exec");

    let current = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(current.status, TaskStatus::InProgress);
    assert_eq!(current.run_phase, Some(RunPhase::Review));
    assert_eq!(current.review_rounds, 1);
    let session = current.session.clone().expect("reviewer session");
    assert!(session.starts_with("ses-claude-"), "{session}");
    let launches: Vec<_> = recorder
        .calls()
        .into_iter()
        .filter(|(id, _, _)| id == &task.id)
        .collect();
    assert_eq!(launches.len(), 2, "executor then reviewer: {launches:?}");
}

#[test]
fn update_task_can_toggle_per_task_bots() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Later")).unwrap();
    let updated = ops
        .update_task(
            &task.id,
            TaskPatch {
                use_designer: Some(true),
                use_reviewer: Some(true),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    assert!(updated.use_designer);
    assert!(updated.use_reviewer);
}

// ------------------------------------------------------- bot reviewer / verdict

fn write_reviewer_config(project: &Path, extra: &str) {
    let mut config = fs::read_to_string(project.join(".kanban/config.yaml")).unwrap();
    config.push_str("orchestration:\n  reviewer:\n    enabled: true\n");
    config.push_str(extra);
    fs::write(project.join(".kanban/config.yaml"), config).unwrap();
}

fn finish_executor(ops: &Operations, dir: &Path, task_id: &str, session: &str) {
    ContextManager::new(dir)
        .append_context(task_id, "implemented and tested", "agent", &ops.storage)
        .unwrap();
    ops.complete_task(task_id, session, true).unwrap().unwrap();
}

fn enter_bot_review(
    ops: &Operations,
    dir: &Path,
    title: &str,
) -> (kanban4ai::core::models::Task, String) {
    let task = ops
        .create_task(NewTask {
            title: title.into(),
            agent_backend: Some("opencode".into()),
            ..Default::default()
        })
        .unwrap();
    ops.take_task(&task.id, "ses-exec", true).unwrap();
    finish_executor(ops, dir, &task.id, "ses-exec");
    let current = ops.get_task(&task.id).unwrap().unwrap();
    let session = current.session.clone().expect("reviewer session");
    (current, session)
}

#[test]
fn executor_done_launches_reviewer_when_enabled() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    write_reviewer_config(dir.path(), "    backend: claude\n");
    ops.config.load_fresh().unwrap();

    let (task, session) = enter_bot_review(&ops, dir.path(), "Needs review");
    assert_eq!(task.status, TaskStatus::InProgress);
    assert_eq!(task.run_phase, Some(RunPhase::Review));
    assert_eq!(task.review_rounds, 1);
    assert_eq!(task.agent_backend.as_deref(), Some("opencode"));
    assert!(session.starts_with("ses-claude-"), "{session}");
    assert_eq!(task.completed_at, None);
    assert_ne!(task.status, TaskStatus::Review);

    let launches: Vec<_> = recorder
        .calls()
        .into_iter()
        .filter(|(id, _, _)| id == &task.id)
        .collect();
    assert_eq!(launches.len(), 2, "executor then reviewer: {launches:?}");
    assert_eq!(launches[0].1, "ses-exec");
    assert!(
        launches[1].1.starts_with("ses-claude-"),
        "{:?}",
        launches[1]
    );
}

#[test]
fn verdict_approve_moves_to_review_and_fires_chains() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    write_reviewer_config(dir.path(), "    backend: claude\n");
    ops.config.load_fresh().unwrap();

    let target = ops
        .create_task(NewTask {
            title: "Target".into(),
            agent_backend: Some("opencode".into()),
            ..Default::default()
        })
        .unwrap();
    let chained = ops
        .create_task(NewTask {
            title: "Chained after review".into(),
            chained_to: Some(target.id.clone()),
            ..Default::default()
        })
        .unwrap();
    ops.take_task(&target.id, "ses-exec", true).unwrap();
    finish_executor(&ops, dir.path(), &target.id, "ses-exec");
    let reviewer = ops.get_task(&target.id).unwrap().unwrap();
    let session = reviewer.session.clone().unwrap();

    let approved = ops
        .submit_verdict(&target.id, &session, true, Verdict::Approve)
        .unwrap()
        .unwrap();
    assert_eq!(approved.status, TaskStatus::Review);
    assert_eq!(approved.run_phase, None);
    assert!(approved.completed_at.is_some());
    assert!(approved.review_unseen);

    let chained_now = ops.get_task(&chained.id).unwrap().unwrap();
    assert_eq!(chained_now.status, TaskStatus::InProgress);
    assert!(
        recorder.calls().iter().any(|(id, _, _)| id == &chained.id),
        "approve must fire the existing chained-task path"
    );
}

#[test]
fn verdict_changes_todo_returns_to_todo() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    write_reviewer_config(
        dir.path(),
        "    backend: claude\n    on_changes_requested: todo\n",
    );
    ops.config.load_fresh().unwrap();

    let (task, session) = enter_bot_review(&ops, dir.path(), "Send back");
    let launches_before = recorder.calls().len();
    let returned = ops
        .submit_verdict(
            &task.id,
            &session,
            true,
            Verdict::Changes("please add tests".into()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(returned.status, TaskStatus::Todo);
    assert_eq!(returned.run_phase, None);
    assert!(returned.review_edits.is_empty());
    assert_eq!(
        recorder.calls().len(),
        launches_before,
        "todo route must not auto-launch"
    );

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|m| { m.kind == MessageKind::ReviewEdit && m.body.contains("please add tests") }),
        "changes must fold into the thread: {:?}",
        thread.messages
    );
}

#[test]
fn verdict_changes_in_progress_requeues_the_task_bot() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    write_reviewer_config(
        dir.path(),
        "    backend: claude\n    on_changes_requested: in_progress\n",
    );
    ops.config.load_fresh().unwrap();

    let (task, session) = enter_bot_review(&ops, dir.path(), "Bounce back");
    let bounced = ops
        .submit_verdict(
            &task.id,
            &session,
            true,
            Verdict::Changes("handle the empty list".into()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(bounced.status, TaskStatus::InProgress);
    assert_ne!(
        bounced.run_phase,
        Some(RunPhase::Review),
        "bounce must leave the reviewer phase"
    );
    assert_eq!(bounced.agent_backend.as_deref(), Some("opencode"));
    let bounce_session = bounced.session.clone().unwrap();
    assert!(
        bounce_session.starts_with("ses-opencode-"),
        "re-run must use the task bot, not the reviewer: {bounce_session}"
    );
    assert_ne!(bounce_session, session);

    let launches: Vec<_> = recorder
        .calls()
        .into_iter()
        .filter(|(id, _, _)| id == &task.id)
        .map(|(_, sid, _)| sid)
        .collect();
    assert_eq!(
        launches.len(),
        3,
        "executor, reviewer, executor: {launches:?}"
    );
    assert!(launches[1].starts_with("ses-claude-"), "{:?}", launches[1]);
    assert!(
        launches[2].starts_with("ses-opencode-"),
        "third launch is the task bot: {:?}",
        launches[2]
    );
}

#[test]
fn verdict_changes_exhausts_max_rounds_to_human_review() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    write_reviewer_config(
        dir.path(),
        "    backend: claude\n    max_rounds: 1\n    on_changes_requested: in_progress\n",
    );
    ops.config.load_fresh().unwrap();

    let (task, session) = enter_bot_review(&ops, dir.path(), "Last round");
    let launches_before = recorder.calls().len();
    let handed = ops
        .submit_verdict(
            &task.id,
            &session,
            true,
            Verdict::Changes("still not right".into()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(handed.status, TaskStatus::Review);
    assert_eq!(handed.run_phase, None);
    assert!(handed.completed_at.is_some());
    assert_eq!(
        recorder.calls().len(),
        launches_before,
        "exhausted budget must not relaunch"
    );
}

#[test]
fn reviewer_done_is_rejected() {
    let (dir, ops, _rec) = ops_with_recorder(true);
    write_reviewer_config(dir.path(), "    backend: claude\n");
    ops.config.load_fresh().unwrap();

    let (task, session) = enter_bot_review(&ops, dir.path(), "No done");
    let err = ops.complete_task(&task.id, &session, true).unwrap_err();
    assert!(
        err.to_string().contains("verdict"),
        "reviewer done must point at verdict: {err}"
    );
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.run_phase, Some(RunPhase::Review));
}

// -------------------------------------------- run-phase lifecycle regressions

fn park_in_phase(ops: &Operations, title: &str, phase: RunPhase) -> Task {
    let task = ops.create_task(NewTask::titled(title)).unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.status = TaskStatus::InProgress;
    current.run_phase = Some(phase);
    ops.storage.save_task(&current).unwrap();
    current
}

#[test]
fn recover_clears_the_run_phase() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = park_in_phase(&ops, "Recover me", RunPhase::Review);
    let recovered = ops.recover_task(&task.id).unwrap().unwrap();
    assert_eq!(recovered.status, TaskStatus::Todo);
    assert_eq!(recovered.run_phase, None, "recover must clear the phase");
}

#[test]
fn human_move_clears_the_run_phase() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = park_in_phase(&ops, "Move me", RunPhase::Design);
    let moved = ops.move_task(&task.id, "todo", false).unwrap().unwrap();
    assert_eq!(moved.run_phase, None, "a human move must clear the phase");
}

#[test]
fn agent_move_is_not_blocked_by_a_stale_run_phase() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = park_in_phase(&ops, "Stale", RunPhase::Review);
    ops.move_task(&task.id, "todo", false).unwrap();
    // An unrelated agent moving this task back should not be told it is the
    // reviewer of a task that is not even in a review phase any more.
    let moved = ops.move_task(&task.id, "in_progress", true).unwrap();
    assert!(
        moved.is_some(),
        "agent move must not be blocked by a stale phase"
    );
}

#[test]
fn delegated_take_clears_a_stale_run_phase() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = park_in_phase(&ops, "Take me", RunPhase::Review);
    let taken = ops
        .take_task(&task.id, "ses-take-1", true)
        .unwrap()
        .unwrap();
    assert_eq!(
        taken.run_phase, None,
        "a delegated take must not inherit a stale review phase"
    );
}

#[test]
fn crash_restart_is_not_scheduled_when_the_queue_cannot_start_it() {
    let (dir, ops, _rec) = ops_with_recorder(true);
    // Crash restart is implemented through the queue; with the queue off
    // nothing would ever pick the task back up.
    let mut config = fs::read_to_string(dir.path().join(".kanban/config.yaml")).unwrap();
    config.push_str("orchestration:\n  queue_enabled: false\n");
    fs::write(dir.path().join(".kanban/config.yaml"), config).unwrap();
    ops.config.load_fresh().unwrap();

    let task = park_in_phase(&ops, "Crashed", RunPhase::Execute);
    let session = "ses-crash-1";
    SessionManager::new(dir.path())
        .link_session(&task.id, session)
        .unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.session = Some(session.to_string());
    ops.storage.save_task(&current).unwrap();

    ops.reconcile_agent_exit(&task.id, session, 1).unwrap();

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(
        stored.restart_at, None,
        "no retry deadline may be promised while the dispatcher is off"
    );
    assert_ne!(
        stored.run_phase,
        Some(RunPhase::Queued),
        "the task must stay crashed and recoverable, not park in a dead queue"
    );
}

#[test]
fn recover_clears_the_designed_flag() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = park_in_phase(&ops, "Replan me", RunPhase::Execute);
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.designed = true;
    ops.storage.save_task(&current).unwrap();

    let recovered = ops.recover_task(&task.id).unwrap().unwrap();
    assert!(
        !recovered.designed,
        "a task sent back to To Do restarts from the top and plans again"
    );
}

#[test]
fn a_human_move_back_to_todo_clears_the_designed_flag() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = park_in_phase(&ops, "Drag me back", RunPhase::Execute);
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.designed = true;
    ops.storage.save_task(&current).unwrap();

    let moved = ops.move_task(&task.id, "todo", false).unwrap().unwrap();
    assert!(!moved.designed);
}

#[test]
fn a_human_move_that_is_not_to_todo_keeps_the_plan() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = park_in_phase(&ops, "Park in review", RunPhase::Execute);
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.designed = true;
    ops.storage.save_task(&current).unwrap();

    let moved = ops.move_task(&task.id, "review", false).unwrap().unwrap();
    assert!(
        moved.designed,
        "only a return to To Do discards the existing plan"
    );
}

#[test]
fn waking_a_crash_queued_task_does_not_keep_the_queued_phase() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = park_in_phase(&ops, "Dropped then restarted", RunPhase::Queued);
    let session = "ses-dropped";
    SessionManager::new(dir.path())
        .link_session(&task.id, session)
        .unwrap();
    SessionManager::new(dir.path())
        .crash_session(session)
        .unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.session = Some(session.to_string());
    ops.storage.save_task(&current).unwrap();

    let woken = ops
        .revoke_in_progress_task(&task.id, Some(session))
        .unwrap()
        .unwrap();

    assert_ne!(
        woken.run_phase,
        Some(RunPhase::Queued),
        "a live restart must leave the queue; queued on a running task is the badge bug"
    );
    assert_eq!(woken.run_phase, Some(RunPhase::Execute));
    assert_eq!(recorder.calls().len(), 1);
    assert!(woken.session.is_some_and(|id| id != session));
}

#[test]
fn auto_resume_of_a_queued_task_does_not_keep_the_queued_phase() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = park_in_phase(&ops, "Stranded queued", RunPhase::Queued);
    let session = "ses-queued-exit";
    SessionManager::new(dir.path())
        .link_session(&task.id, session)
        .unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.session = Some(session.to_string());
    ops.storage.save_task(&current).unwrap();
    recorder.calls.lock().unwrap().clear();

    let outcome = ops.reconcile_agent_exit(&task.id, session, 0).unwrap();
    assert!(
        matches!(outcome, AgentExitOutcome::Resumed(_)),
        "clean stranded exit should auto-resume, got {outcome:?}"
    );
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_ne!(
        stored.run_phase,
        Some(RunPhase::Queued),
        "auto-resume must not keep queued on the live successor"
    );
    assert_eq!(stored.run_phase, Some(RunPhase::Execute));
}

#[test]
fn rerunning_a_stranded_queued_task_does_not_keep_the_queued_phase() {
    let (dir, ops, _rec) = ops_with_recorder(true);
    let task = park_in_phase(&ops, "Rerun queued", RunPhase::Queued);
    let session = "ses-rerun-queued";
    SessionManager::new(dir.path())
        .link_session(&task.id, session)
        .unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.session = Some(session.to_string());
    ops.storage.save_task(&current).unwrap();
    SessionManager::new(dir.path())
        .close_session(session)
        .unwrap();

    let rerun = ops
        .rerun_in_progress_task(&task.id, None, RunMode::Immediate)
        .unwrap()
        .unwrap();
    assert_ne!(rerun.run_phase, Some(RunPhase::Queued));
    assert_eq!(rerun.run_phase, Some(RunPhase::Execute));
}

#[test]
fn waking_a_task_keeps_the_reviewer_bounce_count() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = park_in_phase(&ops, "Wake me", RunPhase::Execute);
    let session = "ses-wake-1";
    SessionManager::new(dir.path())
        .link_session(&task.id, session)
        .unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.session = Some(session.to_string());
    current.review_rounds = 2;
    current.crash_restarts = 1;
    ops.storage.save_task(&current).unwrap();
    // A closed session is what `wake` replaces; an active one needs a live
    // process to revoke.
    SessionManager::new(dir.path())
        .close_session(session)
        .unwrap();

    ops.revoke_in_progress_task(&task.id, Some(session))
        .unwrap();

    let woken = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(
        woken.review_rounds, 2,
        "a wake continues the same run; it must not re-arm the bounce cap"
    );
    assert_eq!(
        woken.crash_restarts, 0,
        "the crash budget is still reset by a human nudge"
    );
}

#[test]
fn rerunning_a_stranded_session_keeps_the_reviewer_bounce_count() {
    let (dir, ops, _rec) = ops_with_recorder(false);
    let task = park_in_phase(&ops, "Stranded", RunPhase::Execute);
    let session = "ses-stranded-1";
    SessionManager::new(dir.path())
        .link_session(&task.id, session)
        .unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.session = Some(session.to_string());
    current.review_rounds = 3;
    current.auto_resumes = 2;
    ops.storage.save_task(&current).unwrap();
    SessionManager::new(dir.path())
        .close_session(session)
        .unwrap();

    ops.rerun_in_progress_task(&task.id, None, RunMode::Immediate)
        .unwrap()
        .unwrap();

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.review_rounds, 3);
    assert_eq!(stored.auto_resumes, 0);
}

#[test]
fn rerunning_from_review_does_reset_the_reviewer_bounce_count() {
    let (_dir, ops, _rec) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Fresh attempt")).unwrap();
    let mut current = ops.get_task(&task.id).unwrap().unwrap();
    current.status = TaskStatus::Review;
    current.review_rounds = 3;
    ops.storage.save_task(&current).unwrap();

    ops.rerun_review_task(&task.id, None, RunMode::Immediate)
        .unwrap()
        .unwrap();

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(
        stored.review_rounds, 0,
        "a human restarting the work from Review is a fresh attempt"
    );
}

// ------------------------------------------------------------------ queue_run

/// Quiet board with auto-launch on, a recording launcher, and the given
/// orchestration body (the contents of the `orchestration:` mapping).
fn queue_board(
    orchestration_yaml: &str,
) -> (tempfile::TempDir, Operations, common::RecordingLauncher) {
    let (dir, _storage) = common::quiet_board(false);
    fs::write(
        dir.path().join(".kanban/config.yaml"),
        format!(
            "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n{orchestration_yaml}"
        ),
    )
    .unwrap();
    let recorder = common::RecordingLauncher::new();
    let ops = Operations::with_launcher(dir.path(), Box::new(recorder.clone()));
    (dir, ops, recorder)
}

#[test]
fn queue_run_moves_todo_to_queued_without_launching() {
    let (dir, ops, recorder) = queue_board("");
    let task = ops.create_task(NewTask::titled("Queue me")).unwrap();

    let queued = ops.queue_run(&task.id).unwrap().unwrap();
    assert_eq!(queued.status, TaskStatus::InProgress);
    assert_eq!(queued.run_phase, Some(RunPhase::Queued));
    assert_eq!(queued.session, None, "a queued task owns no session");
    assert!(recorder.calls().is_empty(), "queue_run must never launch");

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread.messages.iter().any(|m| m.body.contains("queued")),
        "the queue note must land on the thread"
    );
}

#[test]
fn queue_run_from_review_folds_review_edits_into_the_thread() {
    let (dir, ops, recorder) = queue_board("");
    let task = ops.create_task(NewTask::titled("Rework")).unwrap();
    ops.move_task(&task.id, "review", false).unwrap();
    ops.set_review_edits(&task.id, "Tighten validation")
        .unwrap();

    let queued = ops.queue_run(&task.id).unwrap().unwrap();
    assert_eq!(queued.status, TaskStatus::InProgress);
    assert_eq!(queued.run_phase, Some(RunPhase::Queued));
    assert_eq!(queued.review_edits, "");
    assert!(!queued.review_unseen);
    assert!(queued.session.is_none());
    assert!(recorder.calls().is_empty());

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|m| m.kind == MessageKind::ReviewEdit && m.body == "Tighten validation"),
        "the folded review edits must survive on the thread"
    );
}

#[test]
fn queue_run_rejects_done_tasks() {
    let (_dir, ops, recorder) = queue_board("");
    let task = ops.create_task(NewTask::titled("Finished")).unwrap();
    ops.move_task(&task.id, "done", false).unwrap();

    assert!(matches!(
        ops.queue_run(&task.id),
        Err(KanbanError::Invalid(_))
    ));
    assert!(recorder.calls().is_empty());
}

#[test]
fn queue_run_rejects_a_task_with_a_live_session() {
    let (_dir, ops, recorder) = queue_board("");
    let task = ops.create_task(NewTask::titled("Busy")).unwrap();
    ops.take_task(&task.id, "ses-live", true).unwrap().unwrap();
    recorder.calls.lock().unwrap().clear();

    assert!(matches!(
        ops.queue_run(&task.id),
        Err(KanbanError::Invalid(_))
    ));
    assert!(recorder.calls().is_empty());
}

#[test]
fn queue_can_dispatch_tracks_the_queue_and_auto_launch_switches() {
    let (_dir, ops, _recorder) = queue_board("");
    assert!(ops.queue_can_dispatch().unwrap());

    let (_dir, queue_off, _recorder) = queue_board("  queue_enabled: false\n");
    assert!(
        !queue_off.queue_can_dispatch().unwrap(),
        "queue_enabled: false must route runs to the direct launch"
    );

    let (dir, _storage) = common::quiet_board(false);
    let ops = Operations::new(dir.path());
    assert!(
        !ops.queue_can_dispatch().unwrap(),
        "auto-launch off leaves nothing to drain the queue"
    );
}

#[test]
fn rerun_review_task_in_queued_mode_does_not_launch() {
    let (dir, ops, recorder) = queue_board("");
    let task = ops.create_task(NewTask::titled("Queue rerun")).unwrap();
    ops.move_task(&task.id, "review", false).unwrap();
    ops.set_review_edits(&task.id, "Fold me").unwrap();

    let queued = ops
        .rerun_review_task(&task.id, None, RunMode::Queued)
        .unwrap()
        .unwrap();
    assert_eq!(queued.status, TaskStatus::InProgress);
    assert_eq!(queued.run_phase, Some(RunPhase::Queued));
    assert!(queued.session.is_none());
    assert_eq!(queued.review_edits, "");
    assert!(recorder.calls().is_empty());

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|m| m.kind == MessageKind::ReviewEdit && m.body == "Fold me"),
        "queued re-run folds the edits exactly like the immediate one"
    );
}

#[test]
fn rerun_in_progress_task_in_queued_mode_parks_the_task_without_a_session() {
    let (dir, ops, recorder) = queue_board("");
    let task = ops.create_task(NewTask::titled("Stalled queue")).unwrap();
    ops.take_task(&task.id, "ses-dead", true).unwrap();
    recorder.calls.lock().unwrap().clear();
    SessionManager::new(dir.path())
        .crash_session("ses-dead")
        .unwrap();

    let queued = ops
        .rerun_in_progress_task(&task.id, None, RunMode::Queued)
        .unwrap()
        .unwrap();
    assert_eq!(queued.run_phase, Some(RunPhase::Queued));
    assert_eq!(queued.session, None, "the queued run owns no session yet");
    assert!(
        !SessionManager::new(dir.path()).is_session_active("ses-dead"),
        "the old session must still be closed"
    );
    assert!(recorder.calls().is_empty());

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|m| m.body.contains("re-run from In Progress")),
        "the re-run audit note must survive queued mode"
    );
    assert!(
        thread
            .messages
            .iter()
            .any(|m| m.body.contains("queued — the dispatcher starts it")),
        "the thread must say the new run waits in the queue"
    );
}
