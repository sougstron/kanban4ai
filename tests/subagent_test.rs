//! Subagents v2 (TASK-594): child runs inside a parent executor. They never
//! reach a board column, Review, or landing; they run outside the board
//! queue; and a parent in `sub wait` wakes exactly once, on the event, never
//! on a timer.

mod common;

use common::RecordingLauncher;
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::models::{IntegrationState, SessionStatus, SubState, Task, TaskStatus};
use kanban4ai::core::operations::{AgentExitOutcome, Operations};
use kanban4ai::core::project::ProjectStore;
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::stats;
use kanban4ai::core::storage::{NewTask, Storage};
use kanban4ai::core::subagents::{SubSpawnRequest, SubWait};
use kanban4ai::core::telemetry::TokenBreakdown;
use kanban4ai::core::thread::ThreadManager;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

fn orchestration(ops: &Operations, key: &str, yaml: &str) {
    let mut cfg = ops.config.load_raw().unwrap();
    cfg.orchestration
        .insert(key.into(), serde_yaml_ng::from_str(yaml).unwrap());
    ops.config.save(&cfg).unwrap();
}

fn parent(ops: &Operations) -> Task {
    let task = ops
        .create_task(NewTask {
            title: "Parent".into(),
            silence: true,
            ..Default::default()
        })
        .unwrap();
    ops.take_task(&task.id, "ses-parent", true)
        .unwrap()
        .unwrap()
}

fn spawn(ops: &Operations, parent: &Task, title: &str) -> Task {
    ops.sub_spawn(
        SubSpawnRequest {
            parent_id: parent.id.clone(),
            title: title.into(),
            ..Default::default()
        },
        "ses-parent",
    )
    .unwrap()
}

fn load(ops: &Operations, id: &str) -> Task {
    ops.get_task(id).unwrap().unwrap()
}

fn launches(recorder: &RecordingLauncher, id: &str) -> usize {
    recorder.calls().iter().filter(|call| call.0 == id).count()
}

/// The parent declares `sub wait` and its process exits, as a real agent
/// does after exit code 3.
fn parent_waits(ops: &Operations, parent: &Task) {
    assert!(matches!(
        ops.sub_wait(&parent.id, "ses-parent", &[]).unwrap(),
        SubWait::Waiting(_)
    ));
    assert!(matches!(
        ops.reconcile_agent_exit(&parent.id, "ses-parent", 0)
            .unwrap(),
        AgentExitOutcome::Waiting
    ));
}

fn finish(ops: &Operations, id: &str, summary: &str) -> Task {
    let sub = load(ops, id);
    ops.sub_done(id, sub.session.as_deref(), Some(summary))
        .unwrap()
}

#[test]
fn done_skips_review_and_landing_and_wakes_the_parent() {
    let (dir, ops, recorder) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let child = spawn(&ops, &parent, "Research");
    assert_eq!(child.sub_state, Some(SubState::Running));
    assert_eq!(launches(&recorder, &child.id), 1);
    // Never on the board.
    assert!(
        ops.list_tasks(None, None, "number", "asc")
            .unwrap()
            .iter()
            .all(|task| task.id != child.id)
    );
    parent_waits(&ops, &parent);
    let before = launches(&recorder, &parent.id);

    let done = finish(&ops, &child.id, "Result: the answer is 42");
    assert_eq!(done.sub_state, Some(SubState::Done));
    assert_eq!(done.status, TaskStatus::Done);
    assert!(!done.review_unseen);
    assert_eq!(done.integration, IntegrationState::None);

    // Woken at once, outside the queue, without spending auto_resumes.
    assert_eq!(launches(&recorder, &parent.id), before + 1);
    let woken = load(&ops, &parent.id);
    assert_eq!(woken.auto_resumes, 0);
    assert_ne!(woken.session.as_deref(), Some("ses-parent"));
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&parent.id)
        .unwrap();
    assert!(thread.messages.iter().any(|m| {
        m.body
            .contains(&format!("Subagents finished: {} done", child.id))
    }));
    assert!(
        ops.task_output(&child.id)
            .unwrap()
            .digest
            .contains("the answer is 42")
    );
}

