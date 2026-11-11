mod common;

use kanban4ai::core::config::{Config, InheritGroup, SpawnHelperModel, SpawnPolicy};
use kanban4ai::core::models::{MessageKind, MessageRole, RunPhase, Task, TaskStatus};
use kanban4ai::core::operations::{Operations, SpawnRequest, TaskPatch};
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::storage::NewTask;

fn parent(ops: &Operations) -> Task {
    let task = ops
        .create_task(NewTask {
            title: "Parent".into(),
            silence: true,
            permission: Some("readonly".into()),
            ..Default::default()
        })
        .unwrap();
    ops.take_task(&task.id, "ses-parent", true)
        .unwrap()
        .unwrap()
}
fn request(parent: &Task) -> SpawnRequest {
    SpawnRequest {
        parent_id: parent.id.clone(),
        title: "Helper".into(),
        ..Default::default()
    }
}
fn config(ops: &Operations, yaml: &str) {
    let mut cfg = ops.config.load_raw().unwrap();
    cfg.inherit.insert("fan_out".into(), false.into());
    cfg.orchestration
        .insert("spawn".into(), serde_yaml_ng::from_str(yaml).unwrap());
    ops.config.save(&cfg).unwrap();
}

#[test]
fn spawn_links_queues_inherits_permissions_and_assignment() {
    let (_dir, ops, _) = common::ops_with_recorder(false);
    let parent = parent(&ops);
    let child = ops.spawn_task(request(&parent), "ses-parent").unwrap();
    assert_eq!(child.parent_task.as_deref(), Some(parent.id.as_str()));
    assert!(child.spawned && child.silence && child.explicit_assignment);
    assert_eq!(child.permission(), Some("readonly"));
    assert_eq!(child.agent_backend, parent.agent_backend);
    assert_eq!(child.ai_model, parent.ai_model);
    assert_eq!(child.ai_effort, parent.ai_effort);
    assert_eq!(child.spawn_policy, Some(SpawnPolicy::Allowed));
    assert!(!child.use_orchestrator && !child.use_designer && !child.use_reviewer);
    assert!(
        ops.get_task(&parent.id)
            .unwrap()
            .unwrap()
            .depends_on
            .is_empty()
    );
}

#[test]
fn spawn_starts_helper_directly_when_queue_cannot_dispatch() {
    let (_dir, ops, _) = common::ops_with_recorder(false);
    let parent = parent(&ops);
    let child = ops.spawn_task(request(&parent), "ses-parent").unwrap();
    // With the queue unable to dispatch (auto-launch off here) a queue entry
    // would never drain: the helper starts instead of being stranded Queued.
    assert_eq!(child.status, TaskStatus::InProgress);
    assert_ne!(child.run_phase, Some(RunPhase::Queued));
    assert!(child.session.is_some());
    assert!(ops.dispatch_queue().unwrap().is_empty());
}

#[test]
fn spawn_queues_helper_when_queue_can_dispatch() {
    let (_dir, ops, _) = common::ops_with_recorder(false);
    let mut cfg = ops.config.load_raw().unwrap();
    cfg.rules
        .insert("auto_launch_on_delegate".into(), false.into());
    cfg.auto_launch.insert("enabled".into(), true.into());
    ops.config.save(&cfg).unwrap();
    let parent = parent(&ops);
    let child = ops.spawn_task(request(&parent), "ses-parent").unwrap();
    assert_eq!(child.run_phase, Some(RunPhase::Queued));
}

#[test]
fn spawn_posts_board_notes_instead_of_agent_context() {
    let (dir, ops, _) = common::ops_with_recorder(false);
    let parent = parent(&ops);
    let child = ops.spawn_task(request(&parent), "ses-parent").unwrap();
    let threads = kanban4ai::core::thread::ThreadManager::new(dir.path()).unwrap();
    let parent_thread = threads.load(&parent.id).unwrap();
    assert!(
        parent_thread
            .messages
            .iter()
            .all(|m| !(m.kind == MessageKind::Context && m.role == MessageRole::Agent))
    );
    assert!(
        parent_thread
            .messages
            .iter()
            .any(|m| m.body.contains("Spawned helper") && m.body.contains(&child.id))
    );
    let child_thread = threads.load(&child.id).unwrap();
    assert!(
        child_thread
            .messages
            .iter()
            .any(|m| m.body.contains(&format!("Spawned by parent {}", parent.id)))
    );
}

#[test]
fn spawn_requires_live_executor_session_and_respects_forbidden() {
    let (_dir, ops, _) = common::ops_with_recorder(false);
    let mut parent = parent(&ops);
    assert!(ops.spawn_task(request(&parent), "wrong").is_err());
    for phase in [RunPhase::Design, RunPhase::Orchestrate, RunPhase::Review] {
        parent.run_phase = Some(phase);
        ops.storage.save_task(&parent).unwrap();
        assert!(ops.spawn_task(request(&parent), "ses-parent").is_err());
    }
    parent.run_phase = None;
    parent.spawn_policy = Some(SpawnPolicy::Forbidden);
    ops.storage.save_task(&parent).unwrap();
    let error = ops
        .spawn_task(request(&parent), "ses-parent")
        .unwrap_err()
        .to_string();
    assert!(error.contains("forbidden") && error.contains("task"));
    assert_eq!(ops.storage.get_all_tasks().unwrap().len(), 1);
}

