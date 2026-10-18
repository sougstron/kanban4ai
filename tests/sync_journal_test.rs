//! Format-2 journal: torn appends, segments, pruning, versions, v1 migration.
use kanban4ai::core::storage::{Storage, atomic_write_text};
use kanban4ai::core::sync::{
    Hlc, Journal, JournalState, Operation, accept_store_protocol, negotiate,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn task(title: &str) -> String {
    format!("---\nid: TASK-001\ntitle: {title}\nstatus: todo\n---\nbody\n")
}

fn board(store: &Path) -> Storage {
    kanban4ai::core::sync::enable_sync(store).unwrap();
    let root = store.join("projects/demo");
    let storage = Storage::new(&root);
    storage.init_board().unwrap();
    atomic_write_text(
        &root.join("project.yaml"),
        "id: demo\nname: Demo\ncreated_at: '2026-01-01T00:00:00'\n",
    )
    .unwrap();
    storage
}

fn limits(store: &Path, segment_bytes: u64) {
    let path = store.join("config.yaml");
    let mut config: serde_yaml_ng::Value = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_yaml_ng::from_str(&text).ok())
        .unwrap_or_else(|| serde_yaml_ng::Value::Mapping(Default::default()));
    if !config.is_mapping() {
        config = serde_yaml_ng::Value::Mapping(Default::default());
    }
    if !config.get("sync").is_some_and(|sync| sync.is_mapping()) {
        config["sync"] = serde_yaml_ng::Value::Mapping(Default::default());
    }
    if !config["sync"]
        .get("journal")
        .is_some_and(|journal| journal.is_mapping())
    {
        config["sync"]["journal"] = serde_yaml_ng::Value::Mapping(Default::default());
    }
    config["sync"]["journal"]["segment_bytes"] = serde_yaml_ng::Value::Number(segment_bytes.into());
    config["sync"]["journal"]["snapshot_ops"] = serde_yaml_ng::Value::Number(1_000_000.into());
    config["sync"]["journal"]["snapshot_secs"] = serde_yaml_ng::Value::Number(864_000.into());
    fs::write(&path, serde_yaml_ng::to_string(&config).unwrap()).unwrap();
}

fn peer_store(dir: &Path, source: &Journal) -> Journal {
    let journal = Journal::open(dir).unwrap();
    let mut node = journal.node.clone();
    node.store_id = source.node.store_id.clone();
    fs::write(
        dir.join("sync/node.yaml"),
        serde_yaml_ng::to_string(&node).unwrap(),
    )
    .unwrap();
    Journal::open(dir).unwrap()
}

fn op(node: &str, seq: u64, wall: u64, entity: &str, id: &str, set: Value) -> Operation {
    Operation {
        v: 1,
        op: format!("{node}:{seq}"),
        hlc: Hlc(wall, 0, node.into()),
        store: String::new(),
        project: "project-uid".into(),
        entity: entity.into(),
        id: id.into(),
        set: serde_json::from_value(set).unwrap(),
        prev: BTreeMap::new(),
        actor: "human".into(),
        extra: BTreeMap::new(),
    }
}

fn tree_bytes(root: &Path) -> u64 {
    let mut total = 0;
    if !root.exists() {
        return 0;
    }
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            total += tree_bytes(&path);
        } else {
            total += fs::metadata(&path).unwrap().len();
        }
    }
    total
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn active_segment(store: &Path, node: &str) -> PathBuf {
    let dir = store.join("sync/journal").join(node);
    fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.len() == 20)
        })
        .max()
        .unwrap()
}

