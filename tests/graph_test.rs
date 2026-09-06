//! The dependency graph end to end: `depends_on` edges, the readiness sweep,
//! orchestrator planning, role-roster failover and the upstream context an
//! edge carries.

mod common;

use std::fs;

use kanban4ai::agent::{build_agent_prompt, upcoming_run_plan};
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::graph::Plan;
use kanban4ai::core::models::{Role, RunPhase, TaskStatus};
use kanban4ai::core::operations::Operations;
use kanban4ai::core::storage::{NewTask, Storage};

use common::{RecordingLauncher, ops_with_recorder};

/// Board with auto-launch and the queue on, plus the given `orchestration:`
/// body, so the readiness sweep and the dispatcher actually run.
fn graph_board(orch_body: &str) -> (tempfile::TempDir, Operations, RecordingLauncher) {
    let (dir, _ops, recorder) = ops_with_recorder(true);
    fs::write(
        dir.path().join(".kanban/config.yaml"),
        format!(
            "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\n\
             rules:\n  auto_launch_on_delegate: false\norchestration:\n{orch_body}"
        ),
    )
    .unwrap();
    let ops = Operations::with_launcher(dir.path(), Box::new(recorder.clone()));
    (dir, ops, recorder)
}

fn todo(ops: &Operations, title: &str) -> kanban4ai::core::models::Task {
    ops.create_task(NewTask {
        title: title.into(),
        agent_backend: Some("opencode".into()),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn depends_on_is_omitted_from_frontmatter_while_empty() {
    let (dir, ops, _recorder) = graph_board("  queue_enabled: true\n");
    let task = todo(&ops, "No edges");
    let path = dir
        .path()
        .join(".kanban/tasks/todo")
        .join(format!("{}.md", task.id));
    let text = fs::read_to_string(&path).unwrap();
    for key in [
        "depends_on",
        "needs",
        "parent_task",
        "role_profile",
        "roster_index",
        "use_orchestrator",
        "orchestrated",
    ] {
        assert!(
            !text.contains(key),
            "{key} must stay out of the frontmatter of a task that does not use it:\n{text}"
        );
    }

    // …and a task that does use them round-trips through disk unchanged.
    let updated = ops
        .set_dependencies(&task.id, vec![])
        .unwrap()
        .expect("task exists");
    assert!(updated.depends_on.is_empty());
}

#[test]
fn a_dependency_gates_the_sweep_until_every_upstream_finished() {
    let (_dir, ops, _recorder) = graph_board("  queue_enabled: true\n  max_running_total: 5\n");
    let first = todo(&ops, "First");
    let second = todo(&ops, "Second");
    let downstream = todo(&ops, "Downstream");
    ops.set_dependencies(&downstream.id, vec![first.id.clone(), second.id.clone()])
        .unwrap()
        .expect("edges set");

    assert!(
        ops.dispatch_ready_dependents().unwrap().is_empty(),
        "nothing is ready while both dependencies are open"
    );

    ops.move_task(&first.id, TaskStatus::Review.as_str(), false)
        .unwrap();
    assert!(
        ops.dispatch_ready_dependents().unwrap().is_empty(),
        "one finished dependency is not an AND-join"
    );

    // A human move satisfies an edge exactly like an agent `done` does.
    ops.move_task(&second.id, TaskStatus::Done.as_str(), false)
        .unwrap();
    let started = ops.dispatch_ready_dependents().unwrap();
    assert_eq!(started, vec![downstream.id.clone()]);

    let queued = ops.get_task(&downstream.id).unwrap().unwrap();
    assert_eq!(queued.status, TaskStatus::InProgress);
    assert_eq!(
        queued.run_phase,
        Some(RunPhase::Queued),
        "a ready node enters the queue, it never launches around the caps"
    );
    assert!(
        ops.dispatch_ready_dependents().unwrap().is_empty(),
        "the sweep is idempotent"
    );
}

#[test]
fn a_deleted_dependency_releases_the_node_instead_of_deadlocking_it() {
    let (_dir, ops, _recorder) = graph_board("  queue_enabled: true\n");
    let upstream = todo(&ops, "Will be abandoned");
    let downstream = todo(&ops, "Downstream");
    ops.set_dependencies(&downstream.id, vec![upstream.id.clone()])
        .unwrap();
    ops.abandon_task(&upstream.id).unwrap();

    assert_eq!(
        ops.dispatch_ready_dependents().unwrap(),
        vec![downstream.id.clone()]
    );
}

#[test]
fn a_cycle_is_refused_at_write_time() {
    let (_dir, ops, _recorder) = graph_board("  queue_enabled: true\n");
    let a = todo(&ops, "A");
    let b = todo(&ops, "B");
    ops.set_dependencies(&b.id, vec![a.id.clone()]).unwrap();

    let err = ops
        .set_dependencies(&a.id, vec![b.id.clone()])
        .expect_err("A → B → A must be refused");
    assert!(err.to_string().contains("cycle"), "{err}");
    assert!(
        ops.get_task(&a.id).unwrap().unwrap().depends_on.is_empty(),
        "a refused edge is not written"
    );

    let err = ops
        .set_dependencies(&a.id, vec!["TASK-404".into()])
        .expect_err("unknown dependency must be refused");
    assert!(err.to_string().contains("not found"), "{err}");
}

/// The orchestrated task plans a graph, becomes its join node, and only the
/// roots start.
#[test]
fn an_orchestrator_plan_builds_the_graph_and_starts_only_its_roots() {
    let (dir, ops, _recorder) = graph_board(
        "  queue_enabled: true\n  max_running_total: 5\n  \
         orchestrator:\n    max_subtasks: 4\n  \
         roles:\n    cheap:\n    - claude/haiku\n    - opencode/openai/gpt-5.5\n",
    );
    let parent = ops
        .create_task(NewTask {
            title: "Big feature".into(),
            agent_backend: Some("opencode".into()),
            use_orchestrator: true,
            ..Default::default()
        })
        .unwrap();

    // The first run is the planning pass, before any designer pass.
    let config = ops.config.load().unwrap();
    assert_eq!(
        upcoming_run_plan(&config, &parent).unwrap().1,
        RunPhase::Orchestrate
    );

    let session = ops.start_task(&parent.id).unwrap().expect("started");
    let session = session.as_str();
    let running = ops.get_task(&parent.id).unwrap().unwrap();
    assert_eq!(running.run_phase, Some(RunPhase::Orchestrate));

    // Finishing before a plan exists is refused: it would leave the task
    // looking planned with no graph behind it.
    assert!(
        ops.complete_task(&parent.id, session, true).is_err(),
        "the orchestrator cannot finish without a plan"
    );

    let plan = Plan::parse(
        "summary: split by deliverable\n\
         nodes:\n\
         - key: schema\n  title: Add the field\n  role: cheap\n\
         - key: docs\n  title: Document it\n  depends_on: [schema]\n  needs: the field name\n",
    )
    .unwrap();
    let outcome = ops.apply_plan(&parent.id, &plan, Some(session)).unwrap();
    assert_eq!(outcome.created.len(), 2);
    let schema = &outcome.created[0];
    let docs = &outcome.created[1];
    assert_eq!(docs.depends_on, vec![schema.id.clone()]);
    assert_eq!(docs.needs.as_deref(), Some("the field name"));
    assert_eq!(schema.role_profile.as_deref(), Some("cheap"));
    assert_eq!(schema.agent_backend.as_deref(), Some("claude"));
    assert_eq!(schema.ai_model.as_deref(), Some("haiku"));
    assert_eq!(schema.parent_task.as_deref(), Some(parent.id.as_str()));
    assert_eq!(
        outcome.parent.depends_on,
        vec![schema.id.clone(), docs.id.clone()],
        "the orchestrated task joins on every node it planned"
    );

    // A second plan would fork the graph.
    assert!(ops.apply_plan(&parent.id, &plan, Some(session)).is_err());

    ops.complete_task(&parent.id, session, true).unwrap();
    let joined = ops.get_task(&parent.id).unwrap().unwrap();
    assert_eq!(joined.status, TaskStatus::Todo);
    assert_eq!(joined.run_phase, None);
    assert!(joined.orchestrated);

    // Only the root started; the dependent node waits for it.
    let root = ops.get_task(&schema.id).unwrap().unwrap();
    assert_eq!(root.status, TaskStatus::InProgress);
    assert_eq!(root.run_phase, Some(RunPhase::Queued));
    assert_eq!(
        ops.get_task(&docs.id).unwrap().unwrap().status,
        TaskStatus::Todo
    );

    // The join node runs as an executor once the graph is finished.
    let config = ops.config.load().unwrap();
    assert_eq!(
        upcoming_run_plan(&config, &joined).unwrap().1,
        RunPhase::Execute
    );
    for node in [&schema.id, &docs.id] {
        let mut task = ops.get_task(node).unwrap().unwrap();
        task.status = TaskStatus::Review;
        ops.storage.save_task(&task).unwrap();
    }
    assert_eq!(
        ops.dispatch_ready_dependents().unwrap(),
        vec![parent.id.clone()]
    );
    drop(dir);
}

#[test]
fn a_plan_that_cannot_run_creates_nothing() {
    let (_dir, ops, _recorder) = graph_board("  queue_enabled: true\n");
    let parent = ops
        .create_task(NewTask {
            title: "Planner".into(),
            use_orchestrator: true,
            ..Default::default()
        })
        .unwrap();
    let before = ops.list_tasks(None, None, "id", "asc").unwrap().len();

    let plan = Plan::parse(
        "nodes:\n\
         - key: a\n  title: A\n  depends_on: [b]\n\
         - key: b\n  title: B\n  depends_on: [a]\n",
    )
    .unwrap();
    let err = ops
        .apply_plan(&parent.id, &plan, None)
        .expect_err("a cyclic plan must be refused");
    assert!(err.to_string().contains("cyclic"), "{err}");
    assert_eq!(
        ops.list_tasks(None, None, "id", "asc").unwrap().len(),
        before,
        "validation runs before anything is created"
    );
    assert!(!ops.get_task(&parent.id).unwrap().unwrap().orchestrated);
}

/// A subscription limit is the one failure another model absorbs: the node
/// moves to the next roster entry and re-queues instead of parking until the
/// provider's window rolls over.
#[test]
fn a_limit_crash_fails_over_to_the_next_model_in_the_roster() {
    let (_dir, ops, _recorder) = graph_board(
        "  queue_enabled: true\n  max_running_total: 5\n  \
         auto_restart:\n    enabled: true\n    delays_minutes: [1, 30]\n  \
         roles:\n    tier:\n    - claude/opus\n    - claude/sonnet\n",
    );
    let mut task = ops
        .create_task(NewTask {
            title: "Node".into(),
            agent_backend: Some("claude".into()),
            ai_model: Some("opus".into()),
            role_profile: Some("tier".into()),
            ..Default::default()
        })
        .unwrap();
    task.status = TaskStatus::InProgress;
    task.run_phase = Some(RunPhase::Execute);
    ops.storage.save_task(&task).unwrap();

    assert!(
        ops.advance_role_roster(&task.id, "quota").unwrap(),
        "the roster has a second candidate"
    );
    let moved = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(moved.roster_index, 1);
    assert_eq!(moved.ai_model.as_deref(), Some("sonnet"));
    assert_eq!(
        moved.run_phase,
        Some(RunPhase::Queued),
        "the failover re-queues immediately instead of waiting for the reset"
    );
    assert_eq!(
        moved.restart_at, None,
        "no backoff: the point is that another model is free now"
    );

    assert!(
        !ops.advance_role_roster(&task.id, "quota").unwrap(),
        "the roster length bounds the failover"
    );
}

/// The edge's context half: a dependency's results reach the dependent's
/// prompt, and a chained task's do not.
#[test]
fn only_depends_on_carries_upstream_results_into_the_prompt() {
    let (dir, ops, _recorder) =
        graph_board("  queue_enabled: true\n  orchestrator:\n    upstream_budget_chars: 500\n");
    let upstream = todo(&ops, "Upstream work");
    ContextManager::new(ops.data_root())
        .append_context(
            &upstream.id,
            "the field is called depends_on and lives on Task",
            "agent",
            &ops.storage,
        )
        .unwrap();
    ops.move_task(&upstream.id, TaskStatus::Review.as_str(), false)
        .unwrap();

    let mut dependent = todo(&ops, "Dependent");
    dependent.depends_on = vec![upstream.id.clone()];
    dependent.needs = Some("the field name".into());
    ops.storage.save_task(&dependent).unwrap();

    let prompt =
        build_agent_prompt(ops.roots(), &dependent, "ses-test", false, Role::Executor).unwrap();
    assert!(prompt.contains("Upstream results"), "{prompt}");
    assert!(prompt.contains(&upstream.id), "{prompt}");
    assert!(
        prompt.contains("the field is called depends_on"),
        "{prompt}"
    );
    assert!(
        prompt.contains("What this task needs from upstream"),
        "the orchestrator's context contract is shown: {prompt}"
    );

    // Chaining is a human's "run this next"; the two tasks often share
    // nothing but their order, so no context crosses that edge.
    let mut chained = todo(&ops, "Chained");
    chained.chained_to = Some(upstream.id.clone());
    ops.storage.save_task(&chained).unwrap();
    let prompt =
        build_agent_prompt(ops.roots(), &chained, "ses-test", false, Role::Executor).unwrap();
    assert!(!prompt.contains("Upstream results"), "{prompt}");
    drop(dir);
}

/// Role instructions are the opposite of `AGENTS.md`: only the role they name
/// ever sees them, and only when that role is launched.
#[test]
fn role_instructions_reach_only_their_own_role() {
    let (dir, ops, _recorder) = graph_board("  queue_enabled: true\n");
    let instructions = dir.path().join(".kanban/instructions");
    fs::create_dir_all(&instructions).unwrap();
    fs::write(
        instructions.join("orchestrator.md"),
        "Prefer chains over fan-out in this repo.",
    )
    .unwrap();
    fs::write(instructions.join("executor.md"), "Run cargo fmt last.").unwrap();

    let task = todo(&ops, "Anything");
    let orchestrator =
        build_agent_prompt(ops.roots(), &task, "ses-test", false, Role::Orchestrator).unwrap();
    assert!(
        orchestrator.contains("Prefer chains over fan-out"),
        "{orchestrator}"
    );
    assert!(
        !orchestrator.contains("Run cargo fmt last"),
        "{orchestrator}"
    );

    let executor =
        build_agent_prompt(ops.roots(), &task, "ses-test", false, Role::Executor).unwrap();
    assert!(executor.contains("Run cargo fmt last"), "{executor}");
    assert!(
        !executor.contains("Prefer chains over fan-out"),
        "{executor}"
    );

    // The reviewer has no file, so nothing is appended and nothing breaks.
    let reviewer =
        build_agent_prompt(ops.roots(), &task, "ses-test", false, Role::Reviewer).unwrap();
    assert!(
        !reviewer.contains("Project instructions for the"),
        "{reviewer}"
    );
}

/// The orchestrator prompt is role-scoped: the plan schema and the roster list
/// are handed to the planner only, never to every session on the board.
#[test]
fn the_orchestrator_prompt_lists_the_configured_rosters() {
    let (_dir, ops, _recorder) = graph_board(
        "  queue_enabled: true\n  orchestrator:\n    max_subtasks: 3\n  \
         roles:\n    cheap:\n    - claude/haiku\n    - opencode/openai/gpt-5.5\n",
    );
    let task = todo(&ops, "Plan me");
    let prompt =
        build_agent_prompt(ops.roots(), &task, "ses-test", false, Role::Orchestrator).unwrap();
    assert!(prompt.contains("ORCHESTRATOR"), "{prompt}");
    assert!(prompt.contains("at most 3 nodes"), "{prompt}");
    assert!(
        prompt.contains("cheap: claude/haiku → opencode/openai/gpt-5.5"),
        "{prompt}"
    );
    assert!(prompt.contains("kanban plan"), "{prompt}");

    let executor =
        build_agent_prompt(ops.roots(), &task, "ses-test", false, Role::Executor).unwrap();
    assert!(
        !executor.contains("cheap: claude/haiku"),
        "the roster list is not charged to every session: {executor}"
    );
}

/// Legacy boards keep parsing, and none of the graph fields appear in
/// frontmatter that never carried them.
#[test]
fn fixture_tasks_gain_no_graph_keys_on_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    for entry in fs::read_dir(common::fixtures_dir().join("tasks")).unwrap() {
        let path = entry.unwrap().path();
        let task = storage.parse_task_file(&path).unwrap();
        assert!(task.depends_on.is_empty());
        assert!(!task.use_orchestrator);
        assert_eq!(task.roster_index, 0);

        storage.save_task(&task).unwrap();
        let written = storage
            .get_all_tasks()
            .unwrap()
            .into_iter()
            .find(|written| written.id == task.id)
            .expect("saved task");
        assert_eq!(written, task, "round-trip mismatch for {path:?}");

        let target = dir
            .path()
            .join(".kanban/tasks")
            .join(task.status.as_str())
            .join(format!("{}.md", task.id));
        let text = fs::read_to_string(&target).unwrap();
        let frontmatter = text.split("---").nth(1).expect("frontmatter");
        for key in [
            "depends_on",
            "needs:",
            "parent_task",
            "role_profile",
            "roster_index",
            "use_orchestrator",
            "orchestrated",
        ] {
            assert!(
                !frontmatter.contains(key),
                "{key} must stay out of a legacy task's frontmatter:\n{frontmatter}"
            );
        }
    }
}
