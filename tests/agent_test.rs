mod common;

use std::fs;

use kanban4ai::agent::{
    build_agent_prompt, build_launch_plan, cached_opencode_catalog, parse_opencode_agent_list,
    parse_opencode_models_verbose, recent_models, record_recent_model, sort_efforts,
    sort_opencode_models,
};
use kanban4ai::core::models::{MessageKind, MessageRole, Task};
use kanban4ai::core::storage::{NewTask, Storage};
use kanban4ai::core::thread::ThreadManager;

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
    // The requested name stays in args; the registered form is resolved at
    // run time inside the wrapper script (see LaunchPlan::resolve_agent).
    assert!(has_arg_pair(&plan.args, "--agent", "hephaestus"));
    assert_eq!(plan.resolve_agent.as_deref(), Some("hephaestus"));
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
    assert!(plan.prompt.contains("KANBAN_CMD is set"));
    assert!(
        plan.prompt
            .contains("\"$KANBAN_CMD\" done TASK-001 --session ses-opencode-test --agent")
    );
    assert!(plan.prompt.contains(
        "\"$KANBAN_CMD\" ask TASK-001 <question> --agent --wait --session ses-opencode-test"
    ));
    assert!(plan.prompt.contains(
        "\"$KANBAN_CMD\" detach TASK-001 --session ses-opencode-test --eta <expected-seconds> \
         --note <what you wait for> -- <command> [args...]"
    ));
    assert!(plan.prompt.contains(
        "\"$KANBAN_CMD\" waiting TASK-001 --session ses-opencode-test --eta <expected-seconds> \
         --note <what you wait for>"
    ));
    assert!(
        plan.prompt
            .contains("never start it as a plain shell background job")
    );
}

#[test]
fn launch_prompt_includes_thread_review_feedback() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    write_agent_config(
        dir.path(),
        r#"auto_launch:
  enabled: true
  use_tmux: false
  terminal_fallback: true
  default_agent: opencode
notifications:
  enabled: false
agents:
  opencode:
    command: /bin/echo
"#,
    );
    let task = storage
        .create_task(NewTask {
            title: "Review task".into(),
            description: "Initial implementation".into(),
            ..Default::default()
        })
        .unwrap();
    let thread = ThreadManager::new(dir.path()).unwrap();
    thread
        .post(
            &task.id,
            MessageRole::Human,
            MessageKind::ReviewEdit,
            "Return Escape for closing the task detail",
            None,
            vec![],
            Some("user".to_string()),
        )
        .unwrap();

    let plan = build_launch_plan(dir.path(), &task, "ses-opencode-rerun", false).unwrap();

    assert!(plan.prompt.contains("Thread context and review feedback:"));
    assert!(plan.prompt.contains("[human review_edit"));
    assert!(
        plan.prompt
            .contains("Return Escape for closing the task detail")
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
fn opencode_launch_plan_passes_task_effort_as_variant() {
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
    model: openai/gpt-5.5
"#,
    );
    let task = storage
        .create_task(NewTask {
            title: "Effort task".into(),
            ai_effort: Some("xhigh".into()),
            ..Default::default()
        })
        .unwrap();

    let plan = build_launch_plan(dir.path(), &task, "ses-variant-test", false).unwrap();

    assert_eq!(plan.model.as_deref(), Some("openai/gpt-5.5"));
    assert!(has_arg_pair(&plan.args, "--variant", "xhigh"));
    assert!(!plan.args.contains(&"--effort".to_string()));
}

#[test]
fn claude_launch_plan_passes_effort_with_config_fallback() {
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
    model: fable
    effort: medium
"#,
    );
    let defaulted = storage
        .create_task(NewTask::titled("Config effort"))
        .unwrap();
    let plan = build_launch_plan(dir.path(), &defaulted, "ses-effort-default", false).unwrap();
    assert!(has_arg_pair(&plan.args, "--model", "fable"));
    assert!(has_arg_pair(&plan.args, "--effort", "medium"));
    assert!(!plan.args.contains(&"--variant".to_string()));

    let overridden = storage
        .create_task(NewTask {
            title: "Task effort".into(),
            ai_effort: Some("max".into()),
            ..Default::default()
        })
        .unwrap();
    let plan = build_launch_plan(dir.path(), &overridden, "ses-effort-task", false).unwrap();
    assert!(has_arg_pair(&plan.args, "--effort", "max"));
}

