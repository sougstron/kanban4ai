use kanban4ai::core::models::{RunPhase, SessionStatus, TaskStatus};
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::storage::{NewTask, Storage, atomic_write_text};
use kanban4ai::core::sync::{Journal, Operation, RunState, remote_runs};
use kanban4ai::core::timefmt;
use std::fs;

fn operations(store: &std::path::Path, node: &str) -> Vec<Operation> {
    Journal::open(store)
        .unwrap()
        .operations(Some(node))
        .unwrap()
}

fn board(store: &std::path::Path) -> Storage {
    kanban4ai::core::sync::enable_sync(store).unwrap();
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
    kanban4ai::core::sync::enable_sync(store).unwrap();
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
    let path = dir.path().join("config.yaml");
    let mut config: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    config["sync"]["run_heartbeat"] = serde_yaml_ng::Value::from(5);
    atomic_write_text(&path, &serde_yaml_ng::to_string(&config).unwrap()).unwrap();
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
    let vector = journal.state().unwrap().vector.clone();
    journal
        .prune(&std::collections::BTreeMap::from([("peer".into(), vector)]))
        .unwrap();
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

fn control_board(store: &std::path::Path) -> Storage {
    let storage = board(store);
    atomic_write_text(
        &storage.project_path.join("project.yaml"),
        &format!(
            "id: demo\nname: Demo\nwork_path: {}\ncreated_at: {}\n",
            storage.project_path.display(),
            timefmt::format(&timefmt::now())
        ),
    )
    .unwrap();
    storage
}

#[test]
fn laptop_stop_is_durable_duplicate_safe_and_cannot_stop_new_generation() {
    use kanban4ai::core::operations::Operations;
    let server = tempfile::tempdir().unwrap();
    let laptop = tempfile::tempdir().unwrap();
    let storage = control_board(server.path());
    let journal = Journal::open(server.path()).unwrap();
    let laptop_journal = peer(laptop.path(), &journal);
    let mut task = storage.create_task(NewTask::titled("remote stop")).unwrap();
    task.status = TaskStatus::InProgress;
    task.session = Some("ses-server".into());
    storage.save_task(&task).unwrap();
    let sessions = SessionManager::new(&storage.project_path);
    sessions.link_session(&task.id, "ses-server").unwrap();
    laptop_journal
        .apply_remote(&operations(server.path(), &journal.node.node_id), 300_000)
        .unwrap();
    let frontend = Operations::new(laptop.path().join("projects/demo"));
    frontend.stop_task(&task.id).unwrap();
    // Writing intent on an offline frontend must not touch the runner's session.
    assert_eq!(
        sessions.load_session("ses-server").unwrap().status,
        SessionStatus::Active
    );
    let stop = operations(laptop.path(), &laptop_journal.node.node_id);
    journal.apply_remote(&stop, 300_000).unwrap();
    assert_eq!(
        sessions.load_session("ses-server").unwrap().status,
        SessionStatus::Closed
    );
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[&journal.node.node_id].state,
        RunState::Finished
    );
    task.session = Some("ses-replacement".into());
    storage.save_task(&task).unwrap();
    sessions.link_session(&task.id, "ses-replacement").unwrap();
    journal.apply_remote(&stop, 300_000).unwrap();
    assert_eq!(
        sessions.load_session("ses-replacement").unwrap().status,
        SessionStatus::Active
    );
    // A second stale request has a fresh op ID but still targets generation 1.
    frontend.stop_task(&task.id).unwrap();
    journal
        .apply_remote(
            &operations(laptop.path(), &laptop_journal.node.node_id),
            300_000,
        )
        .unwrap();
    assert_eq!(
        sessions.load_session("ses-replacement").unwrap().status,
        SessionStatus::Active
    );
}

