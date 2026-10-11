use kanban4ai::core::models::{RunPhase, SessionStatus, TaskStatus};
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::storage::{NewTask, Storage, atomic_write_text};
use kanban4ai::core::sync::{Journal, Operation, RunState, remote_runs};
use kanban4ai::core::timefmt;
use std::fs;

fn operations(store: &std::path::Path, node: &str) -> Vec<Operation> {
    fs::read_to_string(store.join("sync/journal").join(format!("{node}.jsonl")))
        .unwrap_or_default()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

fn board(store: &std::path::Path) -> Storage {
    let root = store.join("projects/demo");
    fs::create_dir_all(&root).unwrap();
    atomic_write_text(&root.join("project.yaml"), "id: demo\nname: Demo\n").unwrap();
    let storage = Storage::new(root);
    storage.init_board().unwrap();
    storage
}

fn peer(store: &std::path::Path, source: &Journal) -> Journal {
    let mut node = Journal::open(store).unwrap().node;
    node.store_id = source.node.store_id.clone();
    fs::write(
        store.join("sync/node.yaml"),
        serde_yaml_ng::to_string(&node).unwrap(),
    )
    .unwrap();
    Journal::open(store).unwrap()
}

#[test]
fn lifecycle_is_runner_owned_thinned_and_does_not_replicate_process_state() {
    let source = tempfile::tempdir().unwrap();
    let replica = tempfile::tempdir().unwrap();
    let storage = board(source.path());
    let journal = Journal::open(source.path()).unwrap();
    let peer = peer(replica.path(), &journal);
    let mut task = storage
        .create_task(NewTask::titled("portable run"))
        .unwrap();
    task.status = TaskStatus::InProgress;
    task.run_phase = Some(RunPhase::Design);
    task.session = Some("ses-local".into());
    task.worktree = Some("/private/worktree".into());
    task.branch = Some(format!("kanban/{}", task.id));
    storage.save_task(&task).unwrap();
    let sessions = SessionManager::new(&storage.project_path);
    let session = sessions.link_session(&task.id, "ses-local").unwrap();
    let node = &journal.node.node_id;
    let first = journal.runs("demo", &task.id).unwrap()[node].clone();
    assert_eq!(first.state, RunState::Running);
    assert_eq!(first.phase, "designer");
    assert_eq!(first.branch, task.branch);
    assert_eq!(first.generation, 1);
    let count = operations(source.path(), node).len();
    sessions.heartbeat("ses-local").unwrap();
    assert_eq!(operations(source.path(), node).len(), count);
    let mut later = session.clone();
    later.last_seen += chrono::Duration::seconds(60);
    sessions.save_session(&later).unwrap();
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[node].heartbeat,
        timefmt::format(&later.last_seen)
    );
    sessions
        .set_wait(
            "ses-local",
            timefmt::now() + chrono::Duration::hours(1),
            Some("local secret note".into()),
        )
        .unwrap();
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[node].state,
        RunState::Waiting
    );
    let ops = operations(source.path(), node);
    let portable = serde_json::to_string(&ops).unwrap();
    for secret in [
        "ses-local",
        "/private/worktree",
        "local secret note",
        "wait_until",
        "last_seen",
        "pid",
    ] {
        assert!(!portable.contains(secret), "replicated {secret}");
    }
    peer.apply_remote(&ops, 300_000).unwrap();
    let remote_board = Storage::new(replica.path().join("projects/demo"));
    let remote_task = remote_board.load_task(&task.id).unwrap().unwrap();
    assert!(remote_task.session.is_none());
    assert!(remote_task.worktree.is_none());
    assert!(remote_task.branch.is_none());
    assert!(remote_task.run_phase.is_none());
    let remote_sessions = SessionManager::new(&remote_board.project_path);
    assert!(remote_sessions.check_sessions(0).unwrap().is_empty());
    assert_eq!(
        remote_runs(&remote_board.project_path, &task.id).unwrap()[0].state,
        RunState::Waiting
    );
    let before = peer.runs("demo", &task.id).unwrap();
    // Editing the replicated task cannot rewrite another runner's descriptor.
    let mut edited = remote_task;
    edited.title = "peer edit".into();
    remote_board.save_task(&edited).unwrap();
    assert_eq!(peer.runs("demo", &task.id).unwrap(), before);
    assert!(!remote_board.sessions_dir.join("ses-local.yaml").exists());
    sessions.close_session("ses-local").unwrap();
    let finished = journal.runs("demo", &task.id).unwrap()[node].clone();
    assert_eq!(finished.state, RunState::Finished);
    assert_eq!(finished.generation, first.generation);
    peer.apply_remote(&operations(source.path(), node), 300_000)
        .unwrap();
    assert!(
        remote_runs(&remote_board.project_path, &task.id)
            .unwrap()
            .is_empty()
    );
    let replacement = sessions.link_session(&task.id, "ses-next").unwrap();
    assert_eq!(journal.runs("demo", &task.id).unwrap()[node].generation, 2);
    // A delayed close/heartbeat from the old process cannot finish its replacement.
    sessions.save_session(&later).unwrap();
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[node].started_at,
        Some(timefmt::format(&replacement.started_at))
    );
    sessions.crash_session("ses-next").unwrap();
    let failed = journal.runs("demo", &task.id).unwrap()[node].clone();
    assert_eq!(failed.state, RunState::Failed);
}

