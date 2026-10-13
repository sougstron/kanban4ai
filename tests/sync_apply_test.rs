use kanban4ai::core::storage::Storage;
use kanban4ai::core::sync::{Hlc, Journal, Operation};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;

fn op(node: &str, seq: u64, wall: u64, entity: &str, id: &str, set: Value) -> Operation {
    Operation {
        op: format!("{node}:{seq}"),
        hlc: Hlc(wall, 0, node.into()),
        store: String::new(),
        project: "project-uid".into(),
        entity: entity.into(),
        id: id.into(),
        set: serde_json::from_value(set).unwrap(),
        prev: BTreeMap::new(),
        actor: "agent".into(),
    }
}
fn permutations(items: &mut [Operation], at: usize, visit: &mut impl FnMut(&[Operation])) {
    if at == items.len() {
        visit(items);
        return;
    }
    for i in at..items.len() {
        items.swap(at, i);
        permutations(items, at + 1, visit);
        items.swap(at, i);
    }
}
fn apply(order: &[Operation]) -> (tempfile::TempDir, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    for original in order {
        let mut incoming = original.clone();
        incoming.store = journal.node.store_id.clone();
        journal.apply_remote(&[incoming.clone()], 0).unwrap();
        // Redelivery, including an old operation after newer sequences, is harmless.
        journal.apply_remote(&[incoming], 0).unwrap();
    }
    (dir, journal)
}
fn project() -> Operation {
    op(
        "a",
        1,
        1,
        "project",
        "demo",
        json!({"id":"demo", "name":"Demo", "uid":"project-uid", "created_at":"2026-01-01T00:00:00"}),
    )
}
fn task() -> Operation {
    op(
        "a",
        2,
        2,
        "task",
        "TASK-001",
        json!({"id":"TASK-001", "title":"base", "status":"todo", "description":"body"}),
    )
}
fn replicated_thread(path: &std::path::Path) -> Value {
    let mut value: Value = serde_yaml_ng::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("rev");
    value
}

#[test]
fn every_delivery_permutation_converges_with_causal_and_three_way_edits() {
    let mut a = op("a", 3, 3, "task", "TASK-001", json!({"title":"laptop"}));
    a.prev.insert("title".into(), task().hlc);
    let mut b = op("b", 1, 3, "task", "TASK-001", json!({"title":"server"}));
    b.prev = a.prev.clone();
    let mut c = op(
        "c",
        1,
        4,
        "task",
        "TASK-001",
        json!({"title":"third", "status":"review"}),
    );
    c.prev = a.prev.clone();
    c.prev.insert("status".into(), task().hlc);
    let mut causal = op(
        "b",
        2,
        5,
        "task",
        "TASK-001",
        json!({"title":"server followup"}),
    );
    causal.prev.insert("title".into(), b.hlc.clone());
    let mut ops = vec![project(), task(), a, b, c, causal];
    let mut expected = None;
    permutations(&mut ops, 0, &mut |order| {
        let (dir, journal) = apply(order);
        let root = dir.path().join("projects/demo");
        let result = Storage::new(&root).load_task("TASK-001").unwrap().unwrap();
        assert_eq!(result.title, "server followup");
        assert_eq!(result.status.as_str(), "review");
        assert!(!root.join(".kanban/tasks/todo/TASK-001.md").exists());
        let conflicts = fs::read_to_string(dir.path().join("sync/meta/conflicts.jsonl")).unwrap();
        let notes = replicated_thread(&root.join(".kanban/threads/TASK-001.yaml"));
        let projection = (
            serde_json::to_value(journal.state().unwrap().fields).unwrap(),
            conflicts,
            notes,
        );
        if let Some(expected) = &expected {
            assert_eq!(&projection, expected);
        } else {
            expected = Some(projection);
        }
        assert_eq!(
            journal.state().unwrap().vector,
            BTreeMap::from([("a".into(), 3), ("b".into(), 2), ("c".into(), 1)])
        );
    });
}

