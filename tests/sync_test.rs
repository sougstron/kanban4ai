mod common;

use kanban4ai::core::storage::{Storage, atomic_write_text};
use kanban4ai::core::sync::{Hlc, Journal, Operation};
use std::collections::BTreeMap;
use std::fs;

fn operations(store: &std::path::Path, node: &str) -> Vec<Operation> {
    Journal::open(store)
        .unwrap()
        .operations(Some(node))
        .unwrap()
}

#[test]
fn bootstrap_only_transfers_board_data_instructions_and_images() {
    use kanban4ai::core::sync::transport::{snapshot_spool, stage_snapshot};

    let source = tempfile::tempdir().unwrap();
    let board = source.path().join("projects/demo");
    Storage::new(&board).init_board().unwrap();
    fs::write(board.join("project.yaml"), "name: demo\n").unwrap();
    let included = [
        ("config.yaml", "name: demo\n"),
        (
            "tasks/todo/TASK-001.md",
            "---\nid: TASK-001\ntitle: demo\nstatus: todo\n---\nbody\n",
        ),
        ("threads/TASK-001.yaml", "messages: []\n"),
        ("instructions/executor.md", "Follow project rules\n"),
        ("assets/images/nested/screenshot.png", "image bytes"),
    ];
    let excluded = [
        "assets/report.csv",
        "assets/TASK-051/result.json",
        "assets/images-other/large.csv",
        "context/TASK-001.md",
    ];
    for (relative, contents) in included {
        let path = board.join(".kanban").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    for relative in excluded {
        let path = board.join(".kanban").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "local-only").unwrap();
    }
    let snapshot = snapshot_spool(source.path()).unwrap();
    let lines: Vec<serde_json::Value> = fs::read_to_string(snapshot.path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for relative in excluded {
        let path = format!("projects/demo/.kanban/{relative}");
        assert!(
            !lines.iter().any(|line| line["path"] == path),
            "snapshot leaked {relative}"
        );
    }
    let receiver = tempfile::tempdir().unwrap();
    Journal::open(receiver.path()).unwrap();
    common::bootstrap(source.path(), receiver.path());
    for (relative, contents) in included {
        let path = receiver.path().join("projects/demo/.kanban").join(relative);
        let actual = fs::read_to_string(path).unwrap();
        if relative.starts_with("tasks/") {
            assert!(actual.contains("title: demo"));
            assert!(actual.contains("body"));
        } else {
            assert_eq!(actual, contents);
        }
    }
    for relative in excluded {
        assert!(
            !receiver
                .path()
                .join("projects/demo/.kanban")
                .join(relative)
                .exists()
        );
        let mut forbidden = lines.clone();
        let end = forbidden.last_mut().unwrap();
        end["files"] = (end["files"].as_u64().unwrap() + 1).into();
        forbidden.insert(
            forbidden.len() - 1,
            serde_json::json!({
                "kind": "file",
                "path": format!("projects/demo/.kanban/{relative}"),
                "text": "not allowed"
            }),
        );
        let input = tempfile::NamedTempFile::new().unwrap();
        let mut content = forbidden.iter()
            .map(|line| serde_json::to_string(line).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        content.push('\n');
        fs::write(input.path(), content).unwrap();
        assert!(stage_snapshot(receiver.path(), input.path()).is_err());
    }
}

#[test]
fn clock_survives_restart_and_rejects_skew_without_advancing() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let identity = journal.node.clone();
    let remote = Hlc(
        chrono::Utc::now().timestamp_millis() as u64 + 1000,
        40,
        "remote".into(),
    );
    journal.observe(&remote, 5000).unwrap();
    let reopened = Journal::open(dir.path()).unwrap();
    assert_eq!(reopened.node, identity);
    let persisted: Hlc =
        serde_json::from_str(&fs::read_to_string(dir.path().join("sync/hlc")).unwrap()).unwrap();
    assert!(persisted.tick(0, &identity.node_id).unwrap() > remote);
    let future = Hlc(remote.0 + 100000, 0, "bad".into());
    assert!(
        reopened
            .observe(&future, 5000)
            .unwrap_err()
            .to_string()
            .contains("SYNC_CLOCK_SKEW")
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("sync/hlc")).unwrap(),
        serde_json::to_string(&persisted).unwrap()
    );
    assert!(Hlc(1, u64::MAX, "a".into()).tick(0, "b").is_err());
}

