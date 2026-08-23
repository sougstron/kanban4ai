//! Golden-file compatibility: files written by the original Python
//! implementation must load losslessly and survive a Rust rewrite cycle.

mod common;

use std::fs;

use common::{copy_fixture, fixtures_dir, temp_board};
use kanban4ai::core::config::Config;
use kanban4ai::core::models::{
    MessageKind, MessageRole, MessageStatus, RunPhase, Session, SessionStatus, Task, TaskStatus,
};
use kanban4ai::core::storage::Storage;
use kanban4ai::core::thread::ThreadManager;
use kanban4ai::core::timefmt;

#[test]
fn parses_python_written_task_with_agent_fields() {
    let (dir, storage) = temp_board();
    let dest = dir.path().join(".kanban/tasks/review/TASK-085.md");
    copy_fixture("tasks/TASK-085.md", &dest);

    let task = storage.load_task("TASK-085").unwrap().unwrap();
    assert_eq!(task.id, "TASK-085");
    assert_eq!(task.title, "Sonnet 5 update");
    assert_eq!(task.status, TaskStatus::Review);
    assert_eq!(task.session, None);
    assert_eq!(task.ai_model.as_deref(), Some("opus"));
    assert_eq!(task.agent_backend.as_deref(), Some("claude"));
    assert!(task.interactive);
    assert_eq!(task.context_size, 2840);
    assert_eq!(task.review_edits, "");
    assert_eq!(
        timefmt::format(&task.created_at),
        "2026-07-01T10:13:22.036493"
    );
    assert!(task.description.starts_with("Is model in opencode"));
}

#[test]
fn parses_python_written_task_with_chain() {
    let (dir, storage) = temp_board();
    copy_fixture(
        "tasks/TASK-084.md",
        &dir.path().join(".kanban/tasks/review/TASK-084.md"),
    );

    let task = storage.load_task("TASK-084").unwrap().unwrap();
    assert_eq!(task.chained_to.as_deref(), Some("TASK-083"));
    assert_eq!(task.agent_backend, None);
    assert_eq!(task.context_size, 392);
    assert_eq!(
        task.description,
        "Make available collapsable version when it contains 4 rows instead of 3"
    );
}

#[test]
fn task_fixtures_round_trip_losslessly() {
    for entry in fs::read_dir(fixtures_dir().join("tasks")).unwrap() {
        let path = entry.unwrap().path();
        let (dir, storage) = temp_board();
        let original = storage.parse_task_file(&path).unwrap();

        storage.save_task(&original).unwrap();
        let reloaded = storage.load_task(&original.id).unwrap().unwrap();
        assert_eq!(original, reloaded, "round-trip mismatch for {path:?}");
        drop(dir);
    }
}

#[test]
fn legacy_task_without_run_phase_round_trips_byte_identically() {
    let (dir, storage) = temp_board();
    let mut task = Task::new("TASK-090", "Legacy in-progress task");
    task.status = TaskStatus::InProgress;
    task.session = Some("ses-opencode-20260823-120000-000001".to_string());
    task.ai_model = Some("openai/gpt-5.5".to_string());
    task.interactive = true;

    let path = dir.path().join(".kanban/tasks/in_progress/TASK-090.md");
    storage.save_task(&task).unwrap();
    let first = fs::read_to_string(&path).unwrap();

    // The orchestration fields stay out of frontmatter that never carried
    // them — a board written before TASK-222 is rewritten unchanged.
    assert!(!first.contains("run_phase"), "{first}");
    assert!(!first.contains("crash_restarts"), "{first}");
    assert!(!first.contains("restart_at"), "{first}");
    assert!(!first.contains("review_rounds"), "{first}");

    let reparsed = storage.parse_task_file(&path).unwrap();
    assert_eq!(reparsed, task);
    assert_eq!(reparsed.run_phase, None);
    assert_eq!(reparsed.crash_restarts, 0);
    assert_eq!(reparsed.restart_at, None);

    storage.save_task(&reparsed).unwrap();
    let second = fs::read_to_string(&path).unwrap();
    assert_eq!(first, second);
}

#[test]
fn run_phase_and_crash_restart_fields_round_trip() {
    let (dir, storage) = temp_board();
    let mut task = Task::new("TASK-091", "Crashed queued task");
    task.status = TaskStatus::InProgress;
    task.run_phase = Some(RunPhase::Queued);
    task.crash_restarts = 2;
    task.restart_at = Some(timefmt::parse("2026-08-23T18:30:00").unwrap());

    let path = dir.path().join(".kanban/tasks/in_progress/TASK-091.md");
    storage.save_task(&task).unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    // The restart deadline is quoted like every other naive datetime so the
    // Python CLI's `datetime.fromisoformat` keeps reading it as a string.
    assert!(raw.contains("run_phase: queued"), "{raw}");
    assert!(raw.contains("crash_restarts: 2"), "{raw}");
    assert!(raw.contains("restart_at: '2026-08-23T18:30:00'"), "{raw}");

    let mut reparsed = storage.parse_task_file(&path).unwrap();
    assert_eq!(reparsed, task);

    // Zero/None values are omitted again on the way back.
    reparsed.run_phase = None;
    reparsed.restart_at = None;
    reparsed.crash_restarts = 0;
    storage.save_task(&reparsed).unwrap();
    let cleared = fs::read_to_string(&path).unwrap();
    assert!(!cleared.contains("restart_at"), "{cleared}");
}

