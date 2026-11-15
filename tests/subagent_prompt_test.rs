//! Subagents v2, part 2 (TASK-595): the subagent's own short prompt, the
//! parent's controls, and the summary as the subagent's result.

mod common;

use kanban4ai::agent::{build_agent_prompt, build_resume_prompt};
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::models::{Role, SubCollected, SubState, Task};
use kanban4ai::core::operations::Operations;
use kanban4ai::core::storage::NewTask;
use kanban4ai::core::subagents::{SubSpawnRequest, SubWait};
use kanban4ai::core::thread::ThreadManager;
use std::fs;

/// Bytes a subagent's prompt may take without its task text.
const SUBAGENT_PROMPT_BUDGET: usize = 2048;

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

fn spawn_with(ops: &Operations, parent: &Task, request: SubSpawnRequest) -> Task {
    ops.sub_spawn(
        SubSpawnRequest {
            parent_id: parent.id.clone(),
            ..request
        },
        "ses-parent",
    )
    .unwrap()
}

fn spawn(ops: &Operations, parent: &Task, title: &str) -> Task {
    spawn_with(
        ops,
        parent,
        SubSpawnRequest {
            title: title.into(),
            description: "Find the cache bug".into(),
            ..Default::default()
        },
    )
}

fn load(ops: &Operations, id: &str) -> Task {
    ops.get_task(id).unwrap().unwrap()
}

fn set_project_instructions(ops: &Operations, text: &str) {
    let mut cfg = ops.config.load_raw().unwrap();
    cfg.instructions = text.into();
    ops.config.save(&cfg).unwrap();
}

#[test]
fn subagent_prompt_is_short_and_carries_no_board_rules() {
    let (dir, ops, _recorder) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let child = load(&ops, &spawn(&ops, &parent, "Research").id);
    let session = child.session.clone().unwrap();
    let prompt = build_agent_prompt(dir.path(), &child, &session, false, Role::Executor).unwrap();

    for banned in [
        "Column ownership",
        "spawn",
        "Review",
        "suggest",
        "detach",
        "waiting",
        "heartbeat",
        "result ",
        "backups",
    ] {
        assert!(!prompt.contains(banned), "{banned:?} in:\n{prompt}");
    }
    assert!(prompt.contains(&format!(
        "\"$KANBAN_CMD\" sub done {} --session {session} --summary",
        child.id
    )));
    assert!(prompt.contains(&parent.id));
    assert!(prompt.contains("foreground"));
    assert!(prompt.contains("Never ask"));
    let without_task = prompt.replace("Find the cache bug", "");
    assert!(
        without_task.len() <= SUBAGENT_PROMPT_BUDGET,
        "subagent prompt is {} bytes without its task:\n{without_task}",
        without_task.len()
    );

    // Readonly adds one line, still within budget.
    let readonly = spawn_with(
        &ops,
        &parent,
        SubSpawnRequest {
            title: "Look only".into(),
            readonly: true,
            ..Default::default()
        },
    );
    let readonly = load(&ops, &readonly.id);
    let prompt = build_agent_prompt(
        dir.path(),
        &readonly,
        readonly.session.as_deref().unwrap(),
        false,
        Role::Executor,
    )
    .unwrap();
    assert!(prompt.contains("Readonly (mandatory)"));
    assert!(prompt.len() <= SUBAGENT_PROMPT_BUDGET);
}

#[test]
fn subagent_instructions_reach_only_subagents_and_project_ones_never_do() {
    let (dir, ops, _recorder) = common::ops_with_recorder(true);
    set_project_instructions(&ops, "PROJECT-WIDE RULE");
    let instructions = dir.path().join(".kanban/instructions");
    fs::create_dir_all(&instructions).unwrap();
    fs::write(instructions.join("subagent.md"), "SUBAGENT-ONLY RULE\n").unwrap();
    fs::write(instructions.join("executor.md"), "EXECUTOR-ONLY RULE\n").unwrap();
    let parent = parent(&ops);
    let child = load(&ops, &spawn(&ops, &parent, "Research").id);

    let sub_prompt = build_agent_prompt(
        dir.path(),
        &child,
        child.session.as_deref().unwrap(),
        false,
        Role::Executor,
    )
    .unwrap();
    assert!(sub_prompt.contains("SUBAGENT-ONLY RULE"));
    assert!(!sub_prompt.contains("PROJECT-WIDE RULE"));
    assert!(!sub_prompt.contains("EXECUTOR-ONLY RULE"));

    let sub_resume = build_resume_prompt(
        dir.path(),
        &child,
        "ses-sub-2",
        child.session.as_deref().unwrap(),
        Role::Executor,
    )
    .unwrap();
    assert!(sub_resume.contains("SUBAGENT-ONLY RULE"));
    assert!(!sub_resume.contains("PROJECT-WIDE RULE"));
    assert!(!sub_resume.contains("suggest"));
    assert!(!sub_resume.contains("heartbeat"));

    let parent_prompt =
        build_agent_prompt(dir.path(), &parent, "ses-parent", false, Role::Executor).unwrap();
    assert!(!parent_prompt.contains("SUBAGENT-ONLY RULE"));
    assert!(parent_prompt.contains("PROJECT-WIDE RULE"));
    assert!(parent_prompt.contains("EXECUTOR-ONLY RULE"));
    // The parent's thread never reaches the subagent.
    ThreadManager::new(dir.path())
        .unwrap()
        .post_board_note(&parent.id, "PARENT-THREAD-SECRET")
        .unwrap();
    ContextManager::new(dir.path())
        .append_context(&parent.id, "PARENT-CONTEXT-SECRET", "agent", &ops.storage)
        .unwrap();
    let sub_prompt = build_agent_prompt(
        dir.path(),
        &child,
        child.session.as_deref().unwrap(),
        false,
        Role::Executor,
    )
    .unwrap();
    assert!(!sub_prompt.contains("PARENT-CONTEXT-SECRET"));
    assert!(!sub_prompt.contains("PARENT-THREAD-SECRET"));
}

