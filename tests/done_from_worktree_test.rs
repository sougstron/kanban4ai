//! `kanban done` run by the agent's own shell from inside its isolated
//! worktree (TASK-346): the landing must not delete the caller's cwd, and a
//! retried `done` is a no-op success. Its own test binary because it changes
//! the process cwd.

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
fn done_from_inside_the_worktree_defers_removal_until_agent_exit() {
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

    let task = ops.create_task(NewTask::titled("Inside")).unwrap();
    ops.take_task(&task.id, "ses-in", true).unwrap().unwrap();
    ContextManager::new(ops.data_root())
        .append_context(&task.id, "done", "agent", &ops.storage)
        .unwrap();
    let wt = project.data_root.join(".kanban/worktrees").join(&task.id);
    fs::write(wt.join("feature.txt"), "feature\n").unwrap();

    std::env::set_current_dir(&wt).unwrap();
    let reviewed = ops
        .complete_task(&task.id, "ses-in", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.status, TaskStatus::Review);
    assert_eq!(reviewed.integration, IntegrationState::Landed);
    assert_eq!(
        fs::read_to_string(work.path().join("feature.txt")).unwrap(),
        "feature\n"
    );
    assert!(wt.is_dir(), "the caller's cwd survives the landing");
    assert!(std::env::current_dir().is_ok());

    // The agent retries after a spurious non-zero exit: still a success.
    let again = ops
        .complete_task(&task.id, "ses-in", true)
        .unwrap()
        .unwrap();
    assert_eq!(again.status, TaskStatus::Review);

    // The launch wrapper's agent-exit (also run from the worktree) finishes
    // the deferred cleanup.
    ops.reconcile_agent_exit(&task.id, "ses-in", 0).unwrap();
    std::env::set_current_dir(store.path()).unwrap();
    assert!(!wt.exists(), "worktree removed once the agent exited");
    let landed = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert!(landed.worktree.is_none() && landed.branch.is_none());
}