#[test]
fn trigger_wait_ignores_time_and_wakes_exactly_once() {
    let (dir, ops, recorder) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let a = spawn(&ops, &parent, "A");
    let b = spawn(&ops, &parent, "B");
    parent_waits(&ops, &parent);
    // However much time passes, a trigger wait has no deadline.
    let mgr = SessionManager::new(dir.path());
    let mut session = mgr.load_session("ses-parent").unwrap();
    assert!(session.wait_until.is_none() && session.wait_trigger.is_some());
    session.last_seen -= chrono::Duration::days(1);
    session.started_at -= chrono::Duration::days(1);
    mgr.save_session(&session).unwrap();
    let before = launches(&recorder, &parent.id);
    for _ in 0..3 {
        ops.wake_expired_waits().unwrap();
        ops.dispatch_queue().unwrap();
    }
    assert_eq!(launches(&recorder, &parent.id), before);
    assert_eq!(
        mgr.load_session("ses-parent").unwrap().status,
        SessionStatus::Active
    );

    finish(&ops, &a.id, "A done");
    assert_eq!(launches(&recorder, &parent.id), before);
    ops.sub_cancel(&b.id, "not needed").unwrap();
    assert_eq!(launches(&recorder, &parent.id), before + 1);
    ops.wake_expired_waits().unwrap();
    ops.pump_subagents().unwrap();
    assert_eq!(launches(&recorder, &parent.id), before + 1);
    assert_eq!(load(&ops, &parent.id).auto_resumes, 0);
}

#[test]
fn subagents_bypass_board_caps_and_queue_fifo_per_parent() {
    let (_dir, ops, recorder) = common::ops_with_recorder(true);
    let mut cfg = ops.config.load_raw().unwrap();
    cfg.orchestration
        .insert("max_running_total".into(), 1.into());
    ops.config.save(&cfg).unwrap();
    orchestration(&ops, "max_running_per_role", "executor: 1");
    orchestration(
        &ops,
        "subagents",
        "max_parallel_per_parent: 3\nmax_children: 6",
    );
    // The parent holds the only board slot.
    let parent = parent(&ops);
    let subs: Vec<Task> = (0..6)
        .map(|i| spawn(&ops, &parent, &format!("Sub {i}")))
        .collect();
    let states = |ops: &Operations| -> Vec<Option<SubState>> {
        subs.iter().map(|s| load(ops, &s.id).sub_state).collect()
    };
    let running = Some(SubState::Running);
    let queued = Some(SubState::Queued);
    assert_eq!(
        states(&ops),
        vec![running, running, running, queued, queued, queued]
    );
    // Nothing ever enters the board queue.
    assert!(ops.dispatch_queue().unwrap().is_empty());
    finish(&ops, &subs[0].id, "first");
    assert_eq!(
        states(&ops),
        vec![
            Some(SubState::Done),
            running,
            running,
            running,
            queued,
            queued
        ]
    );
    assert_eq!(launches(&recorder, &subs[3].id), 1);
    assert_eq!(launches(&recorder, &subs[4].id), 0);
}

#[test]
fn parent_stop_and_recover_cascade_and_done_is_refused_while_live() {
    let (dir, ops, _) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let a = spawn(&ops, &parent, "A");
    let b = spawn(&ops, &parent, "B");
    ContextManager::new(dir.path())
        .append_context(&parent.id, "Parent result.", "agent", &ops.storage)
        .unwrap();
    assert!(
        ops.complete_task(&parent.id, "ses-parent", true)
            .unwrap_err()
            .to_string()
            .contains("live subagents")
    );
    ops.stop_task(&parent.id).unwrap();
    let mgr = SessionManager::new(dir.path());
    for sub in [&a, &b] {
        let current = load(&ops, &sub.id);
        assert_eq!(current.sub_state, Some(SubState::Cancelled));
        assert!(!mgr.is_session_active(current.session.as_deref().unwrap()));
    }

    let other = ops.create_task(NewTask::titled("Other parent")).unwrap();
    ops.take_task(&other.id, "ses-other", true).unwrap();
    let c = ops
        .sub_spawn(
            SubSpawnRequest {
                parent_id: other.id.clone(),
                title: "C".into(),
                ..Default::default()
            },
            "ses-other",
        )
        .unwrap();
    ops.recover_task(&other.id).unwrap();
    assert_eq!(load(&ops, &c.id).sub_state, Some(SubState::Cancelled));
}

