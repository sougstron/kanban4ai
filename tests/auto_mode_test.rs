//! Auto mode (store-wide `auto_mode:`) and the merge-conflict queue hold.

mod common;

use std::fs;
use std::path::Path;

use kanban4ai::core::auto_mode::AUTO_DECIDE_ANSWER;
use kanban4ai::core::config::Config;
use kanban4ai::core::error::KanbanError;
use kanban4ai::core::models::{
    IntegrationState, MessageKind, MessageRole, MessageStatus, RunPhase, TaskStatus,
};
use kanban4ai::core::operations::Operations;
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::storage::NewTask;
use kanban4ai::core::thread::ThreadManager;

/// A queue-enabled board whose global config is `global_yaml`.
fn board(
    orchestration_yaml: &str,
    global_yaml: &str,
) -> (tempfile::TempDir, Operations, common::RecordingLauncher) {
    let (dir, _storage) = common::quiet_board(true);
    fs::write(
        dir.path().join(".kanban/config.yaml"),
        format!(
            "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  queue_enabled: true\n{orchestration_yaml}"
        ),
    )
    .unwrap();
    let global = dir.path().join("global.yaml");
    fs::write(&global, global_yaml).unwrap();
    let recorder = common::RecordingLauncher::new();
    let mut ops = Operations::with_launcher(dir.path(), Box::new(recorder.clone()));
    ops.config = Config::new(dir.path()).with_global_file(Some(global));
    (dir, ops, recorder)
}

const AUTO_DECIDE: &str = "auto_mode:\n  enabled: true\n  questions: decide\n";
const AUTO_ARBITER: &str = "auto_mode:\n  enabled: true\n  questions: arbiter\n";

fn set_integration(ops: &Operations, task_id: &str, state: IntegrationState) {
    let mut task = ops.storage.load_task(task_id).unwrap().unwrap();
    task.integration = state;
    ops.storage.save_task(&task).unwrap();
}

fn thread_bodies(dir: &Path, task_id: &str) -> Vec<String> {
    ThreadManager::new(dir)
        .unwrap()
        .load(task_id)
        .unwrap()
        .messages
        .into_iter()
        .map(|m| m.body)
        .collect()
}

#[test]
fn conflict_holds_new_queue_entries_until_resolved() {
    let (dir, ops, recorder) = board("", "");
    let conflicted = ops
        .create_task_in_status(NewTask::titled("Conflicted"), TaskStatus::Review)
        .unwrap();
    set_integration(&ops, &conflicted.id, IntegrationState::Conflict);
    let task = ops.create_task(NewTask::titled("New work")).unwrap();
    ops.queue_run(&task.id).unwrap();

    assert!(ops.dispatch_queue().unwrap().is_empty());
    let held = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert!(held.conflict_held);
    assert_eq!(held.run_phase, Some(RunPhase::Queued));
    assert!(recorder.calls().is_empty());
    assert!(
        thread_bodies(dir.path(), &task.id)
            .iter()
            .any(|b| b.contains("queue held") && b.contains(&conflicted.id))
    );

    set_integration(&ops, &conflicted.id, IntegrationState::Landed);
    let dispatched = ops.dispatch_queue().unwrap();
    assert_eq!(dispatched.len(), 1);
    let started = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert!(!started.conflict_held);
    assert!(started.queued_at.is_none());
}

#[test]
fn hold_stays_on_while_runs_that_started_before_are_still_live() {
    let (_dir, ops, _recorder) = board("", "");
    let running = ops.create_task(NewTask::titled("Running")).unwrap();
    ops.queue_run(&running.id).unwrap();
    assert_eq!(ops.dispatch_queue().unwrap().len(), 1);

    let conflicted = ops
        .create_task_in_status(NewTask::titled("Conflicted"), TaskStatus::Review)
        .unwrap();
    set_integration(&ops, &conflicted.id, IntegrationState::Conflict);
    let waiting = ops.create_task(NewTask::titled("Waiting")).unwrap();
    ops.queue_run(&waiting.id).unwrap();
    assert!(ops.dispatch_queue().unwrap().is_empty());

    // Conflict resolved, but the earlier run is still live: keep holding.
    set_integration(&ops, &conflicted.id, IntegrationState::Landed);
    assert!(ops.dispatch_queue().unwrap().is_empty());
    assert!(ops.conflict_hold().unwrap().is_some());

    let session = ops.storage.load_task(&running.id).unwrap().unwrap().session;
    SessionManager::new(&ops.storage.project_path)
        .close_session(session.as_deref().unwrap())
        .unwrap();
    let dispatched = ops.dispatch_queue().unwrap();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].task_id, waiting.id);
}