#[test]
fn torn_append_reconciles_one_operation_without_a_hole() {
    let dir = tempfile::tempdir().unwrap();
    let storage = board(dir.path());
    let path = storage.tasks_dir.join("todo/TASK-001.md");
    atomic_write_text(&path, &task("old")).unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let node = journal.node.node_id.clone();
    let before = journal.operations(Some(&node)).unwrap();
    let next = journal.state().unwrap().vector[&node] + 1;
    let segment = active_segment(dir.path(), &node);
    let start = fs::metadata(&segment).unwrap().len();
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap()
        .write_all(b"{\"op\":")
        .unwrap();
    fs::write(&path, task("new")).unwrap();
    let intent = json!({
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
    assert!(!dir.path().join("sync/intent.json").exists());
    let ops = journal.operations(Some(&node)).unwrap();
    assert_eq!(ops.len(), before.len() + 1);
    let seqs: Vec<u64> = ops
        .iter()
        .map(|op| op.op.rsplit_once(':').unwrap().1.parse().unwrap())
        .collect();
    assert_eq!(seqs, (1..=next).collect::<Vec<_>>());
    assert!(
        !fs::read(&segment)
            .unwrap()
            .windows(6)
            .any(|w| w == b"{\"op\":")
    );
    fs::write(dir.path().join("sync/intent.json"), intent.to_string()).unwrap();
    Journal::open(dir.path()).unwrap();
    assert_eq!(journal.operations(Some(&node)).unwrap().len(), ops.len());
}

/// Format 5 splits `meta/uids.json` per project: a format 4 store migrates on
/// open, and a write rewrites only its own project's table file.
#[test]
fn uid_tables_split_per_project_and_writes_touch_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let demo = board(dir.path());
    let other_root = dir.path().join("projects/other");
    let other = Storage::new(&other_root);
    other.init_board().unwrap();
    atomic_write_text(
        &other_root.join("project.yaml"),
        "id: other\nname: Other\ncreated_at: '2026-01-01T00:00:00'\n",
    )
    .unwrap();
    for storage in [&demo, &other] {
        atomic_write_text(
            &storage.threads_dir.join("TASK-001.yaml"),
            "task_id: TASK-001\nrev: 1\nmessages:\n- id: MSG-001\n  body: question\n",
        )
        .unwrap();
    }
    let state = Journal::open(dir.path()).unwrap().state().unwrap();
    assert_eq!(state.project_uids.len(), 2);

    // Back to format 4: one uid file, no per-project directory.
    let meta = dir.path().join("sync/meta");
    fs::remove_dir_all(meta.join("uids")).unwrap();
    let legacy = json!({
        "project_uids": state.project_uids,
        "message_uids": state.message_uids,
        "task_uids": state.task_uids,
        "task_aliases": state.task_aliases,
    });
    fs::write(meta.join("uids.json"), legacy.to_string()).unwrap();
    fs::write(dir.path().join("sync/FORMAT"), "4\n").unwrap();

    let journal = Journal::open(dir.path()).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("sync/FORMAT"))
            .unwrap()
            .trim(),
        "5"
    );
    assert!(!meta.join("uids.json").exists());
    let files = |root: &Path| -> BTreeMap<PathBuf, Vec<u8>> {
        fs::read_dir(root.join("sync/meta/uids"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect()
    };
    let before = files(dir.path());
    assert_eq!(before.len(), 3, "an index plus one file per project");
    let migrated = journal.state().unwrap();
    assert_eq!(migrated.message_uids, state.message_uids);
    assert_eq!(migrated.task_uids, state.task_uids);
    assert_eq!(migrated.project_uids, state.project_uids);

    atomic_write_text(
        &demo.threads_dir.join("TASK-001.yaml"),
        "task_id: TASK-001\nrev: 2\nmessages:\n- id: MSG-001\n  body: question\n- id: MSG-002\n  body: answer\n",
    )
    .unwrap();
    let after = files(dir.path());
    let changed: Vec<_> = before
        .iter()
        .filter(|(path, bytes)| after.get(*path) != Some(*bytes))
        .collect();
    assert_eq!(changed.len(), 1, "only the demo table file is rewritten");
    let state = Journal::open(dir.path()).unwrap().state().unwrap();
    assert_eq!(state.message_uids.len(), 3);
    assert!(!meta.join("uids.json").exists());
}

#[test]
fn committed_intent_reapplies_uids_once() {
    let dir = tempfile::tempdir().unwrap();
    let storage = board(dir.path());
    let path = storage.threads_dir.join("TASK-001.yaml");
    atomic_write_text(
        &path,
        "task_id: TASK-001\nrev: 1\nmessages:\n- id: MSG-001\n  body: question\n",
    )
    .unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let node = journal.node.node_id.clone();
    let state = journal.state().unwrap();
    let count = journal.operations(Some(&node)).unwrap().len();
    let uids = state.message_uids.clone();
    assert!(!uids.is_empty());
    fs::remove_dir_all(dir.path().join("sync/meta/uids")).unwrap();
    let intent = json!({
        "path": "",
        "segment": "",
        "start_offset": 0,
        "through_seq": state.vector[&node],
        "file": false,
        "uids": { "message": { "set": uids, "remove": [] } }
    });
    fs::write(dir.path().join("sync/intent.json"), intent.to_string()).unwrap();
    let restored = Journal::open(dir.path()).unwrap().state().unwrap();
    assert_eq!(restored.message_uids, state.message_uids);
    assert_eq!(journal.operations(Some(&node)).unwrap().len(), count);
    fs::write(dir.path().join("sync/intent.json"), intent.to_string()).unwrap();
    Journal::open(dir.path()).unwrap();
    assert_eq!(journal.operations(Some(&node)).unwrap().len(), count);
    assert_eq!(
        Journal::open(dir.path())
            .unwrap()
            .state()
            .unwrap()
            .message_uids,
        state.message_uids
    );
}

#[test]
fn remote_pending_appends_a_missing_operation_once() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    board(dir.path());
    let mut incoming = op(
        "peer-b",
        1,
        50,
        "task",
        "TASK-001",
        json!({"title": "remote"}),
    );
    incoming.store = journal.node.store_id.clone();
    incoming.project = journal.state().unwrap().project_uids["demo"].clone();
    let pending = json!({
        "boards": [],
        "writes": {},
        "operations": [incoming]
    });
    fs::write(
        dir.path().join("sync/remote-pending.json"),
        pending.to_string(),
    )
    .unwrap();
    Journal::open(dir.path()).unwrap();
    assert!(!dir.path().join("sync/remote-pending.json").exists());
    assert_eq!(journal.operations(Some("peer-b")).unwrap().len(), 1);
    fs::write(
        dir.path().join("sync/remote-pending.json"),
        pending.to_string(),
    )
    .unwrap();
    Journal::open(dir.path()).unwrap();
    assert_eq!(journal.operations(Some("peer-b")).unwrap().len(), 1);
    journal.apply_remote(&[incoming], u64::MAX).unwrap();
    assert_eq!(journal.operations(Some("peer-b")).unwrap().len(), 1);
}

