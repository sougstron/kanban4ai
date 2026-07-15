//! Compatibility tests for agent rules, questions, review edits, and chaining.

mod common;

use common::ops_with_recorder;
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::error::KanbanError;
use kanban4ai::core::models::{MessageKind, MessageRole, MessageStatus, Task, TaskStatus};
use kanban4ai::core::operations::{
    AgentExitOutcome, AgentLauncher, NoopLauncher, Operations, QuestionRef, TaskPatch,
};
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
    fn launch(&self, _project: &Path, _task: &Task, _session_id: &str, _revert: bool) -> bool {
        false
    }
}

struct MoveThenFailLauncher {
    project: PathBuf,
}

impl AgentLauncher for MoveThenFailLauncher {
    fn launch(&self, _project: &Path, task: &Task, _session_id: &str, _revert: bool) -> bool {
        Operations::with_launcher(&self.project, Box::new(NoopLauncher))
            .move_task(&task.id, "review", false)
            .unwrap();
        false
    }
}

#[test]
fn failed_agent_launch_rolls_back_take_assignment() {
    let (dir, _storage) = common::quiet_board(true);
    let ops = Operations::with_launcher(dir.path(), Box::new(FailingLauncher));
    let task = ops.create_task(NewTask::titled("Will fail")).unwrap();

    assert!(ops.take_task(&task.id, "ses-fail", true).unwrap().is_none());

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Todo);
    assert_eq!(stored.session, None);
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

    assert!(ops.rerun_review_task(&task.id, None).unwrap().is_none());

    let stored = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Review);
    assert_eq!(stored.session, None);
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
        ops.rerun_review_task(&task.id, Some("../escape")),
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
        ops.rerun_in_progress_task(&task.id, Some("../escape")),
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
fn recover_task_moves_to_todo_and_clears_session() {
    let (_dir, ops, _recorder) = ops_with_recorder(false);
    let task = ops.create_task(NewTask::titled("Recover me")).unwrap();
    ops.take_task(&task.id, "ses-stale", false)
        .unwrap()
        .unwrap();

    let recovered = ops.recover_task(&task.id).unwrap().unwrap();
    assert_eq!(recovered.status, TaskStatus::Todo);
    assert_eq!(recovered.session, None);
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
    assert_eq!(reviewed.session, None);
    assert!(!SessionManager::new(dir.path()).is_session_active("ses-flow"));

    // a second agent done from Review is refused
    assert!(
        ops.complete_task(&task.id, "ses-flow", true)
            .unwrap()
            .is_none()
    );
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
    assert_eq!(done.session, None);
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
    assert!(!answered.has_questions);
    assert!(ops.list_open_messages(&task.id).unwrap().is_empty());
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

    let calls = recorder.calls();
    assert_eq!(calls.len(), 1, "agent must be relaunched");
    assert_eq!(calls[0].0, task.id);
    assert!(calls[0].1.starts_with("ses-opencode-"));
    assert!(answered.session.is_some());
    assert_eq!(
        SessionManager::new(_dir.path())
            .load_session(&calls[0].1)
            .unwrap()
            .name,
        Some(task.title)
    );
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
    assert_eq!(stored.session, None);
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

    let rerun = ops.rerun_review_task(&task.id, None).unwrap().unwrap();
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

    let rerun = ops.rerun_in_progress_task(&task.id, None).unwrap().unwrap();
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
        ops.rerun_in_progress_task(&task.id, None)
            .unwrap()
            .is_none()
    );
    assert!(recorder.calls().is_empty());
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
fn expired_declared_wait_relaunches_agent_to_check_result() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Check later")).unwrap();
    ops.take_task(&task.id, "ses-wait-old", true)
        .unwrap()
        .unwrap();
    ops.declare_waiting(&task.id, "ses-wait-old", Some(10), Some("batch export"))
        .unwrap();
    ops.reconcile_agent_exit(&task.id, "ses-wait-old", 0)
        .unwrap();
    recorder.calls.lock().unwrap().clear();
    let session_mgr = SessionManager::new(dir.path());
    let mut session = session_mgr.load_session("ses-wait-old").unwrap();
    session.wait_until = Some(timefmt::now() - chrono::Duration::seconds(1));
    session_mgr.save_session(&session).unwrap();

    let resumed = ops.resume_expired_waits().unwrap();

    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].0, task.id);
    let new_session = resumed[0].1.clone();
    assert_eq!(
        recorder.calls(),
        vec![(task.id.clone(), new_session.clone(), false)]
    );
    assert!(!session_mgr.is_session_active("ses-wait-old"));
    assert!(session_mgr.is_session_active(&new_session));
    assert_eq!(
        ops.get_task(&task.id).unwrap().unwrap().session.as_deref(),
        Some(new_session.as_str())
    );
    let contexts = ThreadManager::new(dir.path())
        .unwrap()
        .messages_of_kind(&task.id, MessageKind::Context)
        .unwrap();
    assert!(contexts.iter().any(|message| {
        message.body.contains("Waiting deadline passed")
            && message.body.contains("batch export")
            && message.body.contains("declare waiting again")
    }));
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

    let resumed = ops.resume_expired_waits().unwrap();

    assert!(resumed.is_empty());
    assert!(!session_mgr.is_session_active("ses-wait-fail"));
    let stored = ops.get_task(&task.id).unwrap().unwrap();
    let new_session = stored.session.expect("failed relaunch session persisted");
    assert_ne!(new_session, "ses-wait-fail");
    assert_eq!(
        session_mgr.session_state(&new_session, 300),
        Some(SessionState::Crashed)
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

    let resumed = ops.resume_expired_waits().unwrap();

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

    let resumed = ops.resume_expired_waits().unwrap();

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
    assert_eq!(stored.session, None);
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
fn stop_session_closes_session_and_detaches_task() {
    let (dir, ops, _recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Stop me")).unwrap();
    ops.take_task(&task.id, "ses-stop", true).unwrap().unwrap();

    let stopped = ops.stop_session("ses-stop").unwrap().unwrap();
    assert_eq!(stopped.session, None);
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