#[test]
fn crash_budget_fails_the_subagent_and_wakes_the_parent() {
    let (_dir, ops, recorder) = common::ops_with_recorder(true);
    orchestration(&ops, "auto_restart", "enabled: true\ndelays_minutes: [1]");
    let parent = parent(&ops);
    let child = spawn(&ops, &parent, "Flaky");
    parent_waits(&ops, &parent);
    let before = launches(&recorder, &parent.id);

    // First crash: a restart inside the parent, never the board queue.
    let first = child.session.clone().unwrap();
    ops.reconcile_agent_exit(&child.id, &first, 1).unwrap();
    let mut crashed = load(&ops, &child.id);
    assert_eq!(crashed.sub_state, Some(SubState::Running));
    assert!(crashed.restart_at.is_some());
    assert_ne!(
        crashed.run_phase,
        Some(kanban4ai::core::models::RunPhase::Queued)
    );
    crashed.restart_at = Some(kanban4ai::core::timefmt::now() - chrono::Duration::seconds(1));
    ops.storage.save_task(&crashed).unwrap();
    ops.pump_subagents().unwrap();
    let restarted = load(&ops, &child.id);
    assert_eq!(restarted.sub_state, Some(SubState::Running));
    assert_eq!(restarted.crash_restarts, 1);
    assert_ne!(restarted.session.as_deref(), Some(first.as_str()));
    assert_eq!(launches(&recorder, &parent.id), before);

    // Budget spent: failed, terminal, and the parent wakes.
    ops.reconcile_agent_exit(&child.id, restarted.session.as_deref().unwrap(), 1)
        .unwrap();
    let failed = load(&ops, &child.id);
    assert_eq!(failed.sub_state, Some(SubState::Failed));
    assert!(failed.sub_reason.unwrap().contains("budget"));
    assert_eq!(launches(&recorder, &parent.id), before + 1);
}

#[test]
fn stall_watchdog_fails_a_silent_subagent() {
    let (dir, ops, _) = common::ops_with_recorder(true);
    orchestration(&ops, "subagents", "stall_timeout: 1");
    let parent = parent(&ops);
    let child = spawn(&ops, &parent, "Hung");
    let mgr = SessionManager::new(dir.path());
    let mut session = mgr.load_session(child.session.as_deref().unwrap()).unwrap();
    session.started_at -= chrono::Duration::seconds(30);
    mgr.save_session(&session).unwrap();
    ops.pump_subagents().unwrap();
    let failed = load(&ops, &child.id);
    assert_eq!(failed.sub_state, Some(SubState::Failed));
    assert!(failed.sub_reason.unwrap().contains("stalled"));
}

#[test]
fn legacy_spawned_helpers_stay_visible_board_tasks() {
    let (_dir, ops, _) = common::ops_with_recorder(false);
    let parent = ops.create_task(NewTask::titled("Parent")).unwrap();
    let mut legacy = ops
        .create_task(NewTask {
            title: "Old helper".into(),
            parent_task: Some(parent.id.clone()),
            ..Default::default()
        })
        .unwrap();
    legacy.spawned = true;
    legacy.status = TaskStatus::Review;
    ops.storage.save_task(&legacy).unwrap();
    assert!(!legacy.is_subagent());
    let listed = ops
        .list_tasks(Some("review"), None, "number", "asc")
        .unwrap();
    assert!(listed.iter().any(|task| task.id == legacy.id));
    assert_eq!(
        ops.wait_poll(&[legacy.id.clone()]).unwrap()[0].state,
        "review"
    );
    // A legacy helper never blocks its parent's done.
    ops.require_no_live_subagents(&parent.id).unwrap();
}