#[test]
fn reset_human_restart_clears_crash_bookkeeping() {
    let mut task = Task::new("TASK-092", "Restart bookkeeping");
    task.auto_resumes = 1;
    task.crash_restarts = 3;
    task.restart_at = Some(timefmt::now());
    task.review_rounds = 2;
    task.reset_human_restart();
    assert_eq!(task.auto_resumes, 0);
    assert_eq!(task.crash_restarts, 0);
    assert_eq!(task.restart_at, None);
    assert_eq!(task.review_rounds, 0);
}

#[test]
fn parses_python_written_thread_with_answers_and_variants() {
    let (dir, _storage) = temp_board();
    copy_fixture(
        "threads/TASK-043.yaml",
        &dir.path().join(".kanban/threads/TASK-043.yaml"),
    );
    let manager = ThreadManager::new(dir.path()).unwrap();

    let thread = manager.load("TASK-043").unwrap();
    assert_eq!(thread.rev, 15);
    assert_eq!(thread.task_id, "TASK-043");

    let first = &thread.messages[0];
    assert_eq!(first.id, "MSG-001");
    assert_eq!(first.kind, MessageKind::Question);
    assert_eq!(first.status, MessageStatus::Answered);
    assert_eq!(first.answer.as_deref(), Some("Green"));
    assert_eq!(first.answered_by_role, Some(MessageRole::Human));
    assert!(first.resolved_at.is_some());

    let open = thread
        .messages
        .iter()
        .find(|m| m.id == "MSG-008")
        .expect("MSG-008 present");
    assert_eq!(open.status, MessageStatus::Open);
    assert_eq!(open.variants.len(), 4);
    assert_eq!(open.variants[0], "Adjustable backlight");
    // pyyaml escaped an em dash in this body; it must decode back to the char.
    assert!(open.body.contains('\u{2014}'));
    assert_eq!(open.answer, None);
}

#[test]
fn python_thread_survives_rust_post_cycle() {
    let (dir, _storage) = temp_board();
    copy_fixture(
        "threads/TASK-043.yaml",
        &dir.path().join(".kanban/threads/TASK-043.yaml"),
    );
    let manager = ThreadManager::new(dir.path()).unwrap();
    let before = manager.load("TASK-043").unwrap();

    let posted = manager
        .post(
            "TASK-043",
            MessageRole::Agent,
            MessageKind::Context,
            "ported to rust",
            None,
            vec![],
            Some("agent".to_string()),
        )
        .unwrap();
    assert_eq!(posted.id, "MSG-009");

    let after = manager.load("TASK-043").unwrap();
    assert_eq!(after.rev, before.rev + 1);
    assert_eq!(after.messages.len(), before.messages.len() + 1);
    // every original message survives byte-identically at the model level
    for original in &before.messages {
        let survived = after.messages.iter().find(|m| m.id == original.id).unwrap();
        assert_eq!(original, survived);
    }
}

#[test]
fn parses_python_written_multiline_system_message() {
    let (dir, _storage) = temp_board();
    copy_fixture(
        "threads/TASK-085.yaml",
        &dir.path().join(".kanban/threads/TASK-085.yaml"),
    );
    let manager = ThreadManager::new(dir.path()).unwrap();

    let thread = manager.load("TASK-085").unwrap();
    let system = &thread.messages[0];
    assert_eq!(system.kind, MessageKind::System);
    // pyyaml folds multi-line strings; the parsed body must contain real newlines
    assert!(system.body.contains("Task created: Sonnet 5 update\n"));
    assert!(
        system
            .body
            .contains("Created at: 2026-07-01T10:13:22.036493")
    );
}

#[test]
fn parses_python_written_session_files() {
    let raw = fs::read_to_string(fixtures_dir().join("sessions/ses-claude-20260702-091228.yaml"))
        .unwrap();
    let session: Session = serde_yaml_ng::from_str(&raw).unwrap();
    assert_eq!(session.id, "ses-claude-20260702-091228");
    assert_eq!(session.task_id, "TASK-087");
    assert_eq!(session.name, None);
    assert_eq!(session.status, SessionStatus::Closed);
    assert!(session.ended_at.is_some());

    // round-trip
    let rewritten = serde_yaml_ng::to_string(&session).unwrap();
    let reparsed: Session = serde_yaml_ng::from_str(&rewritten).unwrap();
    assert_eq!(session, reparsed);
}

#[test]
fn loads_python_written_config() {
    let dir = tempfile::tempdir().unwrap();
    copy_fixture("config.yaml", &dir.path().join(".kanban/config.yaml"));
    let config = Config::new(dir.path());

    assert_eq!(
        config.get_threshold("context_embed_max_size").unwrap(),
        5120
    );
    assert_eq!(
        config.get_threshold("session_heartbeat_timeout").unwrap(),
        300
    );
    assert!(config.get_rule("one_task_per_instance").unwrap());
    let ids = config.get_column_ids().unwrap();
    assert_eq!(ids, vec!["todo", "in_progress", "review", "done"]);

    let board = config.load().unwrap();
    assert!(board.agents.contains_key("opencode"));
    assert!(board.agents.contains_key("claude"));
}

#[test]
fn rust_written_task_is_parseable_and_stable() {
    let (dir, storage) = temp_board();
    let task = storage
        .create_task(kanban4ai::core::storage::NewTask {
            title: "Round trip".into(),
            description: "line one\n\nline two with --- dashes".into(),
            ai_model: Some("sonnet".into()),
            ai_effort: Some("high".into()),
            agent_backend: Some("claude".into()),
            agent_name: None,
            interactive: true,
            chained_to: Some("TASK-999".into()),
        })
        .unwrap();

    // second storage instance = fresh process view
    let storage2 = Storage::new(dir.path());
    let reloaded = storage2.load_task(&task.id).unwrap().unwrap();
    assert_eq!(task, reloaded);
    assert_eq!(reloaded.description, "line one\n\nline two with --- dashes");
}
