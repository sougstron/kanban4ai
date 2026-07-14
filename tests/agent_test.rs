mod common;

use std::fs;

use kanban4ai::agent::{build_agent_prompt, build_launch_plan, parse_opencode_agent_list};
use kanban4ai::core::models::Task;
use kanban4ai::core::storage::{NewTask, Storage};

#[test]
fn opencode_launch_plan_uses_task_overrides_and_prompt_contract() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    write_agent_config(
        dir.path(),
        r#"auto_launch:
  enabled: true
  use_tmux: false
  terminal_fallback: true
  auto_complete_on_exit: false
  default_agent: opencode
notifications:
  enabled: false
agents:
  opencode:
    command: /bin/echo
    model: default-model
    agent: default-agent
    extra_args:
    - --debug
"#,
    );
    let task = storage
        .create_task(NewTask {
            title: "Implement launcher".into(),
            description: "Wire phase 3".into(),
            ai_model: Some("task-model".into()),
            agent_backend: Some("opencode".into()),
            agent_name: Some("hephaestus".into()),
            interactive: true,
            ..Default::default()
        })
        .unwrap();

    let plan = build_launch_plan(dir.path(), &task, "ses-opencode-test", false).unwrap();

    assert_eq!(plan.backend, "opencode");
    assert_eq!(plan.command, "/bin/echo");
    assert!(
        plan.args
            .starts_with(&["run".to_string(), "--debug".to_string()])
    );
    assert!(has_arg_pair(&plan.args, "--model", "task-model"));
    assert!(has_arg_pair(&plan.args, "--agent", "hephaestus"));
    assert!(has_arg_pair(
        &plan.args,
        "--title",
        "TASK-001: Implement launcher"
    ));
    assert_eq!(plan.args.last().unwrap(), &plan.prompt);
    assert!(
        plan.prompt
            .contains("KANBAN_SESSION is set to ses-opencode-test")
    );
    assert!(plan.prompt.contains(".kanban/backups/TASK-001/"));
    assert!(
        plan.prompt
            .contains("kanban done TASK-001 --session ses-opencode-test --agent")
    );
    assert!(
        plan.prompt
            .contains("kanban ask TASK-001 <question> --agent --wait --session ses-opencode-test")
    );
}

#[test]
fn claude_launch_plan_uses_print_and_default_model() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    write_agent_config(
        dir.path(),
        r#"auto_launch:
  enabled: true
  use_tmux: false
  terminal_fallback: true
  default_agent: claude
notifications:
  enabled: false
agents:
  claude:
    command: claude
    model: sonnet
    extra_args:
    - --dangerously-skip-permissions
"#,
    );
    let task = storage
        .create_task(NewTask {
            title: "Claude task".into(),
            agent_name: Some("ignored-persona".into()),
            ..Default::default()
        })
        .unwrap();

    let plan = build_launch_plan(dir.path(), &task, "ses-claude-test", false).unwrap();

    assert_eq!(plan.backend, "claude");
    assert_eq!(plan.command, "claude");
    assert_eq!(plan.args[0], "--print");
    assert!(has_arg_pair(&plan.args, "--model", "sonnet"));
    assert!(
        plan.args
            .contains(&"--dangerously-skip-permissions".to_string())
    );
    assert!(!plan.args.contains(&"--title".to_string()));
    assert!(!plan.args.contains(&"--agent".to_string()));
    assert!(!plan.args.contains(&"ignored-persona".to_string()));
}

#[test]
fn revert_prompt_is_restrictive() {
    let task = Task::new("TASK-777", "Undo bad edit");
    let prompt = build_agent_prompt(std::path::Path::new("/repo"), &task, "ses-revert", true);

    assert!(prompt.contains("revert agent"));
    assert!(prompt.contains("Restore every file from .kanban/backups/TASK-777/"));
    assert!(prompt.contains("Do not make unrelated edits"));
    assert!(prompt.contains("kanban done TASK-777 --session ses-revert --agent"));
}

#[test]
fn opencode_agent_list_parser_prefers_matching_token() {
    let list = "- oh-my-openagent:hephaestus Senior worker\n- prometheus planner";
    assert_eq!(
        parse_opencode_agent_list(list, "hephaestus"),
        Some("oh-my-openagent:hephaestus".to_string())
    );
}

fn write_agent_config(project: &std::path::Path, body: &str) {
    let config = format!(
        "columns:\n- name: To Do\n  id: todo\n- name: In Progress\n  id: in_progress\n- name: Review\n  id: review\n- name: Done\n  id: done\nrules:\n  one_task_per_instance: true\n  user_only_review_to_done: true\n  auto_move_on_assign: true\n  auto_move_on_complete: true\n  questions_go_to_review: false\n  auto_launch_on_delegate: true\n  auto_launch_chained: true\nthresholds:\n  context_embed_max_size: 5120\n  context_warning: 51200\n  context_auto_compact: 102400\n  context_summary_max_length: 5000\n  session_heartbeat_timeout: 300\n  question_poll_interval: 0\n  question_wait_timeout: 0\n{body}"
    );
    fs::write(project.join(".kanban/config.yaml"), config).unwrap();
}

fn has_arg_pair(args: &[String], key: &str, value: &str) -> bool {
    args.windows(2)
        .any(|pair| pair[0] == key && pair[1] == value)
}