#[test]
fn conflict_mode_outlives_a_conflict_resolved_while_nothing_was_queued() {
    let (dir, ops, _recorder) = board("", "");
    let running = ops.create_task(NewTask::titled("Running")).unwrap();
    ops.queue_run(&running.id).unwrap();
    assert_eq!(ops.dispatch_queue().unwrap().len(), 1);

    // A conflict comes and goes while the queue is empty.
    let conflicted = ops
        .create_task_in_status(NewTask::titled("Conflicted"), TaskStatus::Review)
        .unwrap();
    set_integration(&ops, &conflicted.id, IntegrationState::Conflict);
    assert!(ops.dispatch_queue().unwrap().is_empty());
    set_integration(&ops, &conflicted.id, IntegrationState::Landed);

    // The earlier run is still live, so new work still waits.
    let waiting = ops.create_task(NewTask::titled("Waiting")).unwrap();
    ops.queue_run(&waiting.id).unwrap();
    assert!(ops.dispatch_queue().unwrap().is_empty());
    assert!(
        ops.storage
            .load_task(&waiting.id)
            .unwrap()
            .unwrap()
            .conflict_held
    );

    let session = ops.storage.load_task(&running.id).unwrap().unwrap().session;
    SessionManager::new(&ops.storage.project_path)
        .close_session(session.as_deref().unwrap())
        .unwrap();
    let dispatched = ops.dispatch_queue().unwrap();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].task_id, waiting.id);
    assert!(!dir.path().join(".kanban/conflict_mode").exists());
}

#[test]
fn held_tasks_release_first_in_first_out() {
    let (_dir, ops, _recorder) = board("  max_running_total: 1\n", "");
    let conflicted = ops
        .create_task_in_status(NewTask::titled("Conflicted"), TaskStatus::Review)
        .unwrap();
    set_integration(&ops, &conflicted.id, IntegrationState::Conflict);
    let first = ops.create_task(NewTask::titled("First")).unwrap();
    let second = ops.create_task(NewTask::titled("Second")).unwrap();
    // Queue in reverse id order: FIFO must beat the board's id sort.
    ops.queue_run(&second.id).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    ops.queue_run(&first.id).unwrap();
    assert!(ops.dispatch_queue().unwrap().is_empty());

    set_integration(&ops, &conflicted.id, IntegrationState::Landed);
    let dispatched = ops.dispatch_queue().unwrap();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].task_id, second.id);
}

#[test]
fn a_conflicted_task_itself_is_never_held() {
    let (_dir, ops, _recorder) = board("", "");
    let conflicted = ops
        .create_task_in_status(NewTask::titled("Conflicted"), TaskStatus::Review)
        .unwrap();
    set_integration(&ops, &conflicted.id, IntegrationState::Conflict);
    ops.queue_run(&conflicted.id).unwrap();
    assert_eq!(ops.dispatch_queue().unwrap().len(), 1);
}

#[test]
fn auto_decide_refuses_agent_questions_and_keeps_human_ones() {
    let (_dir, ops, _recorder) = board("", AUTO_DECIDE);
    let task = ops.create_task(NewTask::titled("Auto")).unwrap();
    let err = ops
        .ask_question(&task.id, "Which way?", "agent", vec![])
        .unwrap_err();
    assert!(matches!(err, KanbanError::Permission(ref m) if m.contains("auto mode")));
    // A human note-to-self is not an agent question.
    assert!(
        ops.ask_question(&task.id, "Remember X", "human", vec![])
            .unwrap()
            .is_some()
    );
}