#[test]
fn mobile_synced_last_answer_wakes_only_server_and_duplicate_does_not_replace_queue() {
    use kanban4ai::core::models::{MessageKind, MessageRole};
    use kanban4ai::core::operations::{Operations, QuestionRef};
    use kanban4ai::core::thread::ThreadManager;
    let server = tempfile::tempdir().unwrap();
    let mobile = tempfile::tempdir().unwrap();
    let storage = control_board(server.path());
    let journal = Journal::open(server.path()).unwrap();
    let mobile_journal = peer(mobile.path(), &journal);
    atomic_write_text(&storage.project_path.join(".kanban/config.yaml"),
        "auto_launch:\n  enabled: true\norchestration:\n  queue_enabled: true\n  max_running_total: 1\nrules:\n  resume_after_last_answer: true\n").unwrap();
    let mut task = storage
        .create_task(NewTask::titled("remote answer"))
        .unwrap();
    task.status = TaskStatus::InProgress;
    task.session = Some("ses-server-wait".into());
    task.has_questions = true;
    storage.save_task(&task).unwrap();
    let sessions = SessionManager::new(&storage.project_path);
    sessions.link_session(&task.id, "ses-server-wait").unwrap();
    sessions
        .set_wait(
            "ses-server-wait",
            timefmt::now() + chrono::Duration::hours(1),
            Some("question".into()),
        )
        .unwrap();
    sessions.mark_wait_exited("ses-server-wait").unwrap();
    let mut occupant = storage
        .create_task(NewTask::titled("occupied runner slot"))
        .unwrap();
    occupant.status = TaskStatus::InProgress;
    occupant.session = Some("ses-occupant".into());
    storage.save_task(&occupant).unwrap();
    sessions.link_session(&occupant.id, "ses-occupant").unwrap();
    let tm = ThreadManager::new(&storage.project_path).unwrap();
    let first = tm
        .post(
            &task.id,
            MessageRole::Agent,
            MessageKind::Question,
            "first?",
            None,
            vec![],
            Some("agent".into()),
        )
        .unwrap();
    let last = tm
        .post(
            &task.id,
            MessageRole::Agent,
            MessageKind::Question,
            "last?",
            None,
            vec![],
            Some("agent".into()),
        )
        .unwrap();
    mobile_journal
        .apply_remote(&operations(server.path(), &journal.node.node_id), 300_000)
        .unwrap();
    let frontend = Operations::new(mobile.path().join("projects/demo"));
    frontend
        .answer_question(&task.id, QuestionRef::MsgId(first.id), "one")
        .unwrap();
    journal
        .apply_remote(
            &operations(mobile.path(), &mobile_journal.node.node_id),
            300_000,
        )
        .unwrap();
    assert_eq!(
        sessions.load_session("ses-server-wait").unwrap().status,
        SessionStatus::Active
    );
    frontend
        .answer_question(&task.id, QuestionRef::MsgId(last.id), "two")
        .unwrap();
    let answer = operations(mobile.path(), &mobile_journal.node.node_id);
    journal.apply_remote(&answer, 300_000).unwrap();
    let woken = storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(woken.run_phase, Some(RunPhase::Queued));
    assert!(woken.session.is_none());
    assert_eq!(woken.runner.as_ref().unwrap().as_str(), journal.node.name);
    assert_eq!(
        sessions.load_session("ses-server-wait").unwrap().status,
        SessionStatus::Closed
    );
    assert!(
        SessionManager::new(frontend.data_root())
            .list_active_sessions()
            .is_empty()
    );
    let generation = journal.runs("demo", &task.id).unwrap()[&journal.node.node_id].generation;
    journal.apply_remote(&answer, 300_000).unwrap();
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[&journal.node.node_id].generation,
        generation
    );
}