#[test]
fn spawn_depth_children_and_dependency_deadlocks_are_bounded() {
    let (_dir, ops, _) = common::ops_with_recorder(false);
    config(&ops, "max_depth: 1\nmax_children: 1");
    let parent = parent(&ops);
    let mut invalid = request(&parent);
    invalid.depends_on = vec![parent.id.clone()];
    assert!(ops.spawn_task(invalid, "ses-parent").is_err());
    let child = ops.spawn_task(request(&parent), "ses-parent").unwrap();
    assert!(
        ops.spawn_task(request(&parent), "ses-parent")
            .unwrap_err()
            .to_string()
            .contains("max_children")
    );
    assert_eq!(ops.effective_spawn(&child).unwrap().source, "depth limit");
    config(&ops, "max_depth: 0");
    assert_eq!(
        ops.effective_spawn(&parent).unwrap().policy,
        SpawnPolicy::Forbidden
    );
}

#[test]
fn forced_done_requires_spawn_child_and_does_not_force_children() {
    let (_dir, ops, _) = common::ops_with_recorder(false);
    config(&ops, "policy: forced");
    let parent = parent(&ops);
    assert!(
        ops.complete_task(&parent.id, "ses-parent", true)
            .unwrap_err()
            .to_string()
            .contains("spawn_policy is forced")
    );
    let child = ops.spawn_task(request(&parent), "ses-parent").unwrap();
    assert_eq!(
        ops.effective_spawn(&child).unwrap().policy,
        SpawnPolicy::Allowed
    );
    // The spawn note is a board note now; it no longer satisfies the
    // recorded-context gate on the parent's behalf.
    kanban4ai::core::context::ContextManager::new(ops.data_root())
        .append_context(&parent.id, "Result recorded.", "agent", &ops.storage)
        .unwrap();
    assert_eq!(
        ops.complete_task(&parent.id, "ses-parent", true)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Review
    );
}

#[test]
fn helper_pool_and_explicit_override() {
    for (setting, profile) in [
        (SpawnHelperModel::ExecutorMiddle, "middle"),
        (SpawnHelperModel::ExecutorCheap, "cheap"),
    ] {
        let (_dir, ops, _) = common::ops_with_recorder(false);
        let mut cfg = ops.config.load_raw().unwrap();
        cfg.orchestration.insert("executors".into(), serde_yaml_ng::from_str("middle:\n- backend: codex\n  model: gpt-5\ncheap:\n- backend: codex\n  model: gpt-5-mini").unwrap());
        ops.config.save(&cfg).unwrap();
        let parent = parent(&ops);
        ops.update_task(
            &parent.id,
            TaskPatch {
                spawn_helper_model: Some(Some(setting)),
                ..Default::default()
            },
        )
        .unwrap();
        let child = ops.spawn_task(request(&parent), "ses-parent").unwrap();
        assert_eq!(child.role_profile.as_deref(), Some(profile));
        assert!(!child.explicit_assignment);
        let mut explicit = request(&parent);
        explicit.backend = Some("codex".into());
        explicit.model = Some("gpt-5".into());
        let child = ops.spawn_task(explicit, "ses-parent").unwrap();
        assert!(child.explicit_assignment);
        assert!(child.role_profile.is_none());
        assert_eq!(child.ai_model.as_deref(), Some("gpt-5"));
    }
}

#[test]
fn three_level_resolution_and_legacy_defaults() {
    let (dir, _) = common::quiet_board(false);
    let global = dir.path().join("global.yaml");
    std::fs::write(
        &global,
        "orchestration:\n  spawn:\n    policy: forbidden\n    helper_model: executor_cheap\n",
    )
    .unwrap();
    let cfg = Config::new(dir.path()).with_global_file(Some(global));
    let task = Task::new("TASK-001", "Example");
    assert!(cfg.inherit_flags().unwrap().get(InheritGroup::FanOut));
    let effective = cfg.effective_spawn(&task, 0).unwrap();
    assert_eq!(effective.policy, SpawnPolicy::Forbidden);
    assert_eq!(effective.source, "global");
    let mut raw = cfg.load_raw().unwrap();
    raw.inherit.insert("fan_out".into(), false.into());
    raw.orchestration.insert(
        "spawn".into(),
        serde_yaml_ng::from_str("policy: forced\nhelper_model: same").unwrap(),
    );
    cfg.save(&raw).unwrap();
    assert_eq!(cfg.effective_spawn(&task, 0).unwrap().source, "project");
    let mut task = task;
    task.spawn_policy = Some(SpawnPolicy::Allowed);
    assert_eq!(cfg.effective_spawn(&task, 0).unwrap().source, "task");
    task.spawn_policy = None;
    task.spawned = true;
    assert_eq!(
        cfg.effective_spawn(&task, 0).unwrap().policy,
        SpawnPolicy::Allowed
    );
}

