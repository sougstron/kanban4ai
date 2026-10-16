use kanban4ai::core::storage::{Storage, atomic_write_text};
use kanban4ai::core::sync::{Journal, install_snapshot};
use std::{fs, path::Path, process::Command, time::Instant};

fn task(title: &str) -> String {
    format!("---\nid: TASK-001\ntitle: {title}\nstatus: todo\n---\nbody\n")
}
fn board(store: &Path) -> std::path::PathBuf {
    kanban4ai::core::sync::enable_sync(store).unwrap();
    let board = store.join("projects/demo");
    Storage::new(&board).init_board().unwrap();
    atomic_write_text(
        &board.join("project.yaml"),
        "id: demo\nname: Demo\ncreated_at: '2026-01-01T00:00:00'\n",
    )
    .unwrap();
    board
}
fn replicate(from: &Journal, to: &Journal) {
    let page = from
        .operations_after(&to.vector().unwrap(), usize::MAX)
        .unwrap();
    for hash in page.operations.iter().filter_map(|op| op.blob_hash()) {
        to.put_blob(hash, &from.blob(hash).unwrap()).unwrap();
    }
    to.apply_remote(&page.operations, u64::MAX).unwrap();
}
fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn external_edits_checkout_create_delete_and_repeat_replicate() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let board = board(a.path());
    let path = board.join(".kanban/tasks/todo/TASK-001.md");
    atomic_write_text(&path, &task("original")).unwrap();
    let source = Journal::open(a.path()).unwrap();
    source.reconcile().unwrap();
    Journal::open(b.path()).unwrap();
    install_snapshot(b.path(), &source.snapshot().unwrap()).unwrap();
    let peer = Journal::open(b.path()).unwrap();
    let remote = b
        .path()
        .join("projects/demo/.kanban/tasks/todo/TASK-001.md");
    git(&board, &["init", "--quiet"]);
    git(&board, &["add", ".kanban/tasks"]);
    git(
        &board,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ],
    );
    fs::write(&path, task("manual edit")).unwrap();
    assert_eq!(source.reconcile().unwrap(), 1);
    replicate(&source, &peer);
    assert!(fs::read_to_string(&remote).unwrap().contains("manual edit"));
    let vector = source.vector().unwrap();
    assert_eq!(source.reconcile().unwrap(), 0);
    assert_eq!(source.vector().unwrap(), vector);
    // Metadata-only changes do not become operations either.
    fs::write(&path, fs::read(&path).unwrap()).unwrap();
    assert_eq!(source.reconcile().unwrap(), 0);
    assert_eq!(source.vector().unwrap(), vector);
    git(&board, &["checkout", "--", ".kanban/tasks"]);
    source.reconcile().unwrap();
    replicate(&source, &peer);
    assert!(fs::read_to_string(&remote).unwrap().contains("original"));
    fs::remove_file(&path).unwrap();
    source.reconcile().unwrap();
    replicate(&source, &peer);
    assert!(!remote.exists());
    fs::write(&path, task("recreated externally")).unwrap();
    source.reconcile().unwrap();
    replicate(&source, &peer);
    let tasks = Storage::new(b.path().join("projects/demo"))
        .list_tasks(None)
        .unwrap();
    assert!(tasks.iter().any(|t| t.title == "recreated externally"));
}

