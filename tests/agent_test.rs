mod common;

use std::fs;

use kanban4ai::agent::{
    build_agent_prompt, build_launch_plan, cached_opencode_catalog, load_pi_catalog,
    load_pi_catalog_from_dir, parse_omp_models_json, parse_opencode_agent_list,
    parse_opencode_models_verbose, parse_pi_builtin_catalog, parse_pi_models_json,
    parse_pi_models_store, pi_builtin_data_dir, recent_models, record_recent_model, sort_efforts,
    sort_opencode_models,
};
use kanban4ai::core::models::{MessageKind, MessageRole, Role, RunPhase, Task};
use kanban4ai::core::project::Roots;
use kanban4ai::core::provenance::{self, InputManifest};
use kanban4ai::core::session::SessionManager;
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
    // `run --format json` enables the machine transcript, then configured
    // extra_args (`--debug`) follow.
    assert!(plan.args.starts_with(&[
        "run".to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--debug".to_string(),
    ]));
    assert!(has_arg_pair(&plan.args, "--format", "json"));
    assert!(plan.transcript_file.is_some());
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
    assert!(
        !plan.args.iter().any(|arg| arg == &plan.prompt),
        "prompt body must not be placed on argv"
    );
    assert_eq!(
        plan.prompt_file.as_ref(),
        Some(&plan.log_file.with_file_name("ses-opencode-test.prompt.txt"))
    );
    assert_eq!(
        std::fs::read_to_string(plan.prompt_file.as_ref().unwrap()).unwrap(),
        plan.prompt
    );
    assert!(
        plan.prompt
            .contains("KANBAN_SESSION is set to ses-opencode-test")
    );
    assert!(plan.prompt.contains(&format!(
        "copy it to {}/",
        dir.path()
            .canonicalize()
            .unwrap()
            .join(".kanban/backups/TASK-001")
            .display()
    )));
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

/// With the board in the store, board files (log, transcript, prompt paths)
/// hang off the data root while the agent is told to work in the code folder.
/// Every `.kanban` path the agent is handed must be absolute — a relative one
/// would resolve against its cwd and write into the user's repo.
#[test]
fn launch_plan_splits_board_files_from_the_agent_work_folder() {
    let data_root = tempfile::tempdir().unwrap();
    let work_path = tempfile::tempdir().unwrap();
    let storage = Storage::new(data_root.path());
    storage.init_board().unwrap();
    write_agent_config(
        data_root.path(),
        r#"auto_launch:
  enabled: true
  use_tmux: false
notifications:
  enabled: false
agents:
  opencode:
    command: /bin/echo
"#,
    );
    let task = storage.create_task(NewTask::titled("Split roots")).unwrap();

    let roots = Roots::new(data_root.path(), work_path.path(), Some("split"));
    let plan = build_launch_plan(roots, &task, "ses-split", false).unwrap();

    let board = data_root.path().canonicalize().unwrap().join(".kanban");
    assert!(plan.log_file.starts_with(data_root.path().join(".kanban")));
    assert!(
        plan.transcript_file
            .as_ref()
            .is_some_and(|file| file.starts_with(data_root.path().join(".kanban")))
    );
    assert!(plan.prompt.contains(&format!(
        "working in project: {}",
        work_path.path().display()
    )));
    assert!(plan.prompt.contains(&format!(
        "copy it to {}/",
        board.join("backups").join(&task.id).display()
    )));
    assert!(
        plan.prompt.contains(
            &board
                .join("detached")
                .join("<task>-<stamp>.log")
                .display()
                .to_string()
        )
    );
    // The prompt-contract guard: no bare `.kanban/…` instruction survives.
    for line in plan.prompt.lines().filter(|line| line.contains(".kanban")) {
        assert!(
            line.contains(&board.display().to_string()),
            "prompt line points at a relative board path: {line}"
        );
    }
}