#[test]
fn uid_union_collision_parent_links_and_message_tombstones_converge() {
    let parent = op(
        "a",
        2,
        2,
        "message",
        "a:2",
        json!({"task_id":"TASK-001", "id":"MSG-001", "body":"parent"}),
    );
    let other = op(
        "b",
        1,
        2,
        "message",
        "b:1",
        json!({"task_id":"TASK-001", "id":"MSG-001", "body":"other"}),
    );
    let reply = op(
        "b",
        2,
        3,
        "message",
        "b:2",
        json!({"task_id":"TASK-001", "id":"MSG-002", "body":"reply", "parent_id":"b:1"}),
    );
    let mut ops = vec![project(), parent, other, reply];
    let mut expected = None;
    permutations(&mut ops, 0, &mut |order| {
        let (dir, journal) = apply(order);
        let path = dir
            .path()
            .join("projects/demo/.kanban/threads/TASK-001.yaml");
        let thread: kanban4ai::core::models::Thread =
            serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            thread
                .messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["MSG-001", "MSG-001@b", "MSG-002"]
        );
        assert_eq!(thread.messages[2].parent_id.as_deref(), Some("MSG-001@b"));
        let text = replicated_thread(&path);
        assert!(
            !dir.path()
                .join("projects/project-uid/.kanban/threads/TASK-001.yaml")
                .exists()
        );
        if let Some(expected) = &expected {
            assert_eq!(&text, expected);
        } else {
            expected = Some(text);
        }
        let mut deletion = op(
            "b",
            3,
            4,
            "tombstone",
            "b:1",
            json!({"entity":"message", "deleted":true}),
        );
        deletion.store = journal.node.store_id.clone();
        journal.apply_remote(&[deletion], 0).unwrap();
        let thread: kanban4ai::core::models::Thread =
            serde_yaml_ng::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert!(!thread.messages.iter().any(|m| m.body == "other"));
    });
}

#[test]
fn deletion_wins_newer_edit_until_explicit_restore_in_every_order() {
    let deletion = op(
        "b",
        1,
        3,
        "tombstone",
        "TASK-001",
        json!({"entity":"task", "deleted":true}),
    );
    let edit = op(
        "c",
        1,
        4,
        "task",
        "TASK-001",
        json!({"title":"saved losing edit"}),
    );
    let mut ops = vec![project(), task(), deletion, edit];
    let mut expected = None;
    permutations(&mut ops, 0, &mut |order| {
        let (dir, journal) = apply(order);
        let storage = Storage::new(dir.path().join("projects/demo"));
        assert!(storage.load_task("TASK-001").unwrap().is_none());
        let text = fs::read_to_string(dir.path().join("sync/meta/conflicts.jsonl")).unwrap();
        assert!(text.contains("saved losing edit"));
        if let Some(expected) = &expected {
            assert_eq!(&text, expected);
        } else {
            expected = Some(text);
        }
        let mut restore = op("b", 2, 5, "restore", "TASK-001", json!({"entity":"task"}));
        restore.store = journal.node.store_id.clone();
        journal.apply_remote(&[restore], 0).unwrap();
        assert_eq!(
            storage.load_task("TASK-001").unwrap().unwrap().title,
            "saved losing edit"
        );
    });
}

#[test]
fn project_slug_collision_is_uid_idempotent_and_order_independent() {
    let mut second = op(
        "b",
        1,
        2,
        "project",
        "demo",
        json!({"id":"demo", "name":"Second", "uid":"second-uid", "created_at":"2026-01-01T00:00:00"}),
    );
    second.project = "second-uid".into();
    let mut third = op(
        "c",
        1,
        3,
        "project",
        "demo-2",
        json!({"id":"demo-2","name":"Third","uid":"third-uid","created_at":"2026-01-01T00:00:00"}),
    );
    third.project = "third-uid".into();
    let mut ops = vec![project(), second, third];
    permutations(&mut ops, 0, &mut |order| {
        let (dir, journal) = apply(order);
        assert_eq!(
            journal.state().unwrap().project_uids,
            BTreeMap::from([
                ("demo".into(), "project-uid".into()),
                ("demo-3".into(), "second-uid".into()),
                ("demo-2".into(), "third-uid".into())
            ])
        );
        for (slug, name) in [("demo", "Demo"), ("demo-3", "Second"), ("demo-2", "Third")] {
            let value: Value = serde_yaml_ng::from_str(
                &fs::read_to_string(dir.path().join(format!("projects/{slug}/project.yaml")))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(value["name"], name);
            assert_eq!(value["id"], slug);
            let loaded = kanban4ai::core::project::ProjectStore::at(dir.path())
                .get(slug)
                .unwrap()
                .unwrap();
            assert_eq!(loaded.name, name);
            assert!(!loaded.work_path.is_dir());
        }
    });
}
#[test]
fn skew_store_mismatch_and_local_fields_reject_without_applying() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let mut invalid = project();
    invalid.store = journal.node.store_id.clone();
    invalid.hlc.0 = u64::MAX;
    assert!(
        journal
            .apply_remote(&[invalid.clone()], 0)
            .unwrap_err()
            .to_string()
            .contains("SYNC_CLOCK_SKEW")
    );
    invalid.hlc.0 = 1;
    invalid.store = "other".into();
    assert!(
        journal
            .apply_remote(&[invalid], 0)
            .unwrap_err()
            .to_string()
            .contains("SYNC_STORE_MISMATCH")
    );
    let mut invalid = task();
    invalid.store = journal.node.store_id.clone();
    invalid
        .set
        .insert("session".into(), json!("remote-process"));
    assert!(journal.apply_remote(&[invalid], 0).is_err());
    assert!(journal.state().unwrap().fields.is_empty());
}