#[test]
fn quiet_scan_skips_board_locks_and_still_sees_bypass_writes() {
    let a = tempfile::tempdir().unwrap();
    let board = board(a.path());
    let path = board.join(".kanban/tasks/todo/TASK-001.md");
    atomic_write_text(&path, &task("original")).unwrap();
    let journal = Journal::open(a.path()).unwrap();
    journal.reconcile().unwrap();
    // A writer elsewhere holds the board: an unchanged store must not wait on it.
    let (held, release) = (
        std::sync::mpsc::channel::<()>(),
        std::sync::mpsc::channel::<()>(),
    );
    let holder = {
        let board = board.clone();
        let (held_tx, release_rx) = (held.0, release.1);
        std::thread::spawn(move || {
            let _lock = Storage::new(&board).lock().unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
    };
    held.1.recv().unwrap();
    let started = Instant::now();
    assert_eq!(journal.reconcile().unwrap(), 0);
    assert!(started.elapsed().as_secs() < 5);
    release.0.send(()).unwrap();
    holder.join().unwrap();
    fs::write(&path, task("manual edit")).unwrap();
    assert_eq!(journal.reconcile().unwrap(), 1);
    assert_eq!(journal.reconcile().unwrap(), 0);
}

#[test]
fn blobs_and_instructions_are_incremental_but_artifacts_are_not() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let board = board(a.path());
    let source = Journal::open(a.path()).unwrap();
    Journal::open(b.path()).unwrap();
    install_snapshot(b.path(), &source.snapshot().unwrap()).unwrap();
    let peer = Journal::open(b.path()).unwrap();
    let image = ".kanban/assets/images/pasted-123-deadbeef.png";
    let instructions = ".kanban/instructions/executor.md";
    for (path, bytes) in [
        (image, &[0, 255, 42, 128][..]),
        (instructions, b"Use the project rules\n".as_slice()),
        (".kanban/assets/report.csv", b"ignored".as_slice()),
        (".kanban/context/notes.md", b"ignored".as_slice()),
    ] {
        fs::create_dir_all(board.join(path).parent().unwrap()).unwrap();
        fs::write(board.join(path), bytes).unwrap();
    }
    source.reconcile().unwrap();
    let ops = source
        .operations_after(&peer.vector().unwrap(), usize::MAX)
        .unwrap()
        .operations;
    assert_eq!(ops.iter().filter(|o| o.entity == "file").count(), 2);
    assert!(
        ops.iter()
            .filter_map(|o| o.blob_hash())
            .all(|hash| hash.len() == 64)
    );
    assert!(
        ops.iter()
            .all(|o| !o.id.contains("report.csv") && !o.id.contains("notes.md"))
    );
    replicate(&source, &peer);
    let remote = b.path().join("projects/demo");
    assert_eq!(fs::read(remote.join(image)).unwrap(), [0, 255, 42, 128]);
    assert_eq!(
        fs::read(remote.join(instructions)).unwrap(),
        b"Use the project rules\n"
    );
    fs::write(board.join(instructions), b"Updated rules").unwrap();
    fs::remove_file(board.join(image)).unwrap();
    source.reconcile().unwrap();
    replicate(&source, &peer);
    assert!(!remote.join(image).exists());
    assert_eq!(
        fs::read(remote.join(instructions)).unwrap(),
        b"Updated rules"
    );
    fs::write(board.join(image), [0, 255, 42, 128]).unwrap();
    source.reconcile().unwrap();
    replicate(&source, &peer);
    assert_eq!(fs::read(remote.join(image)).unwrap(), [0, 255, 42, 128]);
    assert_eq!(peer.reconcile().unwrap(), 0);
    assert!(peer.put_blob(&"0".repeat(64), b"wrong digest").is_err());
    assert!(peer.blob("../../config.yaml").is_err());
}

#[test]
fn remote_apply_captures_pending_local_drift_and_moves_do_not_delete_tasks() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let board = board(a.path());
    let path = board.join(".kanban/tasks/todo/TASK-001.md");
    atomic_write_text(&path, &task("original")).unwrap();
    let source = Journal::open(a.path()).unwrap();
    Journal::open(b.path()).unwrap();
    install_snapshot(b.path(), &source.snapshot().unwrap()).unwrap();
    let peer = Journal::open(b.path()).unwrap();
    let remote = b
        .path()
        .join("projects/demo/.kanban/tasks/todo/TASK-001.md");
    fs::write(&remote, task("offline edit")).unwrap();
    let moved = board.join(".kanban/tasks/review/TASK-001.md");
    fs::write(
        &moved,
        task("original").replace("status: todo", "status: review"),
    )
    .unwrap();
    fs::remove_file(&path).unwrap();
    source.reconcile().unwrap();
    replicate(&source, &peer);
    let tasks = Storage::new(b.path().join("projects/demo"))
        .list_tasks(None)
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "offline edit");
    assert_eq!(tasks[0].status.to_string(), "review");
    assert_eq!(source.reconcile().unwrap(), 0);
    assert_eq!(peer.reconcile().unwrap(), 0);
}

#[test]
#[ignore = "2000-file drift benchmark; run explicitly with --ignored --nocapture"]
fn benchmark_reconcile_2000_files() {
    let dir = tempfile::tempdir().unwrap();
    let board = board(dir.path());
    let journal = Journal::open(dir.path()).unwrap();
    let instructions = board.join(".kanban/instructions");
    fs::create_dir_all(&instructions).unwrap();
    for n in 0..2000 {
        fs::write(
            instructions.join(format!("rule-{n:04}.md")),
            format!("Rule {n}\n"),
        )
        .unwrap();
    }
    journal.reconcile().unwrap();
    let vector = journal.vector().unwrap();
    let start = Instant::now();
    for _ in 0..20 {
        assert_eq!(journal.reconcile().unwrap(), 0);
    }
    println!(
        "2000 files: unchanged reconciliation mean {:?} (20 rounds)",
        start.elapsed() / 20
    );
    assert_eq!(journal.vector().unwrap(), vector);
}

#[test]
fn external_thread_and_config_deletion_replicate() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let root = board(a.path());
    let task_path = root.join(".kanban/tasks/todo/TASK-001.md");
    atomic_write_text(&task_path, &task("thread owner")).unwrap();
    let thread = root.join(".kanban/threads/TASK-001.yaml");
    fs::write(&thread, "task_id: TASK-001\nmessages: []\n").unwrap();
    let source = Journal::open(a.path()).unwrap();
    Journal::open(b.path()).unwrap();
    install_snapshot(b.path(), &source.snapshot().unwrap()).unwrap();
    let peer = Journal::open(b.path()).unwrap();
    let remote = b.path().join("projects/demo/.kanban");
    fs::remove_file(&thread).unwrap();
    fs::remove_file(root.join(".kanban/config.yaml")).unwrap();
    source.reconcile().unwrap();
    replicate(&source, &peer);
    assert!(!remote.join("threads/TASK-001.yaml").exists());
    assert!(!remote.join("config.yaml").exists());
    fs::write(&thread, "task_id: TASK-001\nmessages: []\n").unwrap();
    fs::write(root.join(".kanban/config.yaml"), "interactive: false\n").unwrap();
    source.reconcile().unwrap();
    replicate(&source, &peer);
    assert!(remote.join("threads/TASK-001.yaml").is_file());
    assert!(
        fs::read_to_string(remote.join("config.yaml"))
            .unwrap()
            .contains("interactive: false")
    );
}
