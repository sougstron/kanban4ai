//! One-shot generator for `tests/fixtures/sync-v1`, a store written by the v1
//! journal (single `journal/<node>.jsonl` plus `meta/state.json`). Regenerate
//! only with that writer; the migration test consumes the committed bytes.
use kanban4ai::core::models::{Message, MessageKind, MessageRole};
use kanban4ai::core::storage::{NewTask, Storage, atomic_write_text};
use kanban4ai::core::sync::{Hlc, Journal, NodeIdentity, Operation};
use kanban4ai::core::thread::ThreadManager;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn op(
    node: &str,
    seq: u64,
    store: &str,
    project: &str,
    entity: &str,
    id: &str,
    set: Value,
) -> Operation {
    Operation {
        op: format!("{node}:{seq}"),
        hlc: Hlc(1_700_000_000_000 + seq, seq, node.into()),
        store: store.into(),
        project: project.into(),
        entity: entity.into(),
        id: id.into(),
        set: serde_json::from_value(set).unwrap(),
        prev: BTreeMap::new(),
        actor: "human".into(),
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text.ends_with(".lock") || text == "pending.json" || text == "remote-pending.json" {
            continue;
        }
        let dest = to.join(&name);
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[test]
#[ignore = "rewrites tests/fixtures/sync-v1 from the v1 journal writer"]
fn generate_sync_v1_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("projects/demo");
    let storage = Storage::new(&board);
    storage.init_board().unwrap();
    atomic_write_text(
        &board.join("project.yaml"),
        "id: demo\nname: Demo\ncreated_at: '2026-01-01T00:00:00'\nwork_path: ''\n",
    )
    .unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let task = storage.create_task(NewTask::titled("base")).unwrap();
    let mut moved = storage.load_task(&task.id).unwrap().unwrap();
    moved.status = kanban4ai::core::models::TaskStatus::Review;
    storage.save_task(&moved).unwrap();
    let threads = ThreadManager::new(&board).unwrap();
    let mut thread = threads.load(&task.id).unwrap();
    let mut question = Message::new(
        threads.next_msg_id(&task.id).unwrap(),
        MessageRole::Human,
        MessageKind::Question,
        "question",
    );
    question.author = Some("user".into());
    thread.messages.push(question);
    threads.save(&task.id, &mut thread).unwrap();
    let mut thread = threads.load(&task.id).unwrap();
    let parent = thread.messages.last().unwrap().id.clone();
    let mut reply = Message::new(
        threads.next_msg_id(&task.id).unwrap(),
        MessageRole::Human,
        MessageKind::Question,
        "reply",
    );
    reply.author = Some("user".into());
    reply.parent_id = Some(parent);
    thread.messages.push(reply);
    threads.save(&task.id, &mut thread).unwrap();
    storage.delete_task(&task.id).unwrap();
    let restored = board.join(".kanban/tasks/todo/TASK-001.md");
    atomic_write_text(
        &restored,
        "---\nid: TASK-001\ntitle: restored\nstatus: todo\n---\nbody\n",
    )
    .unwrap();
    let image = board.join(".kanban/assets/images/shot.bin");
    fs::create_dir_all(image.parent().unwrap()).unwrap();
    fs::write(&image, b"fixture-bytes").unwrap();
    journal.reconcile().unwrap();

    let state = journal.state().unwrap();
    let project = state.project_uids["demo"].clone();
    let task_uid = state.task_uids.values().next().expect("task uid").clone();
    let store_id = journal.node.store_id.clone();
    let local_node = journal.node.node_id.clone();

    // A foreign runner records the descriptor. Local sequence stays with capture.
    let run = op(
        "runner-node",
        1,
        &store_id,
        &project,
        "run",
        &task_uid,
        json!({"runner-node": {
            "node_id": "runner-node",
            "name": "laptop",
            "role": "laptop",
            "gen": 1,
            "state": "finished",
            "phase": "executor",
            "started_at": "2026-01-01T00:00:00",
            "branch": null,
            "heartbeat": "2026-01-01T00:00:01",
            "code": null
        }}),
    );
    journal.apply_remote(&[run], u64::MAX).unwrap();
    let landing = op(
        "runner-node",
        2,
        &store_id,
        &project,
        "run",
        &task_uid,
        json!({"landing:runner-node": {
            "name": "laptop",
            "generation": 1,
            "tip": "0123456789abcdef0123456789abcdef01234567",
            "pending": false
        }}),
    );
    journal.apply_remote(&[landing], u64::MAX).unwrap();

    // Hole: seq 2 arrives before seq 1 and stays out of the contiguous vector.
    let hole = op(
        "gap-node",
        2,
        &store_id,
        &project,
        "task",
        &task_uid,
        json!({"priority": 1}),
    );
    journal.apply_remote(&[hole], u64::MAX).unwrap();

    // Second node shares the project, claims TASK-001 before seeing ours, then
    // both journals meet. One display id becomes an alias.
    let other = tempfile::tempdir().unwrap();
    let peer = Journal::open(other.path()).unwrap();
    let mut identity: NodeIdentity =
        serde_yaml_ng::from_str(&fs::read_to_string(other.path().join("sync/node.yaml")).unwrap())
            .unwrap();
    identity.store_id = store_id.clone();
    identity.name = "server".into();
    fs::write(
        other.path().join("sync/node.yaml"),
        serde_yaml_ng::to_string(&identity).unwrap(),
    )
    .unwrap();
    let peer = Journal::open(other.path()).unwrap();
    Storage::new(other.path().join("projects/demo"))
        .init_board()
        .unwrap();
    let project_ops: Vec<Operation> = fs::read_to_string(
        dir.path()
            .join("sync/journal")
            .join(format!("{local_node}.jsonl")),
    )
    .unwrap()
    .lines()
    .filter(|line| !line.is_empty())
    .map(|line| serde_json::from_str(line).unwrap())
    .filter(|op: &Operation| op.entity == "project")
    .collect();
    peer.apply_remote(&project_ops, u64::MAX).unwrap();
    Storage::new(other.path().join("projects/demo"))
        .create_task(NewTask::titled("colliding"))
        .unwrap();
    let peer_ops: Vec<Operation> = fs::read_to_string(
        other
            .path()
            .join("sync/journal")
            .join(format!("{}.jsonl", peer.node.node_id)),
    )
    .unwrap()
    .lines()
    .filter(|line| !line.is_empty())
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
    journal.apply_remote(&peer_ops, u64::MAX).unwrap();
    // Concurrent title edit: the peer did not see the restored title.
    let mut conflict = op(
        &peer.node.node_id,
        peer.state().unwrap().vector[&peer.node.node_id] + 1,
        &store_id,
        &project,
        "task",
        &task_uid,
        json!({"title": "from-peer"}),
    );
    conflict.hlc = Hlc(1_700_000_200_000, 0, peer.node.node_id.clone());
    conflict
        .prev
        .insert("title".into(), Hlc(1, 0, "nobody".into()));
    journal.apply_remote(&[conflict], u64::MAX).unwrap();
    assert!(
        !journal.conflicts().unwrap().is_empty(),
        "fixture needs one live conflict"
    );

    let dest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sync-v1");
    let _ = fs::remove_dir_all(&dest);
    copy_tree(dir.path(), &dest);
    let state = fs::read_to_string(dest.join("sync/meta/state.json")).unwrap();
    assert!(state.contains("\"operations\""));
    assert!(
        dest.join("sync/journal")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
    eprintln!("wrote {}", dest.display());
}
