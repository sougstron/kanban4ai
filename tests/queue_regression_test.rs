//! Paired-store queue regressions: local launches must not wait for publication.
mod common;

use std::fs;
use std::sync::mpsc;
use std::time::Duration;

use kanban4ai::core::code;
use kanban4ai::core::models::{RunPhase, TaskStatus};
use kanban4ai::core::operations::Operations;
use kanban4ai::core::project::{Project, ProjectStore};
use kanban4ai::core::storage::{NewTask, Storage};
use kanban4ai::core::sync::Journal;

fn paired_board() -> (
    tempfile::TempDir,
    Project,
    Operations,
    common::RecordingLauncher,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let work = temp.path().join("work");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&work).unwrap();
    fs::write(
        root.join("config.yaml"),
        "node:\n  name: desk\n  role: laptop\n",
    )
    .unwrap();
    let project = ProjectStore::at(&root).add(&work, None).unwrap().project;
    Storage::new(&project.data_root).init_board().unwrap();
    Journal::open(&root).unwrap();
    fs::write(root.join("config.yaml"), "node:\n  name: desk\n  role: laptop\nsync:\n  enabled: true\n  server:\n    node_id: server-node\n    name: home\n    urls: [http://127.0.0.1:1]\n").unwrap();
    fs::write(project.kanban_dir().join("config.yaml"), "runner:\n  default: here\nnotifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  queue_enabled: true\n  max_running_total: 1\n  isolation:\n    enabled: false\n").unwrap();
    let recorder = common::RecordingLauncher::new();
    let ops = Operations::for_project_with_launcher(&project, Box::new(recorder.clone()));
    (temp, project, ops, recorder)
}

#[test]
fn paired_local_queue_obeys_caps_and_does_not_prepare_a_clone() {
    let (_temp, _project, ops, recorder) = paired_board();
    for title in ["First", "Second"] {
        let task = ops.create_task(NewTask::titled(title)).unwrap();
        ops.queue_run(&task.id).unwrap();
    }
    let dispatched = ops.dispatch_queue().unwrap();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(recorder.calls().len(), 1);
    assert_eq!(
        ops.storage
            .get_all_tasks()
            .unwrap()
            .iter()
            .filter(|t| t.run_phase == Some(RunPhase::Queued))
            .count(),
        1
    );
}

#[test]
fn existing_non_git_project_launch_does_not_wait_for_store_lock() {
    let (temp, project, _ops, _recorder) = paired_board();
    let root = temp.path().join("store");
    let guard = ProjectStore::at(&root).lock().unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(code::for_launch(&root, &project.id).map(|p| p.work_path))
            .unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(5));
    drop(guard);
    worker.join().unwrap();
    assert_eq!(result.unwrap().unwrap(), temp.path().join("work"));
}

#[test]
fn completion_starts_chain_before_waiting_for_publication_lock() {
    let (temp, project, ops, recorder) = paired_board();
    let parent = ops.create_task(NewTask::titled("Parent")).unwrap();
    let mut child = ops.create_task(NewTask::titled("Child")).unwrap();
    child.chained_to = Some(parent.id.clone());
    ops.storage.save_task(&child).unwrap();
    ops.take_task(&parent.id, "ses-parent", true).unwrap();
    kanban4ai::core::context::ContextManager::new(&ops.storage.project_path)
        .append_context(&parent.id, "Implemented and checked", "agent", &ops.storage)
        .unwrap();
    let guard = ProjectStore::at(temp.path().join("store")).lock().unwrap();
    let worker_recorder = recorder.clone();
    let worker =
        std::thread::spawn(move || {
            Operations::for_project_with_launcher(&project, Box::new(worker_recorder))
                .complete_task(&parent.id, "ses-parent", true)
        });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while recorder.calls().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let launched_before_unlock = recorder.calls().len() == 2;
    drop(guard);
    let completed = worker.join().unwrap().unwrap().unwrap();
    assert_eq!(completed.status, TaskStatus::Review);
    assert!(
        launched_before_unlock,
        "chain waited for code publication's store lock"
    );
}
