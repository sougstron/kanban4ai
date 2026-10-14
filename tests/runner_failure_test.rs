use kanban4ai::core::models::TaskStatus;
use kanban4ai::core::runner_failure::{self, BACKEND_MISSING, POWERED_OFF, UNREACHABLE};
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::storage::{NewTask, Storage, atomic_write_text};
use kanban4ai::core::sync::{Journal, Operation};
use kanban4ai::core::thread::ThreadManager;
use kanban4ai::core::timefmt;
use std::fs;

fn board(root: &std::path::Path) -> Storage {
    let board = root.join("projects/demo");
    fs::create_dir_all(&board).unwrap();
    atomic_write_text(
        &board.join("project.yaml"),
        &format!(
            "id: demo\nname: Demo\nwork_path: ''\ncreated_at: '{}'\n",
            timefmt::format(&timefmt::now())
        ),
    )
    .unwrap();
    let storage = Storage::new(board);
    storage.init_board().unwrap();
    storage
}
fn replicate(source: &std::path::Path, runner: &Journal, peer: &Journal) {
    let ops: Vec<Operation> = fs::read_to_string(
        source
            .join("sync/journal")
            .join(format!("{}.jsonl", runner.node.node_id)),
    )
    .unwrap()
    .lines()
    .map(|s| serde_json::from_str(s).unwrap())
    .collect();
    peer.apply_remote(&ops, 300_000).unwrap();
}

#[test]
fn peer_reports_expired_heartbeat_and_bye_without_claiming_run_and_recovers() {
    let source = tempfile::tempdir().unwrap();
    let replica = tempfile::tempdir().unwrap();
    let storage = board(source.path());
    let runner = Journal::open(source.path()).unwrap();
    let mut identity = Journal::open(replica.path()).unwrap().node;
    identity.store_id = runner.node.store_id.clone();
    fs::write(
        replica.path().join("sync/node.yaml"),
        serde_yaml_ng::to_string(&identity).unwrap(),
    )
    .unwrap();
    let peer = Journal::open(replica.path()).unwrap();
    let mut task = storage.create_task(NewTask::titled("remote run")).unwrap();
    task.status = TaskStatus::InProgress;
    task.session = Some("ses-runner".into());
    storage.save_task(&task).unwrap();
    let sessions = SessionManager::new(&storage.project_path);
    let mut session = kanban4ai::core::models::Session::new("ses-runner", &task.id);
    session.last_seen = timefmt::now() - chrono::Duration::minutes(5);
    sessions.save_session(&session).unwrap();
    replicate(source.path(), &runner, &peer);
    let remote = Storage::new(replica.path().join("projects/demo"));
    let original = peer.runs("demo", &task.id).unwrap();
    runner_failure::observe(replica.path(), &runner.node.node_id, false, None, 600).unwrap();
    assert!(
        remote
            .load_task(&task.id)
            .unwrap()
            .unwrap()
            .run_failure
            .is_none()
    );
    let descriptors = peer.runs("demo", &task.id).unwrap();
    runner_failure::observe(replica.path(), &runner.node.node_id, false, None, 90).unwrap();
    assert_eq!(
        remote
            .load_task(&task.id)
            .unwrap()
            .unwrap()
            .run_failure
            .as_deref(),
        Some(UNREACHABLE)
    );
    let threads = ThreadManager::new(&remote.project_path).unwrap();
    threads
        .post_board_note(&task.id, "unrelated progress")
        .unwrap();
    runner_failure::observe(replica.path(), &runner.node.node_id, false, None, 90).unwrap();
    assert_eq!(
        threads
            .load(&task.id)
            .unwrap()
            .messages
            .iter()
            .filter(|m| m.body.starts_with(UNREACHABLE))
            .count(),
        1
    );
    let at = chrono::Utc::now();
    runner_failure::observe(
        replica.path(),
        &runner.node.node_id,
        false,
        Some((at, "shutdown")),
        90,
    )
    .unwrap();
    assert_eq!(
        remote
            .load_task(&task.id)
            .unwrap()
            .unwrap()
            .run_failure
            .as_deref(),
        Some(POWERED_OFF)
    );
    let note = threads
        .load(&task.id)
        .unwrap()
        .messages
        .into_iter()
        .find(|m| m.body.starts_with(POWERED_OFF))
        .unwrap();
    assert!(
        note.body
            .contains(&at.with_timezone(&chrono::Local).format("%H:%M").to_string())
    );
    assert!(note.body.contains("на другом узле"));
    assert_eq!(peer.runs("demo", &task.id).unwrap(), descriptors);
    assert!(
        remote
            .load_task(&task.id)
            .unwrap()
            .unwrap()
            .session
            .is_none()
    );
    assert_eq!(
        original[&runner.node.node_id].generation,
        descriptors[&runner.node.node_id].generation
    );
    // Reachability alone cannot clear a failure; the runner must publish a heartbeat.
    runner_failure::observe(replica.path(), &runner.node.node_id, true, None, 90).unwrap();
    assert_eq!(
        remote
            .load_task(&task.id)
            .unwrap()
            .unwrap()
            .run_failure
            .as_deref(),
        Some(POWERED_OFF)
    );
    session.last_seen = timefmt::now() + chrono::Duration::seconds(1);
    session.wait_until = None;
    sessions.save_session(&session).unwrap();
    replicate(source.path(), &runner, &peer);
    runner_failure::observe(replica.path(), &runner.node.node_id, true, None, 90).unwrap();
    assert!(
        remote
            .load_task(&task.id)
            .unwrap()
            .unwrap()
            .run_failure
            .is_none()
    );
}