#[test]
fn independent_fields_drafts_and_dotted_settings_preserve_local_state() {
    let (dir, journal) = apply(&[project(), task()]);
    let path = dir
        .path()
        .join("projects/demo/.kanban/tasks/todo/TASK-001.md");
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("title: base", "title: base\nsession: local-session");
    fs::write(&path, text).unwrap();
    let mut title = op("b", 1, 3, "task", "TASK-001", json!({"title":"renamed"}));
    title.prev.insert("title".into(), task().hlc);
    let mut status = op("c", 1, 3, "task", "TASK-001", json!({"status":"review"}));
    status.prev.insert("status".into(), task().hlc);
    let mut settings = op(
        "b",
        2,
        4,
        "config",
        "config",
        json!({"agents.codex.model":"new-model"}),
    );
    settings.project.clear();
    let message = op(
        "b",
        3,
        5,
        "message",
        "b:3",
        json!({"id":"MSG-001","task_id":"TASK-001","body":"question","draft":"one"}),
    );
    let draft = op(
        "c",
        2,
        6,
        "message",
        "b:3",
        json!({"draft":"two","task_id":"TASK-001"}),
    );
    let mut ops = vec![title, status, settings, message, draft];
    for op in &mut ops {
        op.store = journal.node.store_id.clone();
    }
    assert!(journal.apply_remote(&ops, 0).unwrap().is_empty());
    let task = Storage::new(dir.path().join("projects/demo"))
        .load_task("TASK-001")
        .unwrap()
        .unwrap();
    assert_eq!(task.title, "renamed");
    assert_eq!(task.status.as_str(), "review");
    assert_eq!(task.session.as_deref(), Some("local-session"));
    let config: Value =
        serde_yaml_ng::from_str(&fs::read_to_string(dir.path().join("config.yaml")).unwrap())
            .unwrap();
    assert_eq!(config["agents"]["codex"]["model"], "new-model");
    assert_eq!(config["node"]["node_id"], journal.node.node_id);
    let thread = replicated_thread(
        &dir.path()
            .join("projects/demo/.kanban/threads/TASK-001.yaml"),
    );
    assert_eq!(thread["messages"][0]["draft"], "two");
}

#[test]
fn concurrent_edits_of_different_fields_do_not_conflict_on_updated_at() {
    let (dir, journal) = apply(&[project(), task()]);
    let mut title = op(
        "b",
        1,
        3,
        "task",
        "TASK-001",
        json!({"title":"renamed", "updated_at":"2026-01-01T00:00:03"}),
    );
    title.prev.insert("title".into(), task().hlc);
    let mut description = op(
        "c",
        1,
        4,
        "task",
        "TASK-001",
        json!({"description":"edited", "updated_at":"2026-01-01T00:00:04"}),
    );
    description.prev.insert("description".into(), task().hlc);
    let mut ops = vec![title, description];
    for op in &mut ops {
        op.store = journal.node.store_id.clone();
    }
    assert!(journal.apply_remote(&ops, 0).unwrap().is_empty());
    let conflicts =
        fs::read_to_string(dir.path().join("sync/meta/conflicts.jsonl")).unwrap_or_default();
    assert!(conflicts.is_empty(), "{conflicts}");
    let task = Storage::new(dir.path().join("projects/demo"))
        .load_task("TASK-001")
        .unwrap()
        .unwrap();
    assert_eq!(task.title, "renamed");
    assert_eq!(task.description, "edited");
    assert_eq!(
        task.updated_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
        "2026-01-01T00:00:04"
    );
}

