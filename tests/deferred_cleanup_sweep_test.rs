//! The daemon-tick sweep for deferred post-landing cleanups (TASK-346): a
//! Landed worktree whose session never runs `agent-exit` is removed once no
//! process stands inside it. Its own test binary because it changes the
//! process cwd.

mod common;

use common::RecordingLauncher;
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::models::{IntegrationState, TaskStatus};
use kanban4ai::core::operations::Operations;
use kanban4ai::core::project::ProjectStore;
use kanban4ai::core::storage::{NewTask, Storage};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const CONFIG: &str = "columns:\n- name: To Do\n  id: todo\n- name: In Progress\n  id: in_progress\n- name: Review\n  id: review\n- name: Done\n  id: done\nnotifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  isolation:\n    mode: auto\n";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git spawn");
    assert!(out.status.success(), "git {args:?} failed");
}

#[test]
fn sweep_removes_a_deferred_worktree_once_nothing_stands_in_it() {
    let store = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    git(work.path(), &["init", "-q", "-b", "main"]);
    git(
        work.path(),
        &["config", "user.email", "kanban@example.test"],
    );
    git(work.path(), &["config", "user.name", "Kanban Test"]);
    fs::write(work.path().join("base.txt"), "base\n").unwrap();
    git(work.path(), &["add", "-A"]);
    git(work.path(), &["commit", "-q", "-m", "base"]);
    let project = ProjectStore::at(store.path())
        .add(work.path(), None)
        .unwrap()
        .project;
    Storage::new(&project.data_root).init_board().unwrap();
    fs::write(project.data_root.join(".kanban/config.yaml"), CONFIG).unwrap();
    let ops = Operations::for_project_with_launcher(&project, Box::new(RecordingLauncher::new()));

    let task = ops.create_task(NewTask::titled("Manual session")).unwrap();
    ops.take_task(&task.id, "ses-manual", true)
        .unwrap()
        .unwrap();
    ContextManager::new(ops.data_root())
        .append_context(&task.id, "done", "agent", &ops.storage)
        .unwrap();
    let wt = project.data_root.join(".kanban/worktrees").join(&task.id);
    fs::write(wt.join("feature.txt"), "feature\n").unwrap();

    std::env::set_current_dir(&wt).unwrap();
    let reviewed = ops
        .complete_task(&task.id, "ses-manual", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.integration, IntegrationState::Landed);
    assert!(
        wt.is_dir(),
        "removal deferred while the caller stands inside"
    );

    // The session is closed, but this process still stands in the worktree.
    let swept = ops.sweep_deferred_cleanups().unwrap();
    if Path::new("/proc/self/cwd").exists() {
        assert!(swept.is_empty());
        assert!(wt.is_dir(), "never removed from under a live cwd");
    }

    // No agent-exit ever comes; the caller just leaves.
    std::env::set_current_dir(store.path()).unwrap();
    let swept = ops.sweep_deferred_cleanups().unwrap();
    if !Path::new("/proc/self/cwd").exists() {
        assert!(
            swept.is_empty() && wt.is_dir(),
            "no /proc: stay conservative"
        );
        return;
    }
    assert_eq!(swept, vec![task.id.clone()]);
    assert!(!wt.exists(), "worktree swept once nothing stands in it");
    let landed = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert!(landed.worktree.is_none() && landed.branch.is_none());
    assert_eq!(landed.status, TaskStatus::Review);
    assert!(ops.sweep_deferred_cleanups().unwrap().is_empty());
}