#[test]
fn local_failure_code_survives_crash_and_is_not_inherited_by_next_run() {
    let root = tempfile::tempdir().unwrap();
    let storage = board(root.path());
    let journal = Journal::open(root.path()).unwrap();
    let mut task = storage
        .create_task(NewTask::titled("backend failure"))
        .unwrap();
    task.session = Some("ses-first".into());
    storage.save_task(&task).unwrap();
    let sessions = SessionManager::new(&storage.project_path);
    sessions.link_session(&task.id, "ses-first").unwrap();
    runner_failure::record(
        &storage.project_path,
        &task.id,
        BACKEND_MISSING,
        "not logged in",
    )
    .unwrap();
    sessions.crash_session("ses-first").unwrap();
    assert_eq!(
        journal.runs("demo", &task.id).unwrap()[&journal.node.node_id]
            .code
            .as_deref(),
        Some(BACKEND_MISSING)
    );
    runner_failure::clear(&storage.project_path, &task.id).unwrap();
    sessions.link_session(&task.id, "ses-next").unwrap();
    let run = &journal.runs("demo", &task.id).unwrap()[&journal.node.node_id];
    assert!(run.code.is_none());
    assert!(run.active());
}

#[test]
fn missing_backend_fails_before_spawning_and_records_recoverable_task() {
    let root = tempfile::tempdir().unwrap();
    let storage = Storage::new(root.path());
    storage.init_board().unwrap();
    atomic_write_text(
        &storage.kanban_dir.join("config.yaml"),
        "auto_launch:\n  default_agent: omp\nagents:\n  omp:\n    command: /nonexistent/TASK-421-backend\n",
    )
    .unwrap();
    let task = storage
        .create_task(NewTask::titled("missing backend"))
        .unwrap();
    let ops = kanban4ai::core::operations::Operations::new(root.path());
    let err = ops.start_task(&task.id).unwrap_err();
    assert!(err.to_string().contains(BACKEND_MISSING), "{err}");
    let failed = storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(failed.run_failure.as_deref(), Some(BACKEND_MISSING));
    assert_eq!(failed.status, TaskStatus::Todo);
    let session = SessionManager::new(root.path())
        .load_session(failed.session.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        session.status,
        kanban4ai::core::models::SessionStatus::Crashed
    );
    assert!(
        ThreadManager::new(root.path())
            .unwrap()
            .load(&task.id)
            .unwrap()
            .messages
            .iter()
            .any(|m| m.body.starts_with(BACKEND_MISSING))
    );
}

#[test]
fn authentication_diagnostics_do_not_classify_quota_or_normal_messages() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("transcript.jsonl");
    fs::write(&path, "{\"type\":\"assistant\",\"message\":\"please log in\"}\n{\"type\":\"error\",\"error\":{\"data\":{\"statusCode\":429,\"message\":\"usage limit reached\"}}}\n").unwrap();
    assert!(runner_failure::login_diagnostic(&path).is_none());
    fs::write(
        &path,
        "{\"type\":\"result\",\"is_error\":true,\"result\":\"Not logged in. Please log in.\"}\n",
    )
    .unwrap();
    assert_eq!(
        runner_failure::login_diagnostic(&path).as_deref(),
        Some("Not logged in. Please log in.")
    );
    fs::write(
        &path,
        "The user was not logged in yesterday.\nError: quota exhausted\n",
    )
    .unwrap();
    assert!(runner_failure::login_diagnostic(&path).is_none());
    fs::write(&path, "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"error\",\"errorMessage\":\"No API key found for anthropic\"}}\n").unwrap();
    assert_eq!(
        runner_failure::login_diagnostic(&path).as_deref(),
        Some("No API key found for anthropic")
    );
    let credits = serde_json::json!({"type":"error","error":{"data":{
        "statusCode":401, "message":"Insufficient balance", "isRetryable":false
    }}});
    assert!(!runner_failure::authentication_error(
        &credits,
        "Insufficient balance"
    ));
}

#[test]
fn deleted_work_folder_records_repo_failure_instead_of_spawning() {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    fs::create_dir(&work).unwrap();
    let store = kanban4ai::core::project::ProjectStore::at(root.path().join("store"));
    let project = store.add(&work, None).unwrap().project;
    let ops = kanban4ai::core::operations::Operations::for_project(&project);
    let task = ops
        .create_task(NewTask::titled("missing repository"))
        .unwrap();
    fs::remove_dir(&work).unwrap();
    let err = ops.start_task(&task.id).unwrap_err();
    assert!(
        err.to_string().contains(runner_failure::REPO_MISSING),
        "{err}"
    );
    let failed = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(
        failed.run_failure.as_deref(),
        Some(runner_failure::REPO_MISSING)
    );
    assert_eq!(failed.status, TaskStatus::Todo);
    assert!(
        ThreadManager::new(&project.data_root)
            .unwrap()
            .load(&task.id)
            .unwrap()
            .messages
            .iter()
            .any(|m| m.body.starts_with(runner_failure::REPO_MISSING))
    );
}