#[test]
fn prompt_nudges_suggestions_and_ask_form_for_plain_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    // A plain (non-interactive) task: the guidance must be present anyway.
    let task = storage
        .create_task(NewTask {
            title: "Plain task".into(),
            description: "Do the thing".into(),
            ..Default::default()
        })
        .unwrap();

    let prompt = build_agent_prompt(dir.path(), &task, "ses-plain", false, Role::Executor).unwrap();

    assert!(prompt.contains("\"$KANBAN_CMD\" suggest TASK-001 <idea>"));
    // Board paths are absolute: the agent's cwd is the code folder.
    let form_file = dir
        .path()
        .canonicalize()
        .unwrap()
        .join(".kanban/forms/TASK-001.ask.yaml");
    assert!(prompt.contains(&format!(
        "\"$KANBAN_CMD\" ask-form TASK-001 --file {} --agent --session ses-plain",
        form_file.display()
    )));
    assert!(prompt.contains(&format!("Write {} then submit it", form_file.display())));
    assert!(prompt.contains("Schema and examples: \"$KANBAN_CMD\" ask-form --help."));
    assert!(!prompt.contains("- prompt: <question text>"));
    assert!(prompt.contains("Role: executor"), "{prompt}");
    assert!(prompt.contains("Never move a task to Done"));
    assert!(prompt.contains("lands the task in Review or starts bot review"));
}

#[test]
fn designer_prompt_plans_and_forbids_implementation() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    let mut task = storage
        .create_task(NewTask {
            title: "Add login".into(),
            description: "Users should sign in with OAuth".into(),
            ..Default::default()
        })
        .unwrap();
    task.run_phase = Some(RunPhase::Design);
    storage.save_task(&task).unwrap();

    let prompt =
        build_agent_prompt(dir.path(), &task, "ses-design", false, Role::Designer).unwrap();

    assert!(prompt.contains("DESIGNER"), "{prompt}");
    assert!(prompt.contains("Role: designer"), "{prompt}");
    assert!(prompt.contains("plan, not to implement"), "{prompt}");
    assert!(prompt.contains("Do not implement the task"));
    assert!(prompt.contains("Do not move the task between columns"));
    assert!(prompt.contains("Do not move this task out of In Progress"));
    assert!(prompt.contains("Finish the design phase only"));
    assert!(prompt.contains("\"$KANBAN_CMD\" context TASK-001 <text> --source agent"));
    assert!(prompt.contains("\"$KANBAN_CMD\" done TASK-001 --session ses-design --agent"));
    assert!(prompt.contains("Users should sign in with OAuth"));
    assert!(
        !prompt.contains("Before editing an existing file"),
        "designer must not be told to edit files:\n{prompt}"
    );
}

#[test]
fn reviewer_prompt_requires_verdict_and_forbids_done() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    let mut task = storage
        .create_task(NewTask {
            title: "Add login".into(),
            description: "Users should sign in with OAuth".into(),
            ..Default::default()
        })
        .unwrap();
    task.run_phase = Some(RunPhase::Review);
    storage.save_task(&task).unwrap();

    let prompt =
        build_agent_prompt(dir.path(), &task, "ses-review", false, Role::Reviewer).unwrap();

    assert!(prompt.contains("REVIEWER"), "{prompt}");
    assert!(prompt.contains("Role: reviewer"), "{prompt}");
    assert!(prompt.contains("AGENTS.md"));
    assert!(prompt.contains("CLAUDE.md"));
    assert!(prompt.contains("thread"));
    assert!(
        prompt.contains("\"$KANBAN_CMD\" verdict TASK-001 --approve --session ses-review --agent")
    );
    assert!(prompt.contains("\"$KANBAN_CMD\" verdict TASK-001 --changes"));
    assert!(prompt.contains("Never call done"));
    assert!(prompt.contains("Do not implement fixes"));
    assert!(prompt.contains("Your only exit is kanban verdict"));
    assert!(prompt.contains("Users should sign in with OAuth"));
    assert!(
        !prompt.contains("When implementation and verification are complete"),
        "reviewer must not be given the executor done contract:\n{prompt}"
    );
}