#[test]
fn mobile_answer_after_finished_server_run_does_not_claim_a_local_session() {
    use kanban4ai::core::models::{MessageKind, MessageRole};
    use kanban4ai::core::operations::{Operations, QuestionRef};
    use kanban4ai::core::thread::ThreadManager;
    let server = tempfile::tempdir().unwrap();
    let mobile = tempfile::tempdir().unwrap();
    let storage = control_board(server.path());
    let journal = Journal::open(server.path()).unwrap();
    let mobile_journal = peer(mobile.path(), &journal);
    atomic_write_text(
        &storage.project_path.join(".kanban/config.yaml"),
        "auto_launch:\n  enabled: true\nrules:\n  resume_after_last_answer: true\n",
    )
    .unwrap();
    let mut task = storage
        .create_task(NewTask::titled("finished remote answer"))
        .unwrap();
    task.status = TaskStatus::InProgress;
    task.session = Some("ses-finished-server".into());
    storage.save_task(&task).unwrap();
    let sessions = SessionManager::new(&storage.project_path);
    sessions
        .link_session(&task.id, "ses-finished-server")
        .unwrap();
    sessions.close_session("ses-finished-server").unwrap();
    let question = ThreadManager::new(&storage.project_path)
        .unwrap()
        .post(
            &task.id,
            MessageRole::Agent,
            MessageKind::Question,
            "continue?",
            None,
            vec![],
            Some("agent".into()),
        )
        .unwrap();
    mobile_journal
        .apply_remote(&operations(server.path(), &journal.node.node_id), 300_000)
        .unwrap();
    let frontend = Operations::new(mobile.path().join("projects/demo"));
    let outcome = frontend
        .answer_question(&task.id, QuestionRef::MsgId(question.id), "yes")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.remaining, 0);
    assert!(outcome.resumed_session.is_none());
    assert!(
        SessionManager::new(frontend.data_root())
            .list_active_sessions()
            .is_empty()
    );
    let runs = mobile_journal.runs("demo", &task.id).unwrap();
    assert!(!runs.contains_key(&mobile_journal.node.node_id));
    assert_eq!(runs[&journal.node.node_id].state, RunState::Finished);
}

#[test]
fn remote_rerun_routes_to_previous_runner_without_frontend_session() {
    use kanban4ai::core::models::RunMode;
    use kanban4ai::core::operations::Operations;
    let server = tempfile::tempdir().unwrap();
    let laptop = tempfile::tempdir().unwrap();
    let storage = control_board(server.path());
    let journal = Journal::open(server.path()).unwrap();
    let frontend_journal = peer(laptop.path(), &journal);
    atomic_write_text(
        &storage.project_path.join(".kanban/config.yaml"),
        "auto_launch:\n  enabled: true\norchestration:\n  queue_enabled: true\n",
    )
    .unwrap();
    let mut task = storage
        .create_task(NewTask::titled("remote rerun"))
        .unwrap();
    task.status = TaskStatus::Review;
    task.session = Some("ses-finished".into());
    storage.save_task(&task).unwrap();
    let sessions = SessionManager::new(&storage.project_path);
    sessions.link_session(&task.id, "ses-finished").unwrap();
    sessions.close_session("ses-finished").unwrap();
    frontend_journal
        .apply_remote(&operations(server.path(), &journal.node.node_id), 300_000)
        .unwrap();
    let frontend = Operations::new(laptop.path().join("projects/demo"));
    frontend
        .rerun_review_task(&task.id, None, RunMode::Queued)
        .unwrap();
    assert_eq!(
        storage.load_task(&task.id).unwrap().unwrap().status,
        TaskStatus::Review
    );
    journal
        .apply_remote(
            &operations(laptop.path(), &frontend_journal.node.node_id),
            300_000,
        )
        .unwrap();
    let queued = storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(queued.status, TaskStatus::InProgress);
    assert_eq!(queued.run_phase, Some(RunPhase::Queued));
    assert!(
        !queued
            .session
            .as_deref()
            .is_some_and(|id| sessions.is_session_active(id))
    );
    assert!(
        SessionManager::new(frontend.data_root())
            .list_active_sessions()
            .is_empty()
    );
}