#[test]
fn remote_answer_survives_a_stale_thread_writer() {
    use kanban4ai::core::models::{Message, MessageKind, MessageRole};
    use kanban4ai::core::thread::ThreadManager;
    let message = op(
        "b",
        1,
        2,
        "message",
        "b:1",
        json!({"id":"MSG-001","task_id":"TASK-001","body":"question"}),
    );
    let (dir, journal) = apply(&[project(), message.clone()]);
    let manager = ThreadManager::new(dir.path().join("projects/demo")).unwrap();
    let mut stale = manager.load("TASK-001").unwrap();
    let mut answer = op(
        "b",
        2,
        3,
        "message",
        "b:1",
        json!({"answer":"remote answer","status":"answered","task_id":"TASK-001"}),
    );
    answer.store = journal.node.store_id.clone();
    journal.apply_remote(&[answer], 0).unwrap();
    stale.messages.push(Message::new(
        "MSG-002",
        MessageRole::Human,
        MessageKind::Context,
        "local context",
    ));
    manager.save("TASK-001", &mut stale).unwrap();
    let merged = manager.load("TASK-001").unwrap();
    assert_eq!(merged.messages[0].answer.as_deref(), Some("remote answer"));
    assert_eq!(merged.messages[1].body, "local context");
}

#[test]
fn interrupted_remote_redo_is_finished_before_store_reads() {
    let (dir, journal) = apply(&[project(), task()]);
    let path = dir
        .path()
        .join("projects/demo/.kanban/tasks/todo/TASK-001.md");
    let text = fs::read_to_string(&path).unwrap();
    fs::remove_file(&path).unwrap();
    let writes = BTreeMap::from([(path.clone(), Some(text))]);
    let pending = json!({"boards":[dir.path().join("projects/demo")], "writes":writes});
    fs::write(
        dir.path().join("sync/remote-pending.json"),
        pending.to_string(),
    )
    .unwrap();
    journal.state().unwrap();
    assert_eq!(
        Storage::new(dir.path().join("projects/demo"))
            .load_task("TASK-001")
            .unwrap()
            .unwrap()
            .title,
        "base"
    );
    assert!(!dir.path().join("sync/remote-pending.json").exists());
    let reopened = Journal::open(dir.path()).unwrap();
    let mut replay = task();
    replay.store = reopened.node.store_id.clone();
    reopened.apply_remote(&[replay], 0).unwrap();
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        writes.values().next().unwrap().as_ref().unwrap().as_str()
    );
}

#[test]
fn choosing_losing_version_is_a_replicated_edit_and_rejects_stale_choice() {
    let mut laptop = op("a", 3, 3, "task", "TASK-001", json!({"title":"laptop"}));
    laptop.prev.insert("title".into(), task().hlc);
    let mut server = op("b", 1, 4, "task", "TASK-001", json!({"title":"server"}));
    server.prev = laptop.prev.clone();
    let originals = [project(), task(), laptop, server];
    let (dir, journal) = apply(&originals);
    let conflict = journal.conflicts().unwrap().remove(0);
    journal.take_other_version(&conflict).unwrap();
    let storage = Storage::new(dir.path().join("projects/demo"));
    assert_eq!(
        storage.load_task("TASK-001").unwrap().unwrap().title,
        "laptop"
    );
    assert!(journal.conflicts().unwrap().is_empty());
    assert!(journal.take_other_version(&conflict).is_err());
    let state = journal.state().unwrap();
    let edit = state
        .operations
        .values()
        .find(|op| op.actor == "human")
        .unwrap();
    assert!(edit.hlc > conflict.winner);
    assert_eq!(edit.prev["title"], conflict.winner);
    assert_eq!(edit.hlc.2, journal.node.node_id);
    let (replica_dir, replica) = apply(&originals);
    let mut edit = edit.clone();
    edit.store = replica.node.store_id.clone();
    replica.apply_remote(&[edit.clone()], u64::MAX).unwrap();
    replica.apply_remote(&[edit], u64::MAX).unwrap();
    assert_eq!(
        Storage::new(replica_dir.path().join("projects/demo"))
            .load_task("TASK-001")
            .unwrap()
            .unwrap()
            .title,
        "laptop"
    );
    assert!(replica.conflicts().unwrap().is_empty());
}