#[test]
fn every_role_can_ask_for_clarification_and_interactive_tasks_can_wait() {
    // Given an interactive task launched under every role contract.
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    let task = storage
        .create_task(NewTask {
            title: "Clarify requirements".into(),
            interactive: true,
            ..Default::default()
        })
        .unwrap();

    // When each role prompt is built.
    for role in [Role::Executor, Role::Designer, Role::Reviewer] {
        let prompt = build_agent_prompt(dir.path(), &task, "ses-questions", false, role).unwrap();

        // Then both structured questions and blocking interactive waits are available.
        assert!(
            prompt.contains("\"$KANBAN_CMD\" ask-form TASK-001"),
            "missing ask-form for {role:?}"
        );
        assert!(
            prompt.contains(
                "\"$KANBAN_CMD\" ask TASK-001 <question> --agent --wait --session ses-questions"
            ),
            "missing interactive wait for {role:?}"
        );
    }
}

#[test]
fn prompt_follows_role_not_run_phase() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    let task = storage
        .create_task(NewTask {
            title: "No phase".into(),
            description: "Body".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(task.run_phase, None);

    let designer = build_agent_prompt(dir.path(), &task, "ses-r", false, Role::Designer).unwrap();
    assert!(designer.contains("Role: designer"), "{designer}");
    assert!(!designer.contains("Role: executor"));

    let reviewer = build_agent_prompt(dir.path(), &task, "ses-r", false, Role::Reviewer).unwrap();
    assert!(reviewer.contains("Role: reviewer"), "{reviewer}");
    assert!(reviewer.contains("Your only exit is kanban verdict"));
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
fn omp_launch_plan_uses_print_and_thinking_effort() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    write_agent_config(
        dir.path(),
        r#"auto_launch:
  enabled: true
  use_tmux: false
  terminal_fallback: true
  default_agent: omp
notifications:
  enabled: false
agents:
  omp:
    command: omp
    model: null
"#,
    );
    let task = storage
        .create_task(NewTask {
            title: "Omp task".into(),
            ai_model: Some("openai-codex/gpt-5.6-sol".into()),
            ai_effort: Some("high".into()),
            agent_name: Some("ignored-persona".into()),
            ..Default::default()
        })
        .unwrap();

    let plan = build_launch_plan(dir.path(), &task, "ses-omp-test", false).unwrap();

    assert_eq!(plan.backend, "omp");
    assert_eq!(plan.command, "omp");
    // Non-interactive `-p`; prompt body is on disk, not the trailing argv.
    assert_eq!(plan.args[0], "-p");
    assert!(
        !plan.args.iter().any(|arg| arg == &plan.prompt),
        "prompt body must not be placed on argv"
    );
    assert_eq!(
        std::fs::read_to_string(plan.prompt_file.as_ref().unwrap()).unwrap(),
        plan.prompt
    );
    assert!(has_arg_pair(
        &plan.args,
        "--model",
        "openai-codex/gpt-5.6-sol"
    ));
    // Effort maps onto --thinking, not --effort/--variant.
    assert!(has_arg_pair(&plan.args, "--thinking", "high"));
    assert!(!plan.args.contains(&"--effort".to_string()));
    assert!(!plan.args.contains(&"--variant".to_string()));
    // omp has no launch-time persona, but `--mode json` gives it a parseable
    // transcript harvested exactly like claude/opencode.
    assert!(!plan.args.contains(&"--agent".to_string()));
    assert!(!plan.args.contains(&"ignored-persona".to_string()));
    assert!(!plan.args.contains(&"--title".to_string()));
    assert!(has_arg_pair(&plan.args, "--mode", "json"));
    assert!(plan.transcript_file.is_some());
    assert!(plan.resolve_agent.is_none());
}

#[test]
fn pi_family_auto_relaunch_resumes_native_conversation_with_delta_prompt() {
    for backend in ["pi", "omp"] {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path());
        storage.init_board().unwrap();
        write_agent_config(
            dir.path(),
            &format!(
                "auto_launch:\n  enabled: true\n  default_agent: {backend}\nnotifications:\n  enabled: false\nagents:\n  {backend}:\n    command: {backend}\n"
            ),
        );
        let mut task = storage.create_task(NewTask::titled("Resume me")).unwrap();
        task.agent_backend = Some(backend.to_string());
        task.auto_resumes = 1;
        storage.save_task(&task).unwrap();

        let sessions = SessionManager::new(dir.path());
        let mut previous = sessions
            .link_named_session(&task.id, "ses-previous", "old")
            .unwrap();
        previous.status = kanban4ai::core::models::SessionStatus::Closed;
        previous.ended_at = Some(previous.last_seen);
        sessions.save_session(&previous).unwrap();
        provenance::write_manifest(
            &storage.provenance_dir,
            &InputManifest {
                session_id: previous.id.clone(),
                backend: backend.to_string(),
                backend_session_id: Some("native-conversation-id".to_string()),
                ..InputManifest::default()
            },
        )
        .unwrap();
        post_with_test_origin(
            dir.path(),
            &task.id,
            "agent",
            "already known by native history",
            "agent:ses-previous",
        );
        ThreadManager::new(dir.path())
            .unwrap()
            .post(
                &task.id,
                MessageRole::Human,
                MessageKind::ReviewEdit,
                "new human feedback",
                None,
                vec![],
                Some("user".to_string()),
            )
            .unwrap();

        let plan = build_launch_plan(dir.path(), &task, "ses-current", false).unwrap();
        let resume_flag = if backend == "pi" {
            "--session"
        } else {
            "--resume"
        };
        assert!(has_arg_pair(
            &plan.args,
            resume_flag,
            "native-conversation-id"
        ));
        assert_eq!(
            plan.resumed_backend_session.as_deref(),
            Some("native-conversation-id")
        );
        assert!(
            plan.prompt.contains("new human feedback"),
            "{}",
            plan.prompt
        );
        assert!(!plan.prompt.contains("already known by native history"));
        assert!(!plan.prompt.contains("Before editing an existing file"));
        assert!(plan.prompt.contains("KANBAN_SESSION=ses-current"));
    }
}

