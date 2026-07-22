//! Thread compatibility and merge behavior from the earlier implementation.

mod common;

use common::temp_board;
use kanban4ai::core::models::{Message, MessageKind, MessageRole, MessageStatus};
use kanban4ai::core::storage::NewTask;
use kanban4ai::core::thread::ThreadManager;

fn setup() -> (tempfile::TempDir, ThreadManager, String) {
    let (dir, storage) = temp_board();
    let task = storage
        .create_task(NewTask {
            title: "Threaded".into(),
            description: "body".into(),
            ..Default::default()
        })
        .unwrap();
    let manager = ThreadManager::new(dir.path()).unwrap();
    (dir, manager, task.id)
}

#[test]
fn post_question_and_answer_flow() {
    let (_dir, manager, task_id) = setup();

    let question = manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Question,
            "JWT or cookies?",
            None,
            vec!["JWT".into(), "Cookies".into()],
            Some("opencode".into()),
        )
        .unwrap();
    assert_eq!(question.id, "MSG-003"); // MSG-001/002 are system/task
    assert_eq!(question.status, MessageStatus::Open);
    assert!(manager.has_open_questions(&task_id).unwrap());

    let answered = manager
        .answer(&task_id, &question.id, "JWT", MessageRole::Human)
        .unwrap();
    assert_eq!(answered.status, MessageStatus::Answered);
    assert_eq!(answered.answer.as_deref(), Some("JWT"));
    assert_eq!(answered.answered_by_role, Some(MessageRole::Human));
    assert!(answered.resolved_at.is_some());
    assert!(!manager.has_open_questions(&task_id).unwrap());
}

#[test]
fn saved_thread_quotes_timestamps_for_legacy_python_yaml() {
    let (dir, manager, task_id) = setup();

    let question = manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Question,
            "JWT or cookies?",
            None,
            vec![],
            Some("opencode".into()),
        )
        .unwrap();
    manager
        .answer(&task_id, &question.id, "JWT", MessageRole::Human)
        .unwrap();

    let raw = std::fs::read_to_string(
        dir.path()
            .join(".kanban")
            .join("threads")
            .join(format!("{task_id}.yaml")),
    )
    .unwrap();

    assert!(raw.contains("  created_at: '"));
    assert!(raw.contains("  updated_at: '"));
    assert!(raw.contains("  resolved_at: '"));
    assert!(!raw.contains("  created_at: 20"));
    assert!(!raw.contains("  updated_at: 20"));
    assert!(!raw.contains("  resolved_at: 20"));

    let reloaded = manager.load(&task_id).unwrap();
    assert_eq!(reloaded.messages.len(), 3);
    assert_eq!(reloaded.messages[2].status, MessageStatus::Answered);
}

#[test]
fn message_without_origin_omits_origin_from_yaml() {
    let message = Message::new(
        "MSG-001",
        MessageRole::Agent,
        MessageKind::Context,
        "implementation detail",
    );

    let yaml = serde_yaml_ng::to_string(&message).unwrap();
    assert!(!yaml.contains("origin:"));
}

#[test]
fn open_messages_defaults_to_questions_and_suggestions() {
    let (_dir, manager, task_id) = setup();
    manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Question,
            "q",
            None,
            vec![],
            None,
        )
        .unwrap();
    manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Suggestion,
            "s",
            None,
            vec![],
            None,
        )
        .unwrap();
    manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Context,
            "ctx",
            None,
            vec![],
            None,
        )
        .unwrap();

    let open = manager.open_messages(&task_id, None).unwrap();
    assert_eq!(open.len(), 2);
    assert!(
        open.iter()
            .all(|m| matches!(m.kind, MessageKind::Question | MessageKind::Suggestion))
    );

    let contexts = manager
        .messages_of_kind(&task_id, MessageKind::Context)
        .unwrap();
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].body, "ctx");
}

#[test]
fn resolve_marks_suggestion_resolved() {
    let (_dir, manager, task_id) = setup();
    let suggestion = manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Suggestion,
            "idea",
            None,
            vec![],
            None,
        )
        .unwrap();

    let resolved = manager
        .resolve(&task_id, &suggestion.id, MessageStatus::Resolved)
        .unwrap();
    assert_eq!(resolved.status, MessageStatus::Resolved);
    assert!(resolved.resolved_at.is_some());

    let reopened = manager
        .resolve(&task_id, &suggestion.id, MessageStatus::Open)
        .unwrap();
    assert_eq!(reopened.status, MessageStatus::Open);
    assert!(reopened.resolved_at.is_none());
}