#[test]
fn deleting_projection_or_index_rebuilds_the_same_state() {
    let dir = tempfile::tempdir().unwrap();
    let storage = board(dir.path());
    atomic_write_text(&storage.tasks_dir.join("todo/TASK-001.md"), &task("kept")).unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    kanban4ai::core::sync::transport::snapshot_spool(dir.path()).unwrap();
    let before = serde_json::to_value(&*journal.state().unwrap()).unwrap();
    let shards = dir.path().join("sync/meta/projection");
    let saved = tempfile::tempdir().unwrap();
    copy_tree(&shards, saved.path());
    fs::remove_dir_all(&shards).unwrap();
    fs::remove_file(dir.path().join("sync/meta/index.json")).unwrap();
    let after =
        serde_json::to_value(&*Journal::open(dir.path()).unwrap().state().unwrap()).unwrap();
    assert_eq!(before, after);
    atomic_write_text(&storage.tasks_dir.join("todo/TASK-001.md"), &task("later")).unwrap();
    let full = serde_json::to_value(&*journal.state().unwrap()).unwrap();
    // No checkpoint was due since the deletion, so the directory may be absent.
    let _ = fs::remove_dir_all(&shards);
    copy_tree(saved.path(), &shards);
    let replayed =
        serde_json::to_value(&*Journal::open(dir.path()).unwrap().state().unwrap()).unwrap();
    assert_eq!(replayed, full);
}