#[test]
fn full_prompt_does_not_replay_agent_reply_when_same_run_posted_context() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    let task = storage
        .create_task(NewTask::titled("No duplicate"))
        .unwrap();
    for (author, body) in [
        ("agent", "concise progress"),
        ("agent-reply", "whole noisy run"),
    ] {
        post_with_test_origin(dir.path(), &task.id, author, body, "agent:ses-old");
    }
    let prompt = build_agent_prompt(dir.path(), &task, "ses-new", false, Role::Executor).unwrap();
    assert!(prompt.contains("concise progress"));
    assert!(!prompt.contains("whole noisy run"));
}

#[test]
fn pi_launch_plan_uses_print_and_thinking_effort() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    write_agent_config(
        dir.path(),
        r#"auto_launch:
  enabled: true
  use_tmux: false
  terminal_fallback: true
  default_agent: pi
notifications:
  enabled: false
agents:
  pi:
    command: pi
    model: null
    effort: minimal
"#,
    );
    let task = storage.create_task(NewTask::titled("Pi task")).unwrap();

    let plan = build_launch_plan(dir.path(), &task, "ses-pi-test", false).unwrap();

    assert_eq!(plan.backend, "pi");
    assert_eq!(plan.args[0], "-p");
    // Backend effort default flows through as --thinking.
    assert!(has_arg_pair(&plan.args, "--thinking", "minimal"));
    // `--mode json` yields a parseable transcript for telemetry/provenance.
    assert!(has_arg_pair(&plan.args, "--mode", "json"));
    assert!(plan.transcript_file.is_some());
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
fn omp_models_json_yields_models_and_thinking_efforts() {
    let text = r#"{"models":[
        {"provider":"openai-codex","id":"gpt-5.6-sol","selector":"openai-codex/gpt-5.6-sol",
         "thinking":["high","low","max","medium"]},
        {"provider":"xai-oauth","id":"grok-build","selector":"xai-oauth/grok-build",
         "thinking":null}
    ]}"#;
    let catalog = parse_omp_models_json(text);
    assert_eq!(
        catalog.models,
        ["openai-codex/gpt-5.6-sol", "xai-oauth/grok-build"]
    );
    // Efforts are normalized weakest-to-strongest.
    assert_eq!(
        catalog.variants_for("openai-codex/gpt-5.6-sol"),
        ["low", "medium", "high", "max"]
    );
    assert!(catalog.variants_for("xai-oauth/grok-build").is_empty());
}

