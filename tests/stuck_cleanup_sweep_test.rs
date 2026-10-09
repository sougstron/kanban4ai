//! A deferred post-landing cleanup that cannot finish must not run, and post
//! its failure, on every pump tick: one such task grew a thread to 25k
//! identical notes and kept every open TUI busy rewriting it.

mod common;

use common::RecordingLauncher;
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::models::{IntegrationState, MessageKind};
use kanban4ai::core::operations::Operations;
use kanban4ai::core::project::ProjectStore;
use kanban4ai::core::storage::{NewTask, Storage};
use kanban4ai::core::thread::ThreadManager;
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

/// A board with one task landed and already cleaned up, ready to be put back
/// into a stuck "Landed, still holding its worktree" shape.
fn landed_board(store: &Path, work: &Path) -> (Operations, String) {
    git(work, &["init", "-q", "-b", "main"]);
    git(work, &["config", "user.email", "kanban@example.test"]);
    git(work, &["config", "user.name", "Kanban Test"]);
    fs::write(work.join("base.txt"), "base\n").unwrap();
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", "base"]);
    let project = ProjectStore::at(store).add(work, None).unwrap().project;
    Storage::new(&project.data_root).init_board().unwrap();
    fs::write(project.data_root.join(".kanban/config.yaml"), CONFIG).unwrap();
    let ops = Operations::for_project_with_launcher(&project, Box::new(RecordingLauncher::new()));
    let task = ops.create_task(NewTask::titled("Landed")).unwrap();
    ops.take_task(&task.id, "ses-manual", true)
        .unwrap()
        .unwrap();
    ContextManager::new(ops.data_root())
        .append_context(&task.id, "done", "agent", &ops.storage)
        .unwrap();
    let wt = ops.data_root().join(".kanban/worktrees").join(&task.id);
    fs::write(wt.join("feature.txt"), "feature\n").unwrap();
    let landed = ops
        .complete_task(&task.id, "ses-manual", true)
        .unwrap()
        .unwrap();
    assert_eq!(landed.integration, IntegrationState::Landed);
    assert!(landed.worktree.is_none(), "cleaned up at landing");
    (ops, task.id)
}

fn hold_worktree(ops: &Operations, task_id: &str, branch: &str) {
    let mut task = ops.storage.load_task(task_id).unwrap().unwrap();
    task.worktree = Some(task_id.to_string());
    task.branch = Some(branch.to_string());
    ops.storage.save_task(&task).unwrap();
}

fn failure_notes(ops: &Operations, task_id: &str) -> usize {
    ThreadManager::new(ops.data_root())
        .unwrap()
        .messages_of_kind(task_id, MessageKind::AgentStep)
        .unwrap()
        .iter()
        .filter(|message| message.body.starts_with("⚠ landing cleanup failed"))
        .count()
}

#[test]
fn sweep_clears_a_worktree_dir_git_no_longer_knows() {
    let store = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (ops, task_id) = landed_board(store.path(), work.path());
    // A tool cache written after git removed the checkout: no `.git`, so
    // `git worktree remove` calls it "not a working tree".
    let residue = ops.data_root().join(".kanban/worktrees").join(&task_id);
    fs::create_dir_all(residue.join(".vite/deps")).unwrap();
    hold_worktree(&ops, &task_id, &format!("kanban/{task_id}"));

    if !Path::new("/proc/self/cwd").exists() {
        return;
    }
    assert_eq!(
        ops.sweep_deferred_cleanups().unwrap(),
        vec![task_id.clone()]
    );
    assert!(!residue.exists(), "residue directory swept");
    let task = ops.storage.load_task(&task_id).unwrap().unwrap();
    assert!(task.worktree.is_none() && task.branch.is_none());
    assert_eq!(failure_notes(&ops, &task_id), 0);
}

#[test]
fn a_cleanup_that_needs_a_human_is_not_retried_every_tick() {
    let store = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (ops, task_id) = landed_board(store.path(), work.path());
    // A branch with work the integration ref never saw: deleting it is
    // refused until a human decides.
    let branch = format!("kanban/{task_id}-extra");
    git(work.path(), &["checkout", "-q", "-b", &branch]);
    fs::write(work.path().join("late.txt"), "late\n").unwrap();
    git(work.path(), &["add", "-A"]);
    git(work.path(), &["commit", "-q", "-m", "late"]);
    git(work.path(), &["checkout", "-q", "main"]);
    hold_worktree(&ops, &task_id, &branch);

    if !Path::new("/proc/self/cwd").exists() {
        return;
    }
    for _ in 0..3 {
        assert!(ops.sweep_deferred_cleanups().unwrap().is_empty());
    }
    assert_eq!(
        failure_notes(&ops, &task_id),
        1,
        "posted once, not per tick"
    );
    let task = ops.storage.load_task(&task_id).unwrap().unwrap();
    assert_eq!(task.branch.as_deref(), Some(branch.as_str()));
    // The backoff lives on the task, so a fresh process (here: a fresh
    // board handle) does not retry at once either.
    assert!(task.cleanup_retry_at.is_some());

    // Once the backoff ran out the retry runs again, but the thread does
    // not take the same failure note a second time.
    let mut task = task;
    task.cleanup_retry_at = Some(task.cleanup_retry_at.unwrap() - chrono::Duration::hours(1));
    ops.storage.save_task(&task).unwrap();
    assert!(ops.sweep_deferred_cleanups().unwrap().is_empty());
    assert_eq!(failure_notes(&ops, &task_id), 1);
    let task = ops.storage.load_task(&task_id).unwrap().unwrap();
    assert!(task.cleanup_retry_at.unwrap() > kanban4ai::core::timefmt::now());
}