#[test]
fn context_from_parent_is_the_only_handed_over_view_and_is_capped() {
    let (dir, ops, _recorder) = common::ops_with_recorder(true);
    let mut cfg = ops.config.load_raw().unwrap();
    cfg.orchestration.insert(
        "subagents".into(),
        serde_yaml_ng::from_str("context_budget_chars: 50").unwrap(),
    );
    ops.config.save(&cfg).unwrap();
    let parent = parent(&ops);
    let child = spawn_with(
        &ops,
        &parent,
        SubSpawnRequest {
            title: "With context".into(),
            context: Some(format!("### notes.md\nKEEP-ME {}", "x".repeat(200))),
            ..Default::default()
        },
    );
    let child = load(&ops, &child.id);
    let prompt = build_agent_prompt(
        dir.path(),
        &child,
        child.session.as_deref().unwrap(),
        false,
        Role::Executor,
    )
    .unwrap();
    let section = prompt
        .split("Context from parent:\n")
        .nth(1)
        .expect("context section");
    assert!(section.contains("KEEP-ME"));
    assert!(section.contains("cut at subagents.context_budget_chars = 50"));
    assert!(!section.contains(&"x".repeat(60)));
}

#[test]
fn parent_prompt_teaches_sub_wait_not_waiting_eta() {
    let (dir, ops, _recorder) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let prompt =
        build_agent_prompt(dir.path(), &parent, "ses-parent", false, Role::Executor).unwrap();
    let block = prompt
        .split("\nSubagents (")
        .nth(1)
        .expect("subagent block")
        .split("\n\n")
        .next()
        .unwrap();
    for needle in [
        "sub spawn",
        "sub wait",
        "sub output",
        "sub merge",
        "sub discard",
        "sub send",
    ] {
        assert!(block.contains(needle), "{needle} missing in:\n{block}");
    }
    assert!(block.contains("kanban plan"));
    // `waiting --eta` appears only as the prohibition.
    for line in block.lines().filter(|line| line.contains("waiting --eta")) {
        assert!(
            line.starts_with("Never put waiting --eta on subagents"),
            "{line}"
        );
    }
    assert!(block.contains("never run done for them"));
}

#[test]
fn summary_is_required_and_is_the_result_the_parent_reads() {
    let (dir, ops, _recorder) = common::ops_with_recorder(true);
    let parent = parent(&ops);
    let first = spawn(&ops, &parent, "First");
    let second = spawn(&ops, &parent, "Second");
    let session = load(&ops, &first.id).session.unwrap();

    // No summary from the agent: refused, also through plain `done`.
    assert!(ops.sub_done(&first.id, Some(&session), Some("  ")).is_err());
    assert!(ops.sub_done(&first.id, Some(&session), None).is_err());
    assert_eq!(load(&ops, &first.id).sub_state, Some(SubState::Running));

    assert!(matches!(
        ops.sub_wait(&parent.id, "ses-parent", &[]).unwrap(),
        SubWait::Waiting(_)
    ));
    let done = ops
        .sub_done(
            &first.id,
            Some(&session),
            Some("Found it in cache.rs\nmore detail"),
        )
        .unwrap();
    assert_eq!(
        done.sub_summary.as_deref(),
        Some("Found it in cache.rs\nmore detail")
    );
    let output = ops.task_output(&first.id).unwrap();
    assert_eq!(output.digest, "Found it in cache.rs\nmore detail");
    ops.sub_cancel(&second.id, "not needed").unwrap();

    // The parent's table: id, state, time, title, one summary line.
    let table = ops.subagent_table(&parent.id, false).unwrap();
    assert!(table.starts_with("| id | state | time | title | summary |"));
    assert!(table.contains(&format!("| {} | done |", first.id)));
    assert!(table.contains("| First | Found it in cache.rs |"));
    assert!(!table.contains("more detail"));
    assert!(table.contains(&format!("| {} | cancelled |", second.id)));
    assert!(table.contains("not needed"));

    // Collected subagents leave the parent's resume table; failed or
    // uncollected ones stay.
    ops.sub_discard(&first.id).unwrap();
    assert_eq!(
        load(&ops, &first.id).sub_collected,
        Some(SubCollected::Discarded)
    );
    let open = ops.subagent_table(&parent.id, true).unwrap();
    assert!(!open.contains(&first.id));
    assert!(open.contains(&second.id));
    let resume = build_resume_prompt(
        dir.path(),
        &parent,
        "ses-parent-2",
        "ses-parent",
        Role::Executor,
    )
    .unwrap();
    assert!(resume.contains("Your subagents still to collect"));
    assert!(resume.contains(&second.id));
    assert!(!resume.contains(&format!("| {} |", first.id)));
}