#[test]
fn resolve_rejects_and_restores_a_context_message() {
    let (_dir, manager, task_id) = setup();
    let context = manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Context,
            "possibly poisoned note",
            None,
            vec![],
            None,
        )
        .unwrap();
    assert_eq!(context.status, MessageStatus::Open);

    let rejected = manager
        .resolve(&task_id, &context.id, MessageStatus::Rejected)
        .unwrap();
    assert_eq!(rejected.status, MessageStatus::Rejected);
    assert!(rejected.resolved_at.is_some());

    let restored = manager
        .resolve(&task_id, &context.id, MessageStatus::Open)
        .unwrap();
    assert_eq!(restored.status, MessageStatus::Open);
    assert!(restored.resolved_at.is_none());
}

#[test]
fn concurrent_saves_merge_instead_of_clobbering() {
    let (_dir, manager, task_id) = setup();

    // writer A loads the thread, then writer B posts first
    let mut stale = manager.load(&task_id).unwrap();
    manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Question,
            "from B",
            None,
            vec![],
            None,
        )
        .unwrap();

    // writer A appends its own message to the stale snapshot and saves
    let mut mine = Message::new(
        "MSG-010",
        MessageRole::Human,
        MessageKind::Suggestion,
        "from A",
    );
    mine.author = Some("user".into());
    stale.messages.push(mine);
    manager.save(&task_id, &mut stale).unwrap();

    let merged = manager.load(&task_id).unwrap();
    let bodies: Vec<&str> = merged.messages.iter().map(|m| m.body.as_str()).collect();
    assert!(bodies.contains(&"from B"), "B's message must survive");
    assert!(bodies.contains(&"from A"), "A's message must survive");
}

#[test]
fn stale_writer_does_not_clobber_concurrent_answer() {
    let (_dir, manager, task_id) = setup();
    let question = manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Question,
            "q",
            None,
            vec![],
            None,
        )
        .unwrap();

    // writer A loads while the question is still open
    let mut stale = manager.load(&task_id).unwrap();
    // writer B answers it
    manager
        .answer(&task_id, &question.id, "yes", MessageRole::Human)
        .unwrap();

    // writer A saves without touching the question
    stale.messages.push(Message::new(
        "MSG-020",
        MessageRole::Agent,
        MessageKind::Context,
        "progress",
    ));
    manager.save(&task_id, &mut stale).unwrap();

    let final_question = manager
        .get_message(&task_id, &question.id)
        .unwrap()
        .unwrap();
    assert_eq!(final_question.status, MessageStatus::Answered);
    assert_eq!(final_question.answer.as_deref(), Some("yes"));
}

#[test]
fn update_task_message_tracks_description() {
    let (dir, manager, task_id) = setup();
    let storage = kanban4ai::core::storage::Storage::new(dir.path());
    let mut task = storage.load_task(&task_id).unwrap().unwrap();
    task.description = "rewritten body".into();
    storage.save_task(&task).unwrap();

    let thread = manager.load(&task_id).unwrap();
    let task_msg = thread
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::Task)
        .unwrap();
    assert_eq!(task_msg.body, "rewritten body");
}

#[test]
fn remove_kind_deletes_and_bumps_rev() {
    let (dir, manager, task_id) = setup();
    manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Context,
            "c1",
            None,
            vec![],
            None,
        )
        .unwrap();
    manager
        .post(
            &task_id,
            MessageRole::Agent,
            MessageKind::Context,
            "c2",
            None,
            vec![],
            None,
        )
        .unwrap();
    let before = manager.load(&task_id).unwrap();

    let removed = manager.remove_kind(&task_id, MessageKind::Context).unwrap();
    assert_eq!(removed, 2);

    let after = manager.load(&task_id).unwrap();
    assert_eq!(after.rev, before.rev + 1);
    assert!(
        manager
            .messages_of_kind(&task_id, MessageKind::Context)
            .unwrap()
            .is_empty()
    );
    // untouched kinds survive
    assert_eq!(after.messages.len(), before.messages.len() - 2);

    let raw = std::fs::read_to_string(
        dir.path()
            .join(".kanban")
            .join("threads")
            .join(format!("{task_id}.yaml")),
    )
    .unwrap();
    assert!(raw.contains("  created_at: '"));
    assert!(raw.contains("  updated_at: '"));
    assert!(!raw.contains("  created_at: 20"));
    assert!(!raw.contains("  updated_at: 20"));

    assert_eq!(
        manager.remove_kind(&task_id, MessageKind::Context).unwrap(),
        0
    );
}

#[test]
fn missing_thread_loads_empty() {
    let (_dir, manager, _task_id) = setup();
    let thread = manager.load("TASK-404").unwrap();
    assert_eq!(thread.task_id, "TASK-404");
    assert_eq!(thread.rev, 0);
    assert!(thread.messages.is_empty());
}