#[test]
fn field_diffs_moves_local_exclusions_and_acknowledged_compaction() {
    let dir = tempfile::tempdir().unwrap();
    kanban4ai::core::sync::enable_sync(dir.path()).unwrap();
    let root = dir.path().join("projects/demo");
    let storage = Storage::new(&root);
    storage.init_board().unwrap();
    let path = storage.tasks_dir.join("todo/TASK-001.md");
    let text = "---\nid: TASK-001\ntitle: before\nstatus: todo\nsession: local\n---\nbody\n";
    atomic_write_text(&path, text).unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let node = &journal.node.node_id;
    let created = operations(dir.path(), node).pop().unwrap();
    assert_eq!(created.set["description"], "body");
    assert!(!created.set.contains_key("session"));
    let moved = storage.tasks_dir.join("review/TASK-001.md");
    atomic_write_text(&moved, &text.replace("status: todo", "status: review")).unwrap();
    assert!(!path.exists());
    let ops = operations(dir.path(), node);
    let change = ops.last().unwrap();
    assert_eq!(
        change.set,
        BTreeMap::from([("status".into(), serde_json::json!("review"))])
    );
    assert_eq!(change.prev["status"], created.hlc);
    let count = ops.len();
    atomic_write_text(
        &moved,
        &fs::read_to_string(&moved)
            .unwrap()
            .replace("session: local", "session: other"),
    )
    .unwrap();
    assert_eq!(operations(dir.path(), node).len(), count);
    let vector = journal.state().unwrap().vector;
    let held = journal
        .prune(&BTreeMap::from([("peer".into(), BTreeMap::new())]))
        .unwrap();
    assert_eq!(held.get(node).copied().unwrap_or(0), 0);
    let seq = vector[node];
    let held = journal
        .prune(&BTreeMap::from([(
            "peer".into(),
            BTreeMap::from([(node.clone(), seq - 1)]),
        )]))
        .unwrap();
    assert_eq!(held.get(node).copied().unwrap_or(0), seq - 1);
    assert_eq!(operations(dir.path(), node).len(), 1);
    assert_eq!(
        journal
            .state()
            .unwrap()
            .compacted
            .get(node)
            .copied()
            .unwrap_or(0),
        seq - 1
    );
    let held = journal
        .prune(&BTreeMap::from([("peer".into(), vector.clone())]))
        .unwrap();
    assert_eq!(held.get(node).copied().unwrap_or(0), seq);
    assert!(operations(dir.path(), node).is_empty());
    storage.delete_task("TASK-001").unwrap();
    let deleted = operations(dir.path(), node).pop().unwrap();
    assert_eq!(deleted.entity, "tombstone");
    assert_eq!(deleted.set["deleted"], true);
}