#[test]
fn omp_models_json_ignores_invalid_json() {
    assert!(parse_omp_models_json("not json").models.is_empty());
}

#[test]
fn pi_models_store_yields_provider_slash_id_selectors_and_efforts() {
    let text = r#"{
      "anthropic": {
        "models": [
          {"id":"claude-fable-5","provider":"anthropic",
           "thinkingLevelMap":{"off":null,"max":"max","xhigh":"xhigh"}},
          {"id":"claude-haiku-4-5","provider":"anthropic"}
        ]
      },
      "xai": {
        "models": [
          {"id":"grok-4.5","provider":"xai","thinking":["high","low"]}
        ]
      }
    }"#;
    let catalog = parse_pi_models_store(text);
    assert!(
        catalog
            .models
            .contains(&"anthropic/claude-fable-5".to_string())
    );
    assert!(
        catalog
            .models
            .contains(&"anthropic/claude-haiku-4-5".to_string())
    );
    assert!(catalog.models.contains(&"xai/grok-4.5".to_string()));
    // thinkingLevelMap keys become the model's efforts, weakest-to-strongest.
    assert_eq!(
        catalog.variants_for("anthropic/claude-fable-5"),
        ["off", "xhigh", "max"]
    );
    // No thinking metadata -> no efforts.
    assert!(
        catalog
            .variants_for("anthropic/claude-haiku-4-5")
            .is_empty()
    );
    // Falls back to a plain `thinking` array when present.
    assert_eq!(catalog.variants_for("xai/grok-4.5"), ["low", "high"]);
}

#[test]
fn pi_models_json_yields_custom_provider_selectors() {
    let text = r#"{
      "providers": {
        "Yolo-Auto": {
          "baseUrl": "https://example.test/v1",
          "api": "openai-completions",
          "models": [
            {"id": "qwen3.8-27b", "name": "qwen3.8-27b", "reasoning": true}
          ]
        }
      }
    }"#;
    let catalog = parse_pi_models_json(text);
    assert!(
        catalog
            .models
            .contains(&"Yolo-Auto/qwen3.8-27b".to_string())
    );
    assert!(catalog.variants_for("Yolo-Auto/qwen3.8-27b").is_empty());
}

#[test]
fn pi_models_json_ignores_missing_providers_and_invalid_json() {
    assert!(parse_pi_models_json("{}").models.is_empty());
    assert!(parse_pi_models_json("not json").models.is_empty());
}

#[test]
fn pi_catalog_merges_store_with_custom_models_json() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("models-store.json"),
        r#"{
          "xai": {
            "models": [
              {"id":"grok-4.5","provider":"xai","thinking":["high","low"]}
            ]
          }
        }"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("models.json"),
        r#"{
          "providers": {
            "Yolo-Auto": {
              "models": [{"id": "qwen3.8-27b"}]
            }
          }
        }"#,
    )
    .unwrap();

    let catalog = load_pi_catalog_from_dir(dir.path());
    assert!(catalog.models.contains(&"xai/grok-4.5".to_string()));
    assert!(
        catalog
            .models
            .contains(&"Yolo-Auto/qwen3.8-27b".to_string())
    );
    assert_eq!(catalog.variants_for("xai/grok-4.5"), ["low", "high"]);
}

