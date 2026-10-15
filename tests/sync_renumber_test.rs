use kanban4ai::core::models::{MessageKind, MessageRole, Task};
use kanban4ai::core::operations::Operations;
use kanban4ai::core::storage::{NewTask, Storage, atomic_write_text};
use kanban4ai::core::sync::{Journal, Operation};
use kanban4ai::core::thread::ThreadManager;
use std::fs;
use std::path::Path;

fn ops(journal: &Journal) -> Vec<Operation> {
    journal.operations(None).unwrap()
}

/// A second node of `a`'s store, holding an empty replica of its board.
fn node(a: &Journal) -> (tempfile::TempDir, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let mut identity = journal.node.clone();
    identity.store_id = a.node.store_id.clone();
    fs::write(
        dir.path().join("sync/node.yaml"),
        serde_yaml_ng::to_string(&identity).unwrap(),
    )
    .unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    Storage::new(dir.path().join("projects/demo"))
        .init_board()
        .unwrap();
    (dir, journal)
}

fn origin() -> (tempfile::TempDir, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    // Paired with a server that is never reachable.
    atomic_write_text(
        &dir.path().join("config.yaml"),
        &format!(
            "sync:\n  peers:\n    - node_id: 00000000-0000-4000-8000-000000000000\n      role: server\nnode:\n  node_id: {}\n  name: laptop\n  role: laptop\n",
            journal.node.node_id
        ),
    )
    .unwrap();
    let board = Storage::new(dir.path().join("projects/demo"));
    board.init_board().unwrap();
    atomic_write_text(
        &board.project_path.join("project.yaml"),
        "id: demo\nname: Demo\ncreated_at: '2026-01-01T00:00:00'\n",
    )
    .unwrap();
    (dir, journal)
}

fn exchange(nodes: &[&Journal]) {
    for _ in 0..2 {
        for from in nodes {
            for to in nodes {
                if from.node.node_id != to.node.node_id {
                    to.apply_remote(&ops(from), 300_000).unwrap();
                }
            }
        }
    }
}

/// id, title, parent_task, depends_on, context message bodies.
type TaskView = (String, String, Option<String>, Vec<String>, Vec<String>);

/// Replicated view of a board: everything except machine-local fields.
fn view(root: &Path) -> Vec<TaskView> {
    let board = Storage::new(root.join("projects/demo"));
    let threads = ThreadManager::new(&board.project_path).unwrap();
    let mut tasks: Vec<_> = board
        .list_tasks(None)
        .unwrap()
        .into_iter()
        .map(|task: Task| {
            let bodies = threads
                .load(&task.id)
                .unwrap()
                .messages
                .into_iter()
                .filter(|m| m.kind == MessageKind::Context)
                .map(|m| m.body)
                .collect();
            (
                task.id,
                task.title,
                task.parent_task,
                task.depends_on,
                bodies,
            )
        })
        .collect();
    tasks.sort();
    tasks
}

fn id_of(root: &Path, title: &str) -> String {
    Storage::new(root.join("projects/demo"))
        .list_tasks(None)
        .unwrap()
        .into_iter()
        .find(|task| task.title == title)
        .unwrap_or_else(|| panic!("{title} missing"))
        .id
}

#[test]
fn creation_without_server_or_peers_always_succeeds_and_never_reuses_numbers() {
    let (a, ja) = origin();
    let board = Storage::new(a.path().join("projects/demo"));
    let first = board.create_task(NewTask::titled("first")).unwrap();
    assert_eq!(first.id, "TASK-001");
    board.delete_task(&first.id).unwrap();
    // A legacy file absent from the journal still raises the floor.
    fs::write(board.tasks_dir.join("archive/TASK-040.md"), "legacy file").unwrap();
    for n in 41..=160 {
        let task = board.create_task(NewTask::titled(format!("t{n}"))).unwrap();
        assert_eq!(task.id, format!("TASK-{n:03}"));
    }
    fs::remove_file(board.tasks_dir.join("archive/TASK-040.md")).unwrap();
    let last = board.create_task(NewTask::titled("last")).unwrap();
    board.delete_task(&last.id).unwrap();
    let vector = ja.state().unwrap().vector;
    ja.prune(&std::collections::BTreeMap::from([("peer".into(), vector)]))
        .unwrap();
    let reopened = Storage::new(a.path().join("projects/demo"));
    assert_eq!(
        reopened.create_task(NewTask::titled("after")).unwrap().id,
        "TASK-162"
    );
    assert!(!a.path().join("sync/meta/allocation.json").exists());
}