#[test]
fn queue_cancellation_and_heartbeat_setting_are_local() {
    let dir = tempfile::tempdir().unwrap();
    let storage = board(dir.path());
    let journal = Journal::open(dir.path()).unwrap();
    let mut task = storage.create_task(NewTask::titled("queued")).unwrap();
    task.status = TaskStatus::InProgress;
    task.run_phase = Some(RunPhase::Queued);
    storage.save_task(&task).unwrap();
    let node = &journal.node.node_id;
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[node].state,
        RunState::Queued
    );
    task.run_phase = None;
    task.status = TaskStatus::Todo;
    storage.save_task(&task).unwrap();
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[node].state,
        RunState::Finished
    );
    let config = fs::read_to_string(dir.path().join("config.yaml")).unwrap();
    atomic_write_text(
        &dir.path().join("config.yaml"),
        &(config + "\nsync:\n  run_heartbeat: 5\n"),
    )
    .unwrap();
    task.run_phase = Some(RunPhase::Execute);
    storage.save_task(&task).unwrap();
    let sessions = SessionManager::new(&storage.project_path);
    let mut session = sessions.link_session(&task.id, "ses-interval").unwrap();
    session.last_seen += chrono::Duration::seconds(4);
    sessions.save_session(&session).unwrap();
    assert_ne!(
        journal.runs("demo", &task.id).unwrap()[node].heartbeat,
        timefmt::format(&session.last_seen)
    );
    session.last_seen += chrono::Duration::seconds(1);
    sessions.save_session(&session).unwrap();
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[node].heartbeat,
        timefmt::format(&session.last_seen)
    );
}

#[test]
fn forged_runner_and_local_fields_are_rejected_before_application() {
    let source = tempfile::tempdir().unwrap();
    let replica = tempfile::tempdir().unwrap();
    let storage = board(source.path());
    let journal = Journal::open(source.path()).unwrap();
    let peer = peer(replica.path(), &journal);
    let task = storage.create_task(NewTask::titled("ownership")).unwrap();
    SessionManager::new(&storage.project_path)
        .link_session(&task.id, "ses-owner")
        .unwrap();
    let run = operations(source.path(), &journal.node.node_id)
        .into_iter()
        .find(|op| op.entity == "run")
        .unwrap();
    let mut forged = run.clone();
    let value = forged.set.remove(&journal.node.node_id).unwrap();
    forged.set.insert("other-node".into(), value);
    assert!(peer.apply_remote(&[forged], 300_000).is_err());
    let mut leaked = run;
    leaked.set.get_mut(&journal.node.node_id).unwrap()["pid"] = serde_json::json!(123);
    assert!(peer.apply_remote(&[leaked], 300_000).is_err());
    assert!(peer.state().unwrap().fields.is_empty());
    assert_eq!(
        SessionManager::new(&storage.project_path)
            .load_session("ses-owner")
            .unwrap()
            .status,
        SessionStatus::Active
    );
}