#[test]
fn pi_catalog_from_models_json_alone_includes_custom_providers() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("models.json"),
        r#"{
          "providers": {
            "Yolo-Auto": {
              "models": [{"id": "qwen3.8-27b"}]
            }
          }
        }"#,
    )
    .unwrap();

    let catalog = load_pi_catalog_from_dir(dir.path());
    assert_eq!(catalog.models, vec!["Yolo-Auto/qwen3.8-27b".to_string()]);
}

#[test]
fn pi_catalog_store_thinking_map_wins_on_duplicate_selector() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("models-store.json"),
        r#"{
          "Yolo-Auto": {
            "models": [
              {"id":"qwen3.8-27b","thinkingLevelMap":{"low":null,"high":null}}
            ]
          }
        }"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("models.json"),
        r#"{
          "providers": {
            "Yolo-Auto": {
              "models": [{"id": "qwen3.8-27b"}]
            }
          }
        }"#,
    )
    .unwrap();

    let catalog = load_pi_catalog_from_dir(dir.path());
    assert_eq!(catalog.models, vec!["Yolo-Auto/qwen3.8-27b".to_string()]);
    assert_eq!(
        catalog.variants_for("Yolo-Auto/qwen3.8-27b"),
        ["low", "high"]
    );
}

#[test]
fn pi_builtin_catalog_yields_grouped_provider_slash_id_selectors() {
    let text = r#"{
      "openai-completions": {
        "anthropic/claude-sonnet-5": {
          "id": "anthropic/claude-sonnet-5",
          "provider": "openrouter",
          "thinkingLevelMap": {"off": null, "xhigh": "xhigh", "max": "max"}
        },
        "auto": {
          "id": "auto",
          "provider": "openrouter"
        }
      }
    }"#;
    let catalog = parse_pi_builtin_catalog(text);
    assert!(
        catalog
            .models
            .contains(&"openrouter/anthropic/claude-sonnet-5".to_string())
    );
    assert!(catalog.models.contains(&"openrouter/auto".to_string()));
    assert_eq!(
        catalog.variants_for("openrouter/anthropic/claude-sonnet-5"),
        ["off", "xhigh", "max"]
    );
}

#[test]
fn pi_builtin_catalog_ignores_invalid_json() {
    assert!(parse_pi_builtin_catalog("not json").models.is_empty());
    assert!(parse_pi_builtin_catalog("[]").models.is_empty());
}

#[test]
fn pi_catalog_merges_authenticated_builtin_openrouter() {
    let agent = tempfile::tempdir().unwrap();
    fs::write(
        agent.path().join("models-store.json"),
        r#"{"xai":{"models":[{"id":"grok-4.5","provider":"xai"}]}}"#,
    )
    .unwrap();
    fs::write(
        agent.path().join("auth.json"),
        r#"{"openrouter":{"type":"api_key","key":"sk-test"},"xai":{"type":"oauth"}}"#,
    )
    .unwrap();

    let data = tempfile::tempdir().unwrap();
    fs::write(
        data.path().join("openrouter.json"),
        r#"{
          "openai-completions": {
            "moonshotai/kimi-k2.6": {
              "id": "moonshotai/kimi-k2.6",
              "provider": "openrouter"
            }
          }
        }"#,
    )
    .unwrap();
    fs::write(
        data.path().join("google.json"),
        r#"{
          "google-generative-ai": {
            "gemini-3.1-pro": {"id": "gemini-3.1-pro", "provider": "google"}
          }
        }"#,
    )
    .unwrap();

    let catalog = load_pi_catalog(agent.path(), Some(data.path()));
    assert!(catalog.models.contains(&"xai/grok-4.5".to_string()));
    assert!(
        catalog
            .models
            .contains(&"openrouter/moonshotai/kimi-k2.6".to_string())
    );
    assert!(!catalog.models.iter().any(|m| m.starts_with("google/")));
}

#[test]
fn pi_catalog_skips_builtin_without_auth() {
    let agent = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    fs::write(
        data.path().join("openrouter.json"),
        r#"{
          "openai-completions": {
            "auto": {"id": "auto", "provider": "openrouter"}
          }
        }"#,
    )
    .unwrap();

    let catalog = load_pi_catalog(agent.path(), Some(data.path()));
    assert!(catalog.models.is_empty());
}