#[test]
fn opencode_models_sort_default_then_recent_then_alphabetical() {
    let models: Vec<String> = ["b/two", "a/one", "c/three", "d/four", "e/five", "f/six"]
        .into_iter()
        .map(String::from)
        .collect();
    // More than three recents: only the first three known non-default ones
    // are promoted; unknown and default entries are skipped.
    let recent: Vec<String> = [
        "gone/model",
        "d/four",
        "b/two",
        "c/three",
        "e/five",
        "a/one",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        sort_opencode_models(&models, Some("b/two"), &recent),
        ["b/two", "d/four", "c/three", "e/five", "a/one", "f/six"]
    );
    assert_eq!(
        sort_opencode_models(&models, None, &[]),
        ["a/one", "b/two", "c/three", "d/four", "e/five", "f/six"]
    );
}

#[test]
fn efforts_sort_weakest_to_strongest() {
    let unsorted = ["xhigh", "max", "none", "high", "low", "medium", "custom"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(
        sort_efforts(unsorted),
        ["none", "low", "medium", "high", "xhigh", "max", "custom"]
    );
}

#[test]
fn opencode_verbose_model_listing_yields_models_and_variants() {
    let text = r#"openai/gpt-5.5
{
  "id": "gpt-5.5",
  "variants": {
    "high": {
      "reasoningEffort": "high"
    },
    "low": {
      "reasoningEffort": "low"
    }
  }
}
opencode/plain
{
  "id": "plain",
  "variants": {}
}
"#;
    let catalog = parse_opencode_models_verbose(text);
    assert_eq!(catalog.models, ["openai/gpt-5.5", "opencode/plain"]);
    assert_eq!(catalog.variants_for("openai/gpt-5.5"), ["low", "high"]);
    assert!(catalog.variants_for("opencode/plain").is_empty());
    assert!(catalog.variants_for("unknown/model").is_empty());
}

#[test]
fn cached_opencode_catalog_is_non_blocking_on_cold_cache() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let command = format!("/definitely/missing/opencode-{unique}");
    let started = std::time::Instant::now();

    assert!(cached_opencode_catalog(&command).is_none());

    assert!(started.elapsed() < std::time::Duration::from_millis(50));
}

#[test]
fn recent_models_history_moves_latest_first_and_dedupes() {
    let dir = tempfile::tempdir().unwrap();
    Storage::new(dir.path()).init_board().unwrap();
    assert!(recent_models(dir.path()).is_empty());

    record_recent_model(dir.path(), "a/one");
    record_recent_model(dir.path(), "b/two");
    record_recent_model(dir.path(), "a/one");
    assert_eq!(recent_models(dir.path()), ["a/one", "b/two"]);
}

#[test]
fn revert_prompt_is_restrictive() {
    let task = Task::new("TASK-777", "Undo bad edit");
    let prompt =
        build_agent_prompt(std::path::Path::new("/repo"), &task, "ses-revert", true).unwrap();

    assert!(prompt.contains("revert agent"));
    assert!(prompt.contains("Restore every file from .kanban/backups/TASK-777/"));
    assert!(prompt.contains("Do not make unrelated edits"));
    assert!(prompt.contains("KANBAN_CMD is set"));
    assert!(prompt.contains("\"$KANBAN_CMD\" done TASK-777 --session ses-revert --agent"));
}

#[test]
fn opencode_agent_list_parser_prefers_matching_token() {
    let list = "- oh-my-openagent:hephaestus Senior worker\n- prometheus planner";
    assert_eq!(
        parse_opencode_agent_list(list, "hephaestus"),
        Some("oh-my-openagent:hephaestus".to_string())
    );
}

/// Real `opencode agent list` lines are `<registered name> (<mode>)` where the
/// name may contain spaces and zero-width ordering characters; `--agent` needs
/// the full name verbatim, only without the mode marker.
#[test]
fn opencode_agent_list_parser_keeps_full_name_before_mode_marker() {
    let list = "build (subagent)\n\u{200B}\u{200B}Hephaestus - Deep Agent (primary)\n\u{200B}Sisyphus - Ultraworker (primary)";
    assert_eq!(
        parse_opencode_agent_list(list, "hephaestus"),
        Some("\u{200B}\u{200B}Hephaestus - Deep Agent".to_string())
    );
    assert_eq!(
        parse_opencode_agent_list(list, "sisyphus"),
        Some("\u{200B}Sisyphus - Ultraworker".to_string())
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