#[test]
fn message_uids_parent_links_and_dotted_config_diffs() {
    let dir = tempfile::tempdir().unwrap();
    kanban4ai::core::sync::enable_sync(dir.path()).unwrap();
    let root = dir.path().join("projects/demo");
    let storage = Storage::new(&root);
    storage.init_board().unwrap();
    let path = storage.threads_dir.join("TASK-001.yaml");
    let thread = "task_id: TASK-001\nrev: 1\nmessages:\n- id: MSG-999\n  body: question\n- id: MSG-1000\n  body: reply\n  parent_id: MSG-999\n";
    atomic_write_text(&path, thread).unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let ops = operations(dir.path(), &journal.node.node_id);
    let messages: Vec<_> = ops.iter().filter(|op| op.entity == "message").collect();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].id, messages[0].op);
    assert_eq!(messages[1].set["parent_id"], messages[0].id);
    atomic_write_text(&path, &thread.replace("rev: 1", "rev: 2")).unwrap();
    assert_eq!(
        operations(dir.path(), &journal.node.node_id).len(),
        ops.len()
    );
    atomic_write_text(&path, &thread.replace("body: question", "body: edited")).unwrap();
    let update = operations(dir.path(), &journal.node.node_id).pop().unwrap();
    assert_eq!(update.id, messages[0].id);
    assert_eq!(update.prev["body"], messages[0].hlc);
    atomic_write_text(&path, "task_id: TASK-001\nrev: 4\nmessages: []\n").unwrap();
    let deleted = operations(dir.path(), &journal.node.node_id).pop().unwrap();
    assert_eq!(deleted.entity, "tombstone");
    atomic_write_text(&path, thread).unwrap();
    let recreated = operations(dir.path(), &journal.node.node_id);
    let reply = recreated.last().unwrap();
    assert_eq!(reply.id, reply.op);
    assert_ne!(reply.id, messages[1].id);
    assert_ne!(reply.set["parent_id"], messages[0].id);
    let threads = kanban4ai::core::thread::ThreadManager::new(&root).unwrap();
    assert!(threads.discard_thread("TASK-001").unwrap());
    assert!(!path.exists());
    let tombstones: Vec<_> = operations(dir.path(), &journal.node.node_id)
        .into_iter()
        .rev()
        .take(3)
        .collect();
    assert_eq!(tombstones[0].entity, "thread");
    assert_eq!(tombstones[0].set["deleted"], true);
    assert!(tombstones[1..].iter().all(|op| op.entity == "tombstone"));
    assert!(tombstones.iter().any(|op| op.id == reply.id));
    let config = root.join(".kanban/config.yaml");
    atomic_write_text(&config, "agents:\n  codex:\n    model: first\n").unwrap();
    atomic_write_text(&config, "agents:\n  codex:\n    model: second\n").unwrap();
    let update = operations(dir.path(), &journal.node.node_id).pop().unwrap();
    assert_eq!(
        update.set,
        BTreeMap::from([("agents.codex.model".into(), serde_json::json!("second"))])
    );
    let global = dir.path().join("config.yaml");
    atomic_write_text(&global, "node:\n  name: server\n  role: server\ntui:\n  theme: dark\nagents:\n  codex:\n    model: global\n").unwrap();
    let update = operations(dir.path(), &journal.node.node_id).pop().unwrap();
    assert_eq!(
        update.set,
        BTreeMap::from([("agents.codex.model".into(), serde_json::json!("global"))])
    );
    let reopened = Journal::open(dir.path()).unwrap();
    assert_eq!(reopened.node.node_id, journal.node.node_id);
    assert_eq!(reopened.node.role, kanban4ai::core::sync::NodeRole::Server);
}

#[test]
fn runner_preferences_replicate_and_task_can_return_to_inherit() {
    use kanban4ai::core::config::{Config, Runner};
    use kanban4ai::core::storage::NewTask;
    let source = tempfile::tempdir().unwrap();
    kanban4ai::core::sync::enable_sync(source.path()).unwrap();
    let a = Journal::open(source.path()).unwrap();
    let work = source.path().join("work");
    fs::create_dir(&work).unwrap();
    let project = kanban4ai::core::project::ProjectStore::at(source.path())
        .add(&work, None)
        .unwrap()
        .project;
    let root = project.data_root;
    let storage = Storage::new(&root);
    storage.init_board().unwrap();
    atomic_write_text(
        &source.path().join("config.yaml"),
        "runner:\n  default: server\n  auto_order: [mobile, here]\nsync:\n  enabled: true\n",
    )
    .unwrap();
    kanban4ai::core::sync::enable_sync(source.path()).unwrap();
    let config = Config::new(&root);
    let mut board = config.load_raw_fresh().unwrap();
    board.runner.insert("default".into(), "mobile".into());
    board.inherit.insert("placement".into(), false.into());
    config.save(&board).unwrap();
    let task = storage
        .create_task(NewTask {
            runner: Some(Runner::Here),
            ..NewTask::titled("Placement")
        })
        .unwrap();
    let peer = tempfile::tempdir().unwrap();
    let b = Journal::open(peer.path()).unwrap();
    let mut identity = b.node.clone();
    identity.store_id = a.node.store_id.clone();
    fs::write(
        peer.path().join("sync/node.yaml"),
        serde_yaml_ng::to_string(&identity).unwrap(),
    )
    .unwrap();
    let b = Journal::open(peer.path()).unwrap();
    let ops = operations(source.path(), &a.node.node_id);
    b.apply_remote(&ops, 60_000).unwrap();
    let remote_root = peer.path().join("projects").join(project.id);
    let remote_storage = Storage::new(&remote_root);
    assert_eq!(
        remote_storage.load_task(&task.id).unwrap().unwrap().runner,
        Some(Runner::Here)
    );
    assert_eq!(
        Config::new(&remote_root).load().unwrap().runner_for(None),
        Runner::Mobile
    );
    let global: kanban4ai::core::global::GlobalConfig =
        serde_yaml_ng::from_str(&fs::read_to_string(peer.path().join("config.yaml")).unwrap())
            .unwrap();
    assert_eq!(global.board_sections().runner_for(None), Runner::Server);
    assert_eq!(
        global.board_sections().runner_auto_order(),
        [Runner::Mobile, Runner::Here]
    );
    {
        let _lock = storage.lock().unwrap();
        let mut task = storage.load_task(&task.id).unwrap().unwrap();
        task.runner = None;
        storage.save_task(&task).unwrap();
    }
    b.apply_remote(&operations(source.path(), &a.node.node_id), 60_000)
        .unwrap();
    assert_eq!(
        remote_storage.load_task(&task.id).unwrap().unwrap().runner,
        None
    );
}

