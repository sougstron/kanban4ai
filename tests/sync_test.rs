use kanban4ai::core::storage::{Storage, atomic_write_text};
use kanban4ai::core::sync::{Hlc, Journal, Operation};
use std::collections::BTreeMap;
use std::fs;

fn operations(store: &std::path::Path, node: &str) -> Vec<Operation> {
    let path = store.join("sync/journal").join(format!("{node}.jsonl"));
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
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
    assert_eq!(journal.compact(&[BTreeMap::new()]).unwrap(), 0);
    let seq = vector[node];
    assert_eq!(
        journal
            .compact(&[BTreeMap::from([(node.clone(), seq - 1)])])
            .unwrap(),
        seq - 1
    );
    assert_eq!(operations(dir.path(), node).len(), 1);
    let snapshot: kanban4ai::core::sync::JournalState = serde_json::from_str(
        &fs::read_to_string(dir.path().join("sync/snapshot/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot.vector[node], seq - 1);
    assert_eq!(journal.compact(&[vector]).unwrap(), seq);
    assert!(operations(dir.path(), node).is_empty());
    storage.delete_task("TASK-001").unwrap();
    let deleted = operations(dir.path(), node).pop().unwrap();
    assert_eq!(deleted.entity, "tombstone");
    assert_eq!(deleted.set["deleted"], true);
}

#[test]
fn message_uids_parent_links_and_dotted_config_diffs() {
    let dir = tempfile::tempdir().unwrap();
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
        .take(2)
        .collect();
    assert!(tombstones.iter().all(|op| op.entity == "tombstone"));
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
fn redo_recovers_a_torn_append_and_never_duplicates_committed_operations() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let root = dir.path().join("projects/demo");
    Storage::new(&root).init_board().unwrap();
    let path = root.join(".kanban/tasks/todo/TASK-001.md");
    let text = "---\nid: TASK-001\ntitle: old\nstatus: todo\n---\nbody\n";
    atomic_write_text(&path, text).unwrap();
    let mut state = journal.state().unwrap();
    let mut op = operations(dir.path(), &journal.node.node_id).pop().unwrap();
    let next = state.vector[&journal.node.node_id] + 1;
    op.op = format!("{}:{next}", journal.node.node_id);
    op.hlc = op.hlc.tick(0, &journal.node.node_id).unwrap();
    op.set = BTreeMap::from([("title".into(), serde_json::json!("new"))]);
    state.vector.insert(journal.node.node_id.clone(), next);
    let pending = serde_json::json!({"path": path, "content": text.replace("title: old", "title: new"), "operations": [op], "state": state});
    fs::write(dir.path().join("sync/pending.json"), pending.to_string()).unwrap();
    let log = dir
        .path()
        .join("sync/journal")
        .join(format!("{}.jsonl", journal.node.node_id));
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"{\"op\":")
        .unwrap();
    Journal::open(dir.path()).unwrap();
    assert!(fs::read_to_string(&path).unwrap().contains("title: new"));
    let count = operations(dir.path(), &journal.node.node_id).len();
    assert_eq!(journal.state().unwrap().vector[&journal.node.node_id], next);
    fs::write(dir.path().join("sync/pending.json"), pending.to_string()).unwrap();
    Journal::open(dir.path()).unwrap();
    assert_eq!(operations(dir.path(), &journal.node.node_id).len(), count);
}

#[test]
fn concurrent_board_writers_share_one_monotonic_node_sequence() {
    let dir = tempfile::tempdir().unwrap();
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