#[test]
fn concurrent_runners_survive_reordered_delivery_and_next_generation_sees_both() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let storage = board(a.path());
    let journal = Journal::open(a.path()).unwrap();
    let peer = peer(b.path(), &journal);
    let task = storage.create_task(NewTask::titled("partition")).unwrap();
    peer.apply_remote(&operations(a.path(), &journal.node.node_id), 300_000)
        .unwrap();
    let sa = SessionManager::new(&storage.project_path);
    let sb = SessionManager::new(b.path().join("projects/demo"));
    sa.link_session(&task.id, "ses-a").unwrap();
    sb.link_session(&task.id, "ses-b").unwrap();
    sa.close_session("ses-a").unwrap();
    let mut a_ops = operations(a.path(), &journal.node.node_id);
    a_ops.reverse();
    assert!(peer.apply_remote(&a_ops, 300_000).unwrap().is_empty());
    assert!(
        journal
            .apply_remote(&operations(b.path(), &peer.node.node_id), 300_000)
            .unwrap()
            .is_empty()
    );
    let expected = journal.runs("demo", &task.id).unwrap();
    assert_eq!(peer.runs("demo", &task.id).unwrap(), expected);
    assert_eq!(expected[&journal.node.node_id].state, RunState::Finished);
    assert_eq!(expected[&peer.node.node_id].state, RunState::Running);
    assert_eq!(expected[&journal.node.node_id].generation, 1);
    assert_eq!(expected[&peer.node.node_id].generation, 1);
    sa.link_session(&task.id, "ses-a-next").unwrap();
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[&journal.node.node_id].generation,
        2
    );
    let vector = journal.state().unwrap().vector;
    journal.compact(&[vector]).unwrap();
    assert_eq!(
        Journal::open(a.path())
            .unwrap()
            .runs("demo", &task.id)
            .unwrap()[&journal.node.node_id]
            .generation,
        2
    );
}

#[test]
fn automatic_queue_and_retry_do_not_adopt_a_foreign_run() {
    let source = tempfile::tempdir().unwrap();
    let replica = tempfile::tempdir().unwrap();
    let storage = board(source.path());
    let journal = Journal::open(source.path()).unwrap();
    let peer = peer(replica.path(), &journal);
    let mut task = storage
        .create_task(NewTask::titled("foreign runner"))
        .unwrap();
    task.status = TaskStatus::InProgress;
    storage.save_task(&task).unwrap();
    SessionManager::new(&storage.project_path)
        .link_session(&task.id, "ses-server")
        .unwrap();
    peer.apply_remote(&operations(source.path(), &journal.node.node_id), 300_000)
        .unwrap();
    let remote_board = Storage::new(replica.path().join("projects/demo"));
    atomic_write_text(
        &remote_board.project_path.join(".kanban/config.yaml"),
        "auto_launch:\n  enabled: true\norchestration:\n  queue_enabled: true\n  auto_restart:\n    enabled: true\n    delays_minutes: [1]\nnotifications:\n  enabled: false\n",
    ).unwrap();
    let mut queued = remote_board.load_task(&task.id).unwrap().unwrap();
    queued.run_phase = Some(RunPhase::Queued);
    queued.restart_at = Some(timefmt::now() - chrono::Duration::minutes(1));
    remote_board.save_task(&queued).unwrap();
    let ops = kanban4ai::core::operations::Operations::new(&remote_board.project_path);
    assert!(ops.due_restarts().unwrap().is_empty());
    assert_eq!(
        remote_board
            .load_task(&task.id)
            .unwrap()
            .unwrap()
            .crash_restarts,
        0
    );
    queued.restart_at = None;
    remote_board.save_task(&queued).unwrap();
    assert!(ops.dispatch_queue().unwrap().is_empty());
    let after = remote_board.load_task(&task.id).unwrap().unwrap();
    assert!(after.session.is_none());
    assert_eq!(after.run_phase, Some(RunPhase::Queued));
    assert_eq!(
        peer.runs("demo", &task.id).unwrap()[&journal.node.node_id].state,
        RunState::Running
    );
}