#[test]
fn redo_recovers_a_torn_append_and_never_duplicates_committed_operations() {
    let dir = tempfile::tempdir().unwrap();
    kanban4ai::core::sync::enable_sync(dir.path()).unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let root = dir.path().join("projects/demo");
    Storage::new(&root).init_board().unwrap();
    let path = root.join(".kanban/tasks/todo/TASK-001.md");
    let text = "---\nid: TASK-001\ntitle: old\nstatus: todo\n---\nbody\n";
    atomic_write_text(&path, text).unwrap();
    let node = journal.node.node_id.clone();
    let next = journal.state().unwrap().vector[&node] + 1;
    let segment = fs::read_dir(dir.path().join("sync/journal").join(&node))
        .unwrap()
        .find_map(|entry| {
            let path = entry.unwrap().path();
            path.extension()
                .is_some_and(|ext| ext == "jsonl")
                .then_some(path)
        })
        .unwrap();
    let start = fs::metadata(&segment).unwrap().len();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap()
        .write_all(b"{\"op\":")
        .unwrap();
    let updated = text.replace("title: old", "title: new");
    fs::write(&path, &updated).unwrap();
    let intent = serde_json::json!({
        "path": path,
        "external": false,
        "segment": segment.file_name().unwrap().to_string_lossy(),
        "start_offset": start,
        "through_seq": next,
        "file": true
    });
    fs::write(dir.path().join("sync/intent.json"), intent.to_string()).unwrap();
    Journal::open(dir.path()).unwrap();
    assert!(fs::read_to_string(&path).unwrap().contains("title: new"));
    let count = operations(dir.path(), &node).len();
    assert_eq!(journal.state().unwrap().vector[&node], next);
    assert!(!segment.file_name().unwrap().to_string_lossy().is_empty());
    let segment_text = fs::read_to_string(&segment).unwrap();
    assert!(!segment_text.contains("{\"op\":"));
    fs::write(dir.path().join("sync/intent.json"), intent.to_string()).unwrap();
    Journal::open(dir.path()).unwrap();
    assert_eq!(operations(dir.path(), &node).len(), count);
}

#[test]
fn concurrent_board_writers_share_one_monotonic_node_sequence() {
    let dir = tempfile::tempdir().unwrap();
    kanban4ai::core::sync::enable_sync(dir.path()).unwrap();
    let mut paths = Vec::new();
    for index in 0..8 {
        let root = dir.path().join(format!("projects/board-{index}"));
        let storage = Storage::new(root);
        storage.init_board().unwrap();
        paths.push(storage.tasks_dir.join("todo/TASK-001.md"));
    }
    let journal = Journal::open(dir.path()).unwrap();
    let start = journal.state().unwrap().vector[&journal.node.node_id];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(paths.len()));
    let writers: Vec<_> = paths
        .into_iter()
        .map(|path| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                atomic_write_text(
                    &path,
                    "---\nid: TASK-001\ntitle: concurrent\nstatus: todo\n---\nbody\n",
                )
                .unwrap();
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let ops = operations(dir.path(), &journal.node.node_id);
    let writes: Vec<_> = ops.iter().filter(|op| op.entity == "task").collect();
    assert_eq!(writes.len(), 8);
    let projects: std::collections::BTreeSet<_> = writes.iter().map(|op| &op.project).collect();
    assert_eq!(projects.len(), 8);
    for (index, op) in writes.iter().enumerate() {
        assert_eq!(
            op.op,
            format!("{}:{}", journal.node.node_id, start + index as u64 + 1)
        );
    }
    assert!(writes.windows(2).all(|pair| pair[0].hlc < pair[1].hlc));
    assert_eq!(
        journal.state().unwrap().vector[&journal.node.node_id],
        start + 8
    );
}