#[test]
fn segments_roll_and_ahead_drains_when_the_hole_closes() {
    let dir = tempfile::tempdir().unwrap();
    limits(dir.path(), 400);
    let storage = board(dir.path());
    let path = storage.tasks_dir.join("todo/TASK-001.md");
    atomic_write_text(&path, &task("n0")).unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    for n in 1..30 {
        let mut task = storage.load_task("TASK-001").unwrap().unwrap();
        task.title = format!("n{n}");
        storage.save_task(&task).unwrap();
    }
    let node = journal.node.node_id.clone();
    let segments = fs::read_dir(dir.path().join("sync/journal").join(&node))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "jsonl")
        })
        .count();
    assert!(segments >= 2, "segments={segments}");
    let project = journal.state().unwrap().project_uids["demo"].clone();
    let id = journal
        .operations(Some(&node))
        .unwrap()
        .iter()
        .find(|op| op.entity == "task")
        .unwrap()
        .id
        .clone();
    let mut second = op("gap-node", 2, 90, "task", &id, json!({"title": "hole"}));
    second.store = journal.node.store_id.clone();
    second.project = project.clone();
    journal.apply_remote(&[second.clone()], u64::MAX).unwrap();
    assert!(
        dir.path()
            .join("sync/journal/gap-node/ahead.jsonl")
            .is_file()
    );
    assert_eq!(
        journal
            .state()
            .unwrap()
            .vector
            .get("gap-node")
            .copied()
            .unwrap_or(0),
        0
    );
    let mut first = op("gap-node", 1, 80, "task", &id, json!({"priority": "low"}));
    first.store = journal.node.store_id.clone();
    first.project = project;
    journal.apply_remote(&[first], u64::MAX).unwrap();
    assert!(
        !dir.path()
            .join("sync/journal/gap-node/ahead.jsonl")
            .exists()
    );
    assert_eq!(journal.state().unwrap().vector["gap-node"], 2);
    assert_eq!(journal.operations(Some("gap-node")).unwrap().len(), 2);
}

#[test]
fn operations_after_matches_the_hlc_oracle_and_refuses_a_pruned_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    board(dir.path());
    let cursor = journal.state().unwrap().vector.clone();
    let project = journal.state().unwrap().project_uids["demo"].clone();
    let mut made = Vec::new();
    for node in ["node-a", "node-b", "node-c"] {
        for seq in 1..=12u64 {
            let entity = if seq % 5 == 0 { "tombstone" } else { "task" };
            let mut item = op(
                node,
                seq,
                1000 + seq * 3 + node.as_bytes()[5] as u64,
                entity,
                "TASK-001",
                if entity == "tombstone" {
                    json!({"deleted": true, "entity": "task"})
                } else {
                    json!({"title": format!("{node}-{seq}")})
                },
            );
            item.store = journal.node.store_id.clone();
            item.project = project.clone();
            made.push(item);
        }
    }
    let mut order = made.clone();
    let count = order.len();
    for i in 0..count {
        order.swap(i, (i * 17 + 3) % count);
    }
    for item in &order {
        journal
            .apply_remote(std::slice::from_ref(item), u64::MAX)
            .unwrap();
    }
    let mut oracle = made;
    oracle.retain(|op| {
        let seq: u64 = op.op.rsplit_once(':').unwrap().1.parse().unwrap();
        seq > cursor.get(&op.hlc.2).copied().unwrap_or(0)
    });
    oracle.sort_by(|a, b| a.hlc.cmp(&b.hlc).then_with(|| a.op.cmp(&b.op)));
    for limit in [1usize, 4, 7, 20, 100] {
        let page = journal.operations_after(&cursor, limit).unwrap();
        let mut expected = oracle.clone();
        let more = expected.len() > limit;
        expected.truncate(limit);
        expected.sort_by_key(|op| op.entity != "tombstone");
        assert_eq!(page.more, more);
        assert_eq!(page.operations, expected);
    }
    let vector = journal.state().unwrap().vector.clone();
    let mut acks = BTreeMap::new();
    for node in vector.keys() {
        if node != &journal.node.node_id {
            acks.insert(node.clone(), vector.clone());
        }
    }
    journal.prune(&acks).unwrap();
    let err = journal
        .operations_after(&BTreeMap::new(), 10)
        .unwrap_err()
        .to_string();
    assert!(err.contains("SYNC_SNAPSHOT_REQUIRED"), "{err}");
    let page = journal.operations_after(&vector, 10).unwrap();
    assert!(page.operations.is_empty());
}

