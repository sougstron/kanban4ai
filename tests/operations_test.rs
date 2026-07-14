//! Compatibility tests for agent rules, questions, review edits, and chaining.

mod common;

use common::ops_with_recorder;
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::error::KanbanError;
use kanban4ai::core::models::{MessageKind, MessageRole, MessageStatus, Task, TaskStatus};
use kanban4ai::core::operations::{
    AgentLauncher, NoopLauncher, Operations, QuestionRef, TaskPatch,
};
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::storage::NewTask;
use kanban4ai::core::thread::ThreadManager;
use kanban4ai::core::timefmt;
use std::path::{Path, PathBuf};

#[test]
fn agent_take_moves_to_in_progress_and_links_session() {
    let (dir, ops, recorder) = ops_with_recorder(true);
    let task = ops.create_task(NewTask::titled("Delegate me")).unwrap();

    let taken = ops.take_task(&task.id, "ses-1", true).unwrap().unwrap();
    assert_eq!(taken.status, TaskStatus::InProgress);
    assert_eq!(taken.session.as_deref(), Some("ses-1"));
    assert!(SessionManager::new(dir.path()).is_session_active("ses-1"));
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