#[test]
fn concurrent_offline_creation_of_one_number_converges_with_valid_references() {
    let (a, ja) = origin();
    let (b, jb) = node(&ja);
    let sa = Storage::new(a.path().join("projects/demo"));
    let sb = Storage::new(b.path().join("projects/demo"));
    sa.create_task(NewTask::titled("shared")).unwrap();
    exchange(&[&ja, &jb]);
    assert_eq!(id_of(b.path(), "shared"), "TASK-001");

    // Offline on both nodes: each allocates TASK-002 locally.
    let mine = sa.create_task(NewTask::titled("from a")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let theirs = sb.create_task(NewTask::titled("from b")).unwrap();
    assert_eq!(
        (mine.id.as_str(), theirs.id.as_str()),
        ("TASK-002", "TASK-002")
    );
    let child = sb
        .create_task(NewTask {
            parent_task: Some(theirs.id.clone()),
            depends_on: vec![theirs.id.clone(), "TASK-001".into()],
            chained_to: Some(theirs.id.clone()),
            ..NewTask::titled("child of b")
        })
        .unwrap();
    assert_eq!(child.id, "TASK-003");
    ThreadManager::new(&sb.project_path)
        .unwrap()
        .post(
            &theirs.id,
            MessageRole::Agent,
            MessageKind::Context,
            "note on b",
            None,
            vec![],
            None,
        )
        .unwrap();
    let session = sb.project_path.join(".kanban/sessions/ses-b.yaml");
    fs::create_dir_all(session.parent().unwrap()).unwrap();
    fs::write(&session, "id: ses-b\ntask_id: TASK-002\n").unwrap();

    // A third node receives everything at once, in another order.
    let (c, jc) = node(&ja);
    let mut all = ops(&jb);
    all.extend(ops(&ja));
    all.reverse();
    jc.apply_remote(&all, 300_000).unwrap();
    exchange(&[&ja, &jb]);
    exchange(&[&ja, &jb, &jc]);

    // The earlier creation keeps the number; the other moves to the next
    // number nobody claimed, identically everywhere.
    assert_eq!(id_of(a.path(), "from a"), "TASK-002");
    assert_eq!(id_of(a.path(), "from b"), "TASK-004");
    let expected = view(a.path());
    assert_eq!(view(b.path()), expected);
    assert_eq!(view(c.path()), expected);
    let child = expected.iter().find(|t| t.1 == "child of b").unwrap();
    assert_eq!(child.0, "TASK-003");
    assert_eq!(child.2.as_deref(), Some("TASK-004"));
    assert_eq!(child.3, ["TASK-004", "TASK-001"]);
    let moved = expected.iter().find(|t| t.1 == "from b").unwrap();
    assert_eq!(moved.4, ["note on b"]);
    for root in [a.path(), b.path(), c.path()] {
        let board = Storage::new(root.join("projects/demo"));
        let chained = board.load_task("TASK-003").unwrap().unwrap().chained_to;
        assert_eq!(chained.as_deref(), Some("TASK-004"));
        assert!(
            !board
                .project_path
                .join(".kanban/threads/TASK-002.yaml")
                .exists()
                || ThreadManager::new(&board.project_path)
                    .unwrap()
                    .load("TASK-002")
                    .unwrap()
                    .messages
                    .iter()
                    .all(|m| m.body != "note on b")
        );
    }
    // The losing node's local session follows its task.
    assert!(
        fs::read_to_string(&session)
            .unwrap()
            .contains("task_id: TASK-004")
    );

    // Edits on the renumbered task keep flowing to the same task everywhere.
    let mut moved = sb.load_task("TASK-004").unwrap().unwrap();
    moved.title = "from b, edited".into();
    sb.save_task(&moved).unwrap();
    exchange(&[&ja, &jb, &jc]);
    assert_eq!(id_of(a.path(), "from b, edited"), "TASK-004");
    assert_eq!(id_of(c.path(), "from a"), "TASK-002");

    // New local numbers stay above every claimed one.
    assert_eq!(
        sa.create_task(NewTask::titled("next")).unwrap().id,
        "TASK-005"
    );

    // The retired number resolves once it no longer names a live task.
    let ops_b = Operations::new(&sb.project_path);
    assert_eq!(ops_b.resolve_task_alias("TASK-002").unwrap(), "TASK-002");
    exchange(&[&ja, &jb]);
    sb.delete_task("TASK-002").unwrap();
    assert_eq!(ops_b.resolve_task_alias("TASK-002").unwrap(), "TASK-004");
    assert_eq!(
        ops_b.get_task("TASK-002").unwrap().unwrap().title,
        "from b, edited"
    );
}