#[test]
fn prune_bounds_acked_history_and_keeps_conflicts_and_fresh_blobs() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    limits(a.path(), 700);
    limits(b.path(), 700);
    let storage = board(a.path());
    atomic_write_text(&storage.tasks_dir.join("todo/TASK-001.md"), &task("base")).unwrap();
    let ja = Journal::open(a.path()).unwrap();
    let jb = peer_store(b.path(), &ja);
    let mut cursor = BTreeMap::new();
    let mut peak = 0u64;
    for batch in 0..40 {
        for n in 0..50 {
            let mut task = storage.load_task("TASK-001").unwrap().unwrap();
            task.title = format!("e{batch}-{n}");
            storage.save_task(&task).unwrap();
        }
        let page = ja.operations_after(&cursor, usize::MAX).unwrap();
        if !page.operations.is_empty() {
            jb.apply_remote(&page.operations, u64::MAX).unwrap();
            cursor = page.next;
        }
        let ack = ja.state().unwrap().vector.clone();
        ja.prune(&BTreeMap::from([(jb.node.node_id.clone(), ack.clone())]))
            .unwrap();
        jb.prune(&BTreeMap::from([(ja.node.node_id.clone(), ack)]))
            .unwrap();
        let size =
            tree_bytes(&a.path().join("sync/journal")) + tree_bytes(&a.path().join("sync/meta"));
        peak = peak.max(size);
    }
    assert!(peak < 1_500_000, "journal grew linearly: {peak} bytes");
    assert!(
        tree_bytes(&a.path().join("sync/journal")) + tree_bytes(&a.path().join("sync/meta"))
            < 1_500_000
    );

    let blocked = tempfile::tempdir().unwrap();
    let journal = Journal::open(blocked.path()).unwrap();
    let mut foreign = op(
        "other-node",
        1,
        10,
        "task",
        "TASK-009",
        json!({"title": "stay"}),
    );
    foreign.store = journal.node.store_id.clone();
    journal.apply_remote(&[foreign], u64::MAX).unwrap();
    journal.prune(&BTreeMap::new()).unwrap();
    assert_eq!(journal.operations(Some("other-node")).unwrap().len(), 1);

    let conflict_dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(conflict_dir.path()).unwrap();
    board(conflict_dir.path());
    let project = journal.state().unwrap().project_uids["demo"].clone();
    let wall = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut left = op(
        "node-a",
        1,
        wall,
        "task",
        "TASK-001",
        json!({"title": "left"}),
    );
    let mut right = op(
        "node-b",
        1,
        wall + 1,
        "task",
        "TASK-001",
        json!({"title": "right"}),
    );
    for item in [&mut left, &mut right] {
        item.store = journal.node.store_id.clone();
        item.project = project.clone();
    }
    journal.apply_remote(&[left.clone()], u64::MAX).unwrap();
    let seen = journal.state().unwrap().vector.clone();
    let mut lagging = seen.clone();
    lagging.insert("node-b".into(), 1);
    journal
        .prune(&BTreeMap::from([
            ("node-a".into(), lagging),
            (
                "node-b".into(),
                BTreeMap::from([("node-a".into(), 1u64), ("node-b".into(), 5u64)]),
            ),
        ]))
        .unwrap();
    assert_eq!(journal.operations(Some("node-a")).unwrap().len(), 1);
    journal.apply_remote(&[right], u64::MAX).unwrap();
    assert!(!journal.conflicts().unwrap().is_empty());
    let vector = journal.state().unwrap().vector.clone();
    journal
        .prune(&BTreeMap::from([
            ("node-a".into(), vector.clone()),
            ("node-b".into(), vector),
        ]))
        .unwrap();
    assert!(!journal.conflicts().unwrap().is_empty());

    let blobs = conflict_dir.path().join("sync/blobs");
    fs::create_dir_all(&blobs).unwrap();
    let old = blobs.join("ab".repeat(32));
    let young = blobs.join("cd".repeat(32));
    fs::write(&old, b"old").unwrap();
    fs::write(&young, b"young").unwrap();
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .unwrap();
    journal.prune(&BTreeMap::new()).unwrap();
    assert!(!old.exists());
    assert!(young.exists());
}