#[test]
fn pi_catalog_store_wins_over_builtin_duplicate() {
    let agent = tempfile::tempdir().unwrap();
    fs::write(
        agent.path().join("models-store.json"),
        r#"{
          "openrouter": {
            "models": [
              {"id":"auto","provider":"openrouter","thinking":["low","high"]}
            ]
          }
        }"#,
    )
    .unwrap();
    fs::write(
        agent.path().join("auth.json"),
        r#"{"openrouter":{"type":"api_key","key":"sk-test"}}"#,
    )
    .unwrap();
    let data = tempfile::tempdir().unwrap();
    fs::write(
        data.path().join("openrouter.json"),
        r#"{
          "openai-completions": {
            "auto": {"id": "auto", "provider": "openrouter"}
          }
        }"#,
    )
    .unwrap();

    let catalog = load_pi_catalog(agent.path(), Some(data.path()));
    assert_eq!(catalog.models, vec!["openrouter/auto".to_string()]);
    assert_eq!(catalog.variants_for("openrouter/auto"), ["low", "high"]);
}

#[test]
fn pi_builtin_data_dir_walks_from_pi_command() {
    let root = tempfile::tempdir().unwrap();
    let data = root
        .path()
        .join("node_modules")
        .join("@earendil-works")
        .join("pi-ai")
        .join("dist")
        .join("providers")
        .join("data");
    fs::create_dir_all(&data).unwrap();
    let command = root.path().join("dist").join("cli.js");
    fs::create_dir_all(command.parent().unwrap()).unwrap();
    fs::write(&command, "#!/usr/bin/env node\n").unwrap();

    let found = pi_builtin_data_dir(command.to_str().unwrap()).expect("catalog dir");
    assert_eq!(found, data);
}

#[test]
fn installed_pi_openrouter_catalog_parses_when_pi_is_on_path() {
    let Some(data) = pi_builtin_data_dir("pi") else {
        return;
    };
    let path = data.join("openrouter.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let parsed = parse_pi_builtin_catalog(&text);
    assert!(
        parsed
            .models
            .iter()
            .any(|model| model.starts_with("openrouter/")),
        "installed {} should yield openrouter selectors",
        path.display()
    );

    let agent = tempfile::tempdir().unwrap();
    fs::write(
        agent.path().join("auth.json"),
        r#"{"openrouter":{"type":"api_key","key":"x"}}"#,
    )
    .unwrap();
    let catalog = load_pi_catalog(agent.path(), Some(&data));
    assert!(
        catalog.models.len() > 100,
        "expected a full OpenRouter catalog, got {}",
        catalog.models.len()
    );
    assert!(
        catalog
            .models
            .iter()
            .any(|model| model == "openrouter/auto" || model.starts_with("openrouter/"))
    );
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
    let prompt = build_agent_prompt(
        std::path::Path::new("/repo"),
        &task,
        "ses-revert",
        true,
        Role::Executor,
    )
    .unwrap();

    assert!(prompt.contains("revert agent"));
    assert!(prompt.contains("Restore every file from /repo/.kanban/backups/TASK-777/"));
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

fn post_with_test_origin(
    root: &std::path::Path,
    task_id: &str,
    author: &str,
    body: &str,
    origin: &str,
) {
    let manager = ThreadManager::new(root).unwrap();
    let posted = manager
        .post(
            task_id,
            MessageRole::Agent,
            MessageKind::Context,
            body,
            None,
            vec![],
            Some(author.to_string()),
        )
        .unwrap();
    let mut thread = manager.load(task_id).unwrap();
    thread
        .messages
        .iter_mut()
        .find(|message| message.id == posted.id)
        .unwrap()
        .origin = Some(origin.to_string());
    manager.save(task_id, &mut thread).unwrap();
}

fn has_arg_pair(args: &[String], key: &str, value: &str) -> bool {
    args.windows(2)
        .any(|pair| pair[0] == key && pair[1] == value)
}