#[test]
fn wait_records_targets_and_wakes_early_only_after_children_finish() {
    let (dir, ops, _) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let mut child = ops.spawn_task(request(&parent), "ses-parent").unwrap();
    ops.declare_waiting_for(&parent.id, "ses-parent", &[child.id.clone()])
        .unwrap();
    let mgr = SessionManager::new(dir.path());
    let mut session = mgr.load_session("ses-parent").unwrap();
    assert_eq!(session.wait_for, vec![child.id.clone()]);
    session.wait_exited = true;
    mgr.save_session(&session).unwrap();
    assert!(ops.wake_expired_waits().unwrap().is_empty());
    child.status = TaskStatus::Review;
    ops.storage.save_task(&child).unwrap();
    assert_eq!(ops.wake_expired_waits().unwrap().len(), 1);
}

#[test]
fn output_shares_upstream_digest_and_wait_classifies_terminal_states() {
    let (dir, ops, _) = common::ops_with_recorder(false);
    let parent = parent(&ops);
    let output = ops.task_output(&parent.id).unwrap();
    let threads = kanban4ai::core::thread::ThreadManager::new(dir.path()).unwrap();
    let budget = ops
        .config
        .get_orchestration()
        .unwrap()
        .orchestrator
        .upstream_budget_chars as usize;
    assert_eq!(
        output.digest,
        kanban4ai::core::context::result_digest(&threads, &parent.id, budget).unwrap()
    );
    assert!(!output.state.terminal);
    let states = ops.wait_poll(&["TASK-999".into()]).unwrap();
    assert_eq!(states[0].state, "missing");
    assert!(states[0].terminal && !states[0].success);
}

#[test]
fn spawn_helpers_share_parent_concurrency_caps() {
    let (_dir, ops, recorder) = common::ops_with_recorder(true);
    let mut cfg = ops.config.load_raw().unwrap();
    cfg.orchestration
        .insert("max_running_total".into(), 1.into());
    ops.config.save(&cfg).unwrap();
    let parent = parent(&ops);
    let child = ops.spawn_task(request(&parent), "ses-parent").unwrap();
    assert!(ops.spawn_wait_needs_slot(&[child.id.clone()]).unwrap());
    assert!(ops.dispatch_queue().unwrap().is_empty());
    let before = recorder.calls().len();
    ops.declare_waiting_for(&parent.id, "ses-parent", &[child.id.clone()])
        .unwrap();
    assert_eq!(ops.dispatch_queue().unwrap().len(), 1);
    assert_eq!(recorder.calls().len(), before + 1);
}

#[test]
fn dependency_spawn_stays_todo_until_upstream_is_ready() {
    let (_dir, ops, _) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let mut dependency = ops.create_task(NewTask::titled("Upstream")).unwrap();
    let mut req = request(&parent);
    req.depends_on.push(dependency.id.clone());
    let child = ops.spawn_task(req, "ses-parent").unwrap();
    assert_eq!(child.status, TaskStatus::Todo);
    dependency.status = TaskStatus::Review;
    ops.storage.save_task(&dependency).unwrap();
    assert_eq!(ops.dispatch_ready_dependents().unwrap(), vec![child.id]);
}

#[test]
fn concurrent_spawns_cannot_exceed_child_cap() {
    let (dir, ops, _) = common::ops_with_recorder(false);
    config(&ops, "max_children: 1");
    let parent = parent(&ops);
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let root = dir.path().to_path_buf();
            let req = request(&parent);
            std::thread::spawn(move || Operations::new(root).spawn_task(req, "ses-parent").is_ok())
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>(),
        1
    );
}

#[test]
fn forced_with_impossible_spawn_never_deadlocks() {
    for yaml in [
        "policy: forced\nmax_depth: 0",
        "policy: forced\nmax_children: 0",
    ] {
        let (_dir, ops, _) = common::ops_with_recorder(false);
        config(&ops, yaml);
        let parent = parent(&ops);
        // Spawning stays impossible (depth limit / child cap) ...
        assert!(!ops.effective_spawn(&parent).unwrap().requires_helper);
        assert!(ops.spawn_task(request(&parent), "ses-parent").is_err());
        // ... so done must not demand the impossible helper.
        kanban4ai::core::context::ContextManager::new(ops.data_root())
            .append_context(&parent.id, "Result recorded.", "agent", &ops.storage)
            .unwrap();
        assert_eq!(
            ops.complete_task(&parent.id, "ses-parent", true)
                .unwrap()
                .unwrap()
                .status,
            TaskStatus::Review
        );
    }
}