#[test]
fn newer_store_format_is_refused_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let storage = board(dir.path());
    fs::write(dir.path().join("sync/FORMAT"), "6\n").unwrap();
    let stamp = |root: &Path| {
        let mut names = fs::read_dir(root.join("sync"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let before = stamp(dir.path());
    let err = match Journal::open(dir.path()) {
        Err(err) => err.to_string(),
        Ok(_) => panic!("opened a store newer than this binary"),
    };
    assert!(err.contains("SYNC_STORE_TOO_NEW"), "{err}");
    let path = storage.tasks_dir.join("todo/TASK-009.md");
    let err = atomic_write_text(&path, &task("nope"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("SYNC_STORE_TOO_NEW"), "{err}");
    assert!(!path.exists());
    assert_eq!(before, stamp(dir.path()));
}

#[test]
fn v1_fixture_migrates_and_a_partial_migration_restarts() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sync-v1");
    #[derive(serde::Deserialize)]
    struct V1 {
        vector: BTreeMap<String, u64>,
        fields: BTreeMap<String, Value>,
        #[serde(default)]
        project_uids: BTreeMap<String, String>,
        #[serde(default)]
        message_uids: BTreeMap<String, String>,
        #[serde(default)]
        task_uids: BTreeMap<String, String>,
        #[serde(default)]
        task_aliases: BTreeMap<String, String>,
        operations: BTreeMap<String, Operation>,
    }
    let v1: V1 =
        serde_json::from_str(&fs::read_to_string(fixture.join("sync/meta/state.json")).unwrap())
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    copy_tree(&fixture, dir.path());
    let journal = Journal::open(dir.path()).unwrap();
    let state = journal.state().unwrap();
    assert_eq!(state.vector, v1.vector);
    assert_eq!(
        serde_json::to_value(&state.fields).unwrap(),
        serde_json::to_value(&v1.fields).unwrap()
    );
    assert_eq!(state.project_uids, v1.project_uids);
    assert_eq!(state.message_uids, v1.message_uids);
    assert_eq!(state.task_uids, v1.task_uids);
    assert_eq!(state.task_aliases, v1.task_aliases);
    let conflicts =
        fs::read_to_string(fixture.join("sync/meta/conflicts.jsonl")).unwrap_or_default();
    let migrated =
        fs::read_to_string(dir.path().join("sync/meta/conflicts.jsonl")).unwrap_or_default();
    assert_eq!(
        migrated.lines().filter(|l| !l.is_empty()).count(),
        conflicts.lines().filter(|l| !l.is_empty()).count()
    );
    let mut contiguous: Vec<Operation> = v1
        .operations
        .values()
        .filter(|op| {
            let seq: u64 = op.op.rsplit_once(':').unwrap().1.parse().unwrap();
            seq <= v1.vector.get(&op.hlc.2).copied().unwrap_or(0)
        })
        .cloned()
        .collect();
    contiguous.sort_by(|a, b| a.hlc.cmp(&b.hlc).then_with(|| a.op.cmp(&b.op)));
    let page = journal
        .operations_after(&BTreeMap::new(), usize::MAX)
        .unwrap();
    assert_eq!(page.operations.len(), contiguous.len());
    assert!(dir.path().join("sync/FORMAT").is_file());
    let stub = fs::read_to_string(dir.path().join("sync/meta/state.json")).unwrap();
    assert!(serde_json::from_str::<JournalState>(&stub).is_err());
    assert!(stub.contains("upgrade kanban4ai"));

    let again = tempfile::tempdir().unwrap();
    copy_tree(&fixture, again.path());
    let node = v1.vector.keys().next().unwrap();
    fs::create_dir_all(again.path().join("sync/journal").join(node)).unwrap();
    fs::write(again.path().join("sync/meta/projection.json"), "{}\n").unwrap();
    let journal = Journal::open(again.path()).unwrap();
    assert_eq!(journal.state().unwrap().vector, v1.vector);
    assert_eq!(
        fs::read_to_string(again.path().join("sync/FORMAT"))
            .unwrap()
            .trim(),
        "5"
    );
}

#[test]
fn unknown_op_fields_round_trip_and_versions_are_negotiated() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    board(dir.path());
    let mut item = op("peer-z", 1, 5, "task", "TASK-001", json!({"title": "kept"}));
    item.store = journal.node.store_id.clone();
    item.project = journal.state().unwrap().project_uids["demo"].clone();
    let mut value = serde_json::to_value(&item).unwrap();
    value["custom_hint"] = json!("keep-me");
    let item: Operation = serde_json::from_value(value).unwrap();
    journal.apply_remote(&[item.clone()], u64::MAX).unwrap();
    let page = journal.operations_after(&BTreeMap::new(), 10).unwrap();
    let text = serde_json::to_string(&page.operations).unwrap();
    assert!(text.contains("custom_hint"), "{text}");
    assert!(text.contains("keep-me"));
    let mut newer = item.clone();
    newer.v = 2;
    newer.op = "peer-z:2".into();
    newer.hlc = Hlc(6, 0, "peer-z".into());
    let err = journal
        .apply_remote(&[newer], u64::MAX)
        .unwrap_err()
        .to_string();
    assert!(err.contains("SYNC_OP_FORMAT_TOO_NEW"), "{err}");
    // Protocol 3 dropped peer exchange: older nodes are refused, not downgraded.
    assert!(accept_store_protocol(0, 0).is_err());
    assert!(accept_store_protocol(3, 3).is_ok());
    assert!(negotiate(1, 2, 0, 0).is_some());
    let err = accept_store_protocol(0, 99).unwrap_err().to_string();
    assert!(err.contains("SYNC_PROTOCOL_MISMATCH"), "{err}");
}

#[test]
fn a_prune_that_frees_nothing_rewrites_nothing_and_reads_share_the_projection() {
    let dir = tempfile::tempdir().unwrap();
    let storage = board(dir.path());
    atomic_write_text(&storage.tasks_dir.join("todo/TASK-001.md"), &task("base")).unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    journal.reconcile().unwrap();
    // A participant that acked nothing holds the whole history.
    let held = BTreeMap::from([("peer".to_owned(), BTreeMap::new())]);
    journal.prune(&held).unwrap();
    let meta = dir.path().join("sync/meta");
    let stamp = || {
        let mut files = Vec::new();
        let mut stack = vec![meta.clone()];
        while let Some(path) = stack.pop() {
            for entry in fs::read_dir(&path).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                } else {
                    files.push((entry.path(), entry.metadata().unwrap().modified().unwrap()));
                }
            }
        }
        files.sort();
        files
    };
    let before = stamp();
    let retained = journal.operations(None).unwrap().len();
    assert!(retained > 0);
    journal.prune(&held).unwrap();
    assert_eq!(stamp(), before, "a no-op prune rewrote meta files");
    assert_eq!(journal.operations(None).unwrap().len(), retained);
    assert!(std::sync::Arc::ptr_eq(
        &journal.state().unwrap(),
        &journal.state().unwrap()
    ));
}