#[test]
fn archive_and_delete_cascade_to_subagents() {
    let (_dir, ops, _) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let child = spawn(&ops, &parent, "Child");
    finish(&ops, &child.id, "ok");
    ops.move_task(&parent.id, "archive", false).unwrap();
    assert_eq!(load(&ops, &child.id).status, TaskStatus::Archive);

    let other = ops.create_task(NewTask::titled("Other")).unwrap();
    ops.take_task(&other.id, "ses-other", true).unwrap();
    let gone = ops
        .sub_spawn(
            SubSpawnRequest {
                parent_id: other.id.clone(),
                title: "Gone".into(),
                ..Default::default()
            },
            "ses-other",
        )
        .unwrap();
    ops.abandon_task(&other.id).unwrap();
    assert!(ops.storage.load_task(&gone.id).unwrap().is_none());
}

#[test]
fn stats_fold_subagents_into_the_parent() {
    let (dir, ops, _) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let child = spawn(&ops, &parent, "Counted");
    let breakdown = TokenBreakdown {
        input: 600,
        output: 400,
        cache_read: 0,
        cache_write: 0,
    };
    stats::record_usage(
        dir.path(),
        &child.id,
        "ses-sub-usage",
        1000,
        Some(&breakdown),
        &stats::Tags::default(),
    );
    let analytics = stats::task_analytics(dir.path(), &parent.id, parent.created_at);
    assert_eq!(analytics.subagents.count, 1);
    assert_eq!(analytics.subagents.tokens, 1000);
    assert_eq!(analytics.breakdown.output, 400);
    let report = stats::load(dir.path());
    assert!(report.usage.iter().all(|usage| usage.task_id == parent.id));
}

// ------------------------------------------------------------ git worktrees

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn from_parent_sees_uncommitted_work_and_merge_reaches_only_the_parent() {
    let store = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    git(work.path(), &["init", "-q", "-b", "main"]);
    git(
        work.path(),
        &["config", "user.email", "kanban@example.test"],
    );
    git(work.path(), &["config", "user.name", "Kanban Test"]);
    fs::write(work.path().join("committed.txt"), "committed\n").unwrap();
    git(work.path(), &["add", "-A"]);
    git(work.path(), &["commit", "-q", "-m", "base"]);
    let project = ProjectStore::at(store.path())
        .add(work.path(), None)
        .unwrap()
        .project;
    Storage::new(&project.data_root).init_board().unwrap();
    common::write_quiet_config(&project.data_root, true);
    let recorder = RecordingLauncher::new();
    let ops = Operations::for_project_with_launcher(&project, Box::new(recorder.clone()));
    orchestration(&ops, "isolation", "mode: required");

    let parent = parent(&ops);
    let parent = load(&ops, &parent.id);
    let parent_wt = ops
        .storage
        .worktrees_dir
        .join(parent.worktree.as_deref().expect("parent worktree"));
    // Uncommitted parent work the subagent must see.
    fs::write(parent_wt.join("draft.txt"), "parent draft\n").unwrap();

    let child = spawn(&ops, &parent, "Extend the draft");
    let child = load(&ops, &child.id);
    let child_wt = ops
        .storage
        .worktrees_dir
        .join(child.worktree.as_deref().expect("child worktree"));
    assert_eq!(
        fs::read_to_string(child_wt.join("draft.txt")).unwrap(),
        "parent draft\n"
    );
    fs::write(child_wt.join("result.txt"), "subagent result\n").unwrap();
    finish(&ops, &child.id, "wrote result.txt");
    // Done never lands: the project is untouched, the branch is kept.
    assert!(!work.path().join("result.txt").exists());
    let branch = load(&ops, &child.id).branch.expect("branch kept");
    git(work.path(), &["rev-parse", "--verify", &branch]);
    assert!(
        ops.task_output(&child.id)
            .unwrap()
            .diff_stat
            .is_some_and(|stat| stat.contains("1 file"))
    );

    ops.sub_merge(&child.id).unwrap();
    assert_eq!(
        fs::read_to_string(parent_wt.join("result.txt")).unwrap(),
        "subagent result\n"
    );
    assert_eq!(
        fs::read_to_string(parent_wt.join("draft.txt")).unwrap(),
        "parent draft\n"
    );
    assert!(!work.path().join("result.txt").exists());
    let merged = load(&ops, &child.id);
    assert!(merged.branch.is_none() && merged.worktree.is_none());
    assert_eq!(merged.integration, IntegrationState::Landed);
}