#[test]
fn switching_auto_mode_on_answers_questions_already_open() {
    let (dir, mut ops, _recorder) = board("", "");
    fs::write(
        dir.path().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\nrules:\n  questions_go_to_review: true\norchestration:\n  queue_enabled: true\n",
    )
    .unwrap();
    ops.config = Config::new(dir.path()).with_global_file(Some(dir.path().join("global.yaml")));
    let task = ops.create_task(NewTask::titled("Asked")).unwrap();
    ops.queue_run(&task.id).unwrap();
    ops.dispatch_queue().unwrap();
    let session = ops.storage.load_task(&task.id).unwrap().unwrap().session;
    ops.ask_question(&task.id, "Which way?", "agent", vec![])
        .unwrap();
    SessionManager::new(dir.path())
        .close_session(session.as_deref().unwrap())
        .unwrap();
    let parked = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(parked.status, TaskStatus::Review, "questions_go_to_review");

    let global = dir.path().join("global.yaml");
    fs::write(&global, AUTO_DECIDE).unwrap();
    ops.config = Config::new(dir.path()).with_global_file(Some(global));
    ops.dispatch_queue().unwrap();

    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let question = thread
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::Question)
        .unwrap();
    assert_eq!(question.status, MessageStatus::Answered);
    assert_eq!(question.answer.as_deref(), Some(AUTO_DECIDE_ANSWER));
    assert_eq!(question.answered_by_role, Some(MessageRole::System));
    let resumed = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(
        resumed.status,
        TaskStatus::InProgress,
        "resumed from Review"
    );
}

#[test]
fn arbiter_mode_keeps_the_task_running_and_falls_back_without_an_arbiter() {
    let (dir, ops, _recorder) = board("", AUTO_ARBITER);
    let task = ops.create_task(NewTask::titled("Arbitrated")).unwrap();
    ops.queue_run(&task.id).unwrap();
    ops.dispatch_queue().unwrap();
    let asked = ops
        .ask_question(
            &task.id,
            "Which way?",
            "agent",
            vec!["a".into(), "b".into()],
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        asked.status,
        TaskStatus::InProgress,
        "auto mode never parks in Review"
    );
    // The recording launcher starts no arbiter, so the decide answer lands.
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let question = thread
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::Question)
        .unwrap();
    assert_eq!(question.answer.as_deref(), Some(AUTO_DECIDE_ANSWER));
    assert!(
        thread_bodies(dir.path(), &task.id)
            .iter()
            .any(|b| b.contains("arbiter could not start"))
    );
}

#[test]
fn auto_mode_reruns_a_conflicted_review_task_within_budget() {
    let global = "auto_mode:\n  enabled: true\n  max_resolver_runs: 1\n";
    let (_dir, ops, recorder) = board("", global);
    let task = ops
        .create_task_in_status(NewTask::titled("Conflicted"), TaskStatus::Review)
        .unwrap();
    set_integration(&ops, &task.id, IntegrationState::Conflict);

    ops.dispatch_queue().unwrap();
    let rerun = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(rerun.status, TaskStatus::InProgress);
    assert_eq!(rerun.resolver_runs, 1);
    assert_eq!(recorder.calls().len(), 1);

    // Back in Review, still conflicted, budget spent: left for the human.
    let session = rerun.session.clone().unwrap();
    SessionManager::new(&ops.storage.project_path)
        .close_session(&session)
        .unwrap();
    let mut back = ops.storage.load_task(&task.id).unwrap().unwrap();
    back.status = TaskStatus::Review;
    ops.storage.save_task(&back).unwrap();
    ops.dispatch_queue().unwrap();
    assert_eq!(recorder.calls().len(), 1);
}

#[test]
fn without_auto_mode_conflicted_review_tasks_wait_for_the_human() {
    let (_dir, ops, recorder) = board("", "");
    let task = ops
        .create_task_in_status(NewTask::titled("Conflicted"), TaskStatus::Review)
        .unwrap();
    set_integration(&ops, &task.id, IntegrationState::Conflict);
    ops.dispatch_queue().unwrap();
    assert!(recorder.calls().is_empty());
    let task = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Review);
}
