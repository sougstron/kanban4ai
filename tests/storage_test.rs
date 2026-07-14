//! Storage compatibility and concurrency behavior from the earlier implementation.

mod common;

use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

use common::temp_board;
use kanban4ai::core::models::TaskStatus;
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::storage::NewTask;
use kanban4ai::core::thread::ThreadManager;

#[test]
fn init_board_creates_layout_and_config() {
    let (dir, _storage) = temp_board();
    let kanban = dir.path().join(".kanban");
    for sub in [
        "tasks/todo",
        "tasks/in_progress",
        "tasks/review",
        "tasks/done",
        "tasks/archive",
        "context",
        "sessions",
        "threads",
        "logs",
        "backups",
        "assets/images",
    ] {
        assert!(kanban.join(sub).is_dir(), "missing {sub}");
    }
    assert!(kanban.join("config.yaml").is_file());
}

#[test]
fn create_task_assigns_sequential_ids_in_todo() {
    let (dir, storage) = temp_board();
    let first = storage.create_task(NewTask::titled("First")).unwrap();
    let second = storage.create_task(NewTask::titled("Second")).unwrap();
    assert_eq!(first.id, "TASK-001");
    assert_eq!(second.id, "TASK-002");
    assert_eq!(first.status, TaskStatus::Todo);
    assert!(dir.path().join(".kanban/tasks/todo/TASK-001.md").is_file());
}

#[test]
fn concurrent_creates_assign_unique_ids() {
    let (dir, _storage) = temp_board();
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let handles = (0..workers)
        .map(|index| {
            let path = dir.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                kanban4ai::core::storage::Storage::new(path)
                    .create_task(NewTask::titled(format!("Concurrent {index}")))
                    .unwrap()
                    .id
            })
        })
        .collect::<Vec<_>>();
    let mut ids = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), workers);
}

#[test]
fn create_task_initializes_thread_with_system_and_task_messages() {
    let (dir, storage) = temp_board();
    let task = storage
        .create_task(NewTask {
            title: "With body".into(),
            description: "Do the thing".into(),
            ..Default::default()
        })
        .unwrap();
    let empty = storage.create_task(NewTask::titled("No body")).unwrap();

    let manager = ThreadManager::new(dir.path()).unwrap();
    let thread = manager.load(&task.id).unwrap();
    assert_eq!(thread.messages.len(), 2);
    assert_eq!(thread.messages[0].id, "MSG-001");
    assert!(
        thread.messages[0]
            .body
            .starts_with("Task created: With body")
    );
    assert_eq!(thread.messages[1].id, "MSG-002");
    assert_eq!(thread.messages[1].body, "Do the thing");

    let empty_thread = manager.load(&empty.id).unwrap();
    assert_eq!(empty_thread.messages[1].body, "(no description provided)");
}

#[test]
fn tui_fingerprint_tracks_thread_and_session_sidecars() {
    let (dir, storage) = temp_board();
    let task = storage.create_task(NewTask::titled("Live detail")).unwrap();
    let initial = storage.tui_fingerprint();

    ThreadManager::new(dir.path())
        .unwrap()
        .post(
            &task.id,
            kanban4ai::core::models::MessageRole::Human,
            kanban4ai::core::models::MessageKind::Context,
            "thread changed",
            None,
            vec![],
            None,
        )
        .unwrap();
    let after_thread = storage.tui_fingerprint();
    assert_ne!(after_thread, initial);

    SessionManager::new(dir.path())
        .link_session(&task.id, "ses-fingerprint")
        .unwrap();
    assert_ne!(storage.tui_fingerprint(), after_thread);
}

#[test]
fn next_id_scans_all_status_dirs() {
    let (_dir, storage) = temp_board();
    storage.create_task(NewTask::titled("One")).unwrap();
    storage.create_task(NewTask::titled("Two")).unwrap();
    storage.move_task("TASK-002", "done").unwrap();

    assert_eq!(storage.get_next_id().unwrap(), "TASK-003");
}

#[test]
fn move_task_relocates_the_file() {
    let (dir, storage) = temp_board();
    storage.create_task(NewTask::titled("Mover")).unwrap();

    let moved = storage.move_task("TASK-001", "review").unwrap().unwrap();
    assert_eq!(moved.status, TaskStatus::Review);
    assert!(
        dir.path()
            .join(".kanban/tasks/review/TASK-001.md")
            .is_file()
    );
    assert!(!dir.path().join(".kanban/tasks/todo/TASK-001.md").exists());
}

#[test]
fn move_task_to_invalid_status_errors() {
    let (_dir, storage) = temp_board();
    storage.create_task(NewTask::titled("Bad move")).unwrap();
    assert!(storage.move_task("TASK-001", "nonsense").is_err());
}

#[test]
fn save_task_sweeps_duplicate_copies() {
    let (dir, storage) = temp_board();
    let task = storage.create_task(NewTask::titled("Dup")).unwrap();

    // simulate a crash leaving a stale copy in another column
    let stale = dir.path().join(".kanban/tasks/done/TASK-001.md");
    fs::copy(dir.path().join(".kanban/tasks/todo/TASK-001.md"), &stale).unwrap();

    storage.save_task(&task).unwrap();
    assert!(dir.path().join(".kanban/tasks/todo/TASK-001.md").is_file());
    assert!(!stale.exists(), "stale duplicate must be swept");
}

#[test]
fn delete_and_exists() {
    let (_dir, storage) = temp_board();
    storage.create_task(NewTask::titled("Doomed")).unwrap();
    assert!(storage.task_exists("TASK-001"));
    assert!(storage.delete_task("TASK-001").unwrap());
    assert!(!storage.task_exists("TASK-001"));
    assert!(!storage.delete_task("TASK-001").unwrap());
}

#[test]
fn list_tasks_filters_by_status() {
    let (_dir, storage) = temp_board();
    storage.create_task(NewTask::titled("A")).unwrap();
    storage.create_task(NewTask::titled("B")).unwrap();
    storage.move_task("TASK-002", "in_progress").unwrap();

    assert_eq!(storage.get_all_tasks().unwrap().len(), 2);
    let todo = storage.get_tasks_by_status("todo").unwrap();
    assert_eq!(todo.len(), 1);
    assert_eq!(todo[0].id, "TASK-001");
    assert!(storage.get_tasks_by_status("done").unwrap().is_empty());
}

#[test]
fn load_missing_task_is_none() {
    let (_dir, storage) = temp_board();
    assert!(storage.load_task("TASK-404").unwrap().is_none());
}

#[test]
fn fingerprint_changes_when_tasks_change() {
    let (_dir, storage) = temp_board();
    let empty = storage.tasks_fingerprint();
    storage.create_task(NewTask::titled("Fp")).unwrap();
    let one = storage.tasks_fingerprint();
    assert_ne!(empty, one);
    assert_eq!(one.0, 1);
}
