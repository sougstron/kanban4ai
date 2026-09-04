//! Configuration compatibility behavior from the earlier implementation.

mod common;

use std::fs;

use kanban4ai::core::config::{
    Config, IsolationCleanup, IsolationLand, IsolationMode, IsolationOnConflict, IsolationSeed,
    OnChangesRequested, OrchestrationSettings,
};

fn write_config(dir: &tempfile::TempDir, content: &str) -> Config {
    let kanban = dir.path().join(".kanban");
    fs::create_dir_all(&kanban).unwrap();
    fs::write(kanban.join("config.yaml"), content).unwrap();
    Config::new(dir.path())
}

#[test]
fn init_writes_defaults_and_load_reads_them() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path());
    assert!(!config.exists());
    config.init().unwrap();
    assert!(config.exists());

    assert_eq!(
        config.get_threshold("context_embed_max_size").unwrap(),
        5120
    );
    assert_eq!(config.get_threshold("question_poll_interval").unwrap(), 3);
    assert_eq!(config.get_threshold("question_wait_timeout").unwrap(), 600);
    assert!(config.get_rule("user_only_review_to_done").unwrap());
    assert!(!config.get_rule("questions_go_to_review").unwrap());
    assert_eq!(
        config.get_column_names().unwrap(),
        vec!["To Do", "In Progress", "Review", "Done"]
    );
}

#[test]
fn default_opencode_agent_options_exclude_obsolete_hephaestus() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path());

    config.init().unwrap();
    let board = config.load().unwrap();
    let options = board
        .agents
        .get("opencode")
        .and_then(|value| value.get("agent_options"))
        .and_then(|value| value.as_sequence())
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(options, ["sisyphus", "prometheus", "atlas"]);
}

#[test]
fn load_creates_config_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path());
    config.load().unwrap();
    assert!(dir.path().join(".kanban/config.yaml").is_file());
}

#[test]
fn string_booleans_and_integers_are_coerced() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
rules:
  one_task_per_instance: "true"
  user_only_review_to_done: "no"
thresholds:
  context_warning: "42"
auto_launch:
  enabled: "yes"
notifications:
  enabled: "1"
  timeout: "9"
"#,
    );

    assert!(config.get_rule("one_task_per_instance").unwrap());
    assert!(!config.get_rule("user_only_review_to_done").unwrap());
    assert_eq!(config.get_threshold("context_warning").unwrap(), 42);

    let board = config.load().unwrap();
    assert_eq!(
        board.auto_launch.get("enabled").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        board.notifications.get("timeout").and_then(|v| v.as_i64()),
        Some(9)
    );
}

#[test]
fn invalid_boolean_rule_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\nrules:\n  one_task_per_instance: \"maybe\"\n",
    );
    assert!(config.load().is_err());
}

#[test]
fn invalid_integer_threshold_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\nthresholds:\n  context_warning: \"lots\"\n",
    );
    assert!(config.load().is_err());
}

#[test]
fn missing_sections_are_filled_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(&dir, "columns:\n- name: Only\n  id: only\n");

    // rules/thresholds/agents absent in the file, but defaults kick in
    assert!(config.get_rule("auto_move_on_assign").unwrap());
    assert_eq!(
        config.get_threshold("session_heartbeat_timeout").unwrap(),
        1800
    );
    assert_eq!(config.get_threshold("max_auto_resumes").unwrap(), 3);
    assert_eq!(config.get_threshold("waiting_min_eta").unwrap(), 10);
    assert_eq!(config.get_threshold("waiting_max_eta").unwrap(), 604800);
    assert_eq!(config.get_threshold("waiting_default_eta").unwrap(), 900);
    assert_eq!(config.get_threshold("waiting_eta_multiplier").unwrap(), 2);
    assert_eq!(
        config.get_threshold("waiting_note_max_chars").unwrap(),
        1000
    );

    let board = config.load().unwrap();
    assert!(board.agents.contains_key("opencode"));
    assert!(board.agents.contains_key("claude"));
    assert!(board.agents.contains_key("codex"));
    assert_eq!(board.column_ids(), vec!["only"]);
}

#[test]
fn partial_agent_backend_gets_default_keys_merged() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
agents:
  claude:
    model: opus
"#,
    );
    let board = config.load().unwrap();
    let claude = board.agents.get("claude").unwrap();
    // user override survives
    assert_eq!(claude.get("model").and_then(|v| v.as_str()), Some("opus"));
    // missing keys arrive from defaults
    assert_eq!(
        claude.get("command").and_then(|v| v.as_str()),
        Some("claude")
    );
    assert!(claude.get("extra_args").is_some());
}

#[test]
fn existing_agent_model_catalog_gets_new_defaults_merged() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
agents:
  claude:
    models:
    - sonnet
    - opus
    - haiku
    - custom-model
    extra_args: []
"#,
    );

    let board = config.load().unwrap();
    let claude = board.agents.get("claude").unwrap();
    let models = claude
        .get("models")
        .and_then(|value| value.as_sequence())
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        models,
        vec!["sonnet", "opus", "haiku", "custom-model", "fable"]
    );
    assert!(
        claude
            .get("extra_args")
            .and_then(|value| value.as_sequence())
            .unwrap()
            .is_empty(),
        "user-customized non-catalog sequences must not be merged"
    );
}

#[test]
fn custom_keys_survive_save_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
thresholds:
  my_custom_threshold: 777
"#,
    );
    let board = config.load().unwrap();
    config.save(&board).unwrap();

    let fresh = Config::new(dir.path());
    assert_eq!(fresh.get_threshold("my_custom_threshold").unwrap(), 777);
}

#[test]
fn unknown_top_level_section_survives_load_save_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
custom_integration:
  endpoint: https://example.test
  enabled: true
"#,
    );

    let board = config.load().unwrap();
    assert_eq!(
        board
            .extras
            .get("custom_integration")
            .and_then(|value| value.get("endpoint"))
            .and_then(|value| value.as_str()),
        Some("https://example.test")
    );
    config.save(&board).unwrap();

    let fresh = Config::new(dir.path());
    assert_eq!(
        fresh
            .load()
            .unwrap()
            .extras
            .get("custom_integration")
            .and_then(|value| value.get("enabled"))
            .and_then(|value| value.as_bool()),
        Some(true)
    );
}

#[test]
fn empty_columns_is_an_error_or_defaulted() {
    let dir = tempfile::tempdir().unwrap();
    // completely empty file falls back to full defaults
    let config = write_config(&dir, "");
    let board = config.load().unwrap();
    assert_eq!(board.column_ids().len(), 4);
}

#[test]
fn legacy_board_without_orchestration_gets_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\nthresholds:\n  context_warning: 42\n",
    );

    let orch = config.get_orchestration().unwrap();
    assert!(orch.queue_enabled);
    assert_eq!(orch.max_running_total, 3);
    assert_eq!(orch.max_running_per_backend.get("claude"), Some(&2));
    assert_eq!(orch.max_running_per_backend.get("codex"), Some(&2));
    assert_eq!(orch.max_running_per_backend.get("opencode"), Some(&2));
    assert_eq!(orch.max_running_per_backend.get("omp"), Some(&2));
    assert_eq!(orch.max_running_per_backend.get("pi"), Some(&2));
    assert!(orch.max_running_per_backend_model.is_empty());
    assert_eq!(orch.max_running_per_role.get("designer"), Some(&1));
    assert_eq!(orch.max_running_per_role.get("reviewer"), Some(&1));
    assert_eq!(orch.max_running_per_role.get("executor"), Some(&3));
    assert!(orch.auto_restart_enabled);
    assert_eq!(orch.auto_restart_delays_minutes, vec![1, 30, 270]);
    assert!(!orch.designer.enabled);
    assert_eq!(orch.designer.backend.as_deref(), Some("claude"));
    assert_eq!(orch.designer.model.as_deref(), Some("sonnet"));
    assert_eq!(orch.designer.effort, None);
    assert!(!orch.reviewer.enabled);
    assert_eq!(orch.reviewer.max_rounds, 3);
}

#[test]
fn partial_orchestration_gets_sibling_defaults_deep_merged() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  queue_enabled: false
  max_running_per_backend:
    claude: 5
  designer:
    enabled: true
"#,
    );

    let orch = config.get_orchestration().unwrap();
    // user values kept
    assert!(!orch.queue_enabled);
    assert_eq!(orch.max_running_per_backend.get("claude"), Some(&5));
    assert!(orch.designer.enabled);
    // sibling defaults filled in at every nesting level
    assert_eq!(orch.max_running_per_backend.get("opencode"), Some(&2));
    assert_eq!(orch.max_running_total, 3);
    assert_eq!(orch.designer.model.as_deref(), Some("sonnet"));
    assert_eq!(orch.designer.backend.as_deref(), Some("claude"));
    assert!(!orch.reviewer.enabled);
    assert_eq!(orch.auto_restart_delays_minutes, vec![1, 30, 270]);

    // the merged section survives save/reload with user keys intact
    let board = config.load().unwrap();
    config.save(&board).unwrap();
    let fresh = Config::new(dir.path());
    let orch = fresh.get_orchestration().unwrap();
    assert!(!orch.queue_enabled);
    assert_eq!(orch.max_running_per_backend.get("claude"), Some(&5));
    assert!(orch.designer.enabled);
    assert_eq!(orch.designer.model.as_deref(), Some("sonnet"));
}

#[test]
fn orchestration_strings_are_coerced() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  queue_enabled: "yes"
  max_running_total: "7"
  max_running_per_backend:
    claude: "4"
  auto_restart:
    enabled: "no"
    delays_minutes: ["5", "10"]
"#,
    );

    let orch = config.get_orchestration().unwrap();
    assert!(orch.queue_enabled);
    assert_eq!(orch.max_running_total, 7);
    assert_eq!(orch.max_running_per_backend.get("claude"), Some(&4));
    assert!(!orch.auto_restart_enabled);
    assert_eq!(orch.auto_restart_delays_minutes, vec![5, 10]);
}

#[test]
fn zero_cap_means_unlimited_negative_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\norchestration:\n  max_running_total: 0\n",
    );
    assert_eq!(config.get_orchestration().unwrap().max_running_total, 0);

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\norchestration:\n  max_running_total: -1\n",
    );
    assert!(config.load().is_err());
}

#[test]
fn bare_model_id_key_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  max_running_per_backend_model:
    opus: 1
"#,
    );
    let err = format!("{}", config.load().unwrap_err());
    assert!(
        err.contains("<backend>/<model>"),
        "error should explain the key shape: {err}"
    );
}

#[test]
fn backend_model_key_splits_on_first_slash_and_validates_backend() {
    assert_eq!(
        OrchestrationSettings::parse_backend_model_key("opencode/openai/gpt-5.5"),
        Some(("opencode", "openai/gpt-5.5"))
    );
    assert_eq!(
        OrchestrationSettings::backend_model_key("claude", "opus"),
        "claude/opus"
    );

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  max_running_per_backend_model:
    opencode/openai/gpt-5.5: 1
"#,
    );
    let orch = config.get_orchestration().unwrap();
    assert_eq!(
        orch.max_running_per_backend_model
            .get("opencode/openai/gpt-5.5"),
        Some(&1)
    );

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  max_running_per_backend_model:
    nosuch/model: 1
"#,
    );
    assert!(config.load().is_err());
}

#[test]
fn unknown_role_cap_key_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  max_running_per_role:
    reviewers: 1
"#,
    );
    let err = format!("{}", config.load().unwrap_err());
    assert!(
        err.contains("is not a role"),
        "a typo must not silently cap nothing: {err}"
    );

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  max_running_per_role:
    reviewer: 2
"#,
    );
    assert_eq!(
        config
            .get_orchestration()
            .unwrap()
            .max_running_per_role
            .get("reviewer"),
        Some(&2)
    );
}

#[test]
fn delays_minutes_must_be_positive_ints() {
    for bad in [
        "delays_minutes: [0]",
        "delays_minutes: [-3]",
        "delays_minutes: 5",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(
            &dir,
            &format!(
                "columns:\n- name: To Do\n  id: todo\norchestration:\n  auto_restart:\n    {bad}\n"
            ),
        );
        assert!(config.load().is_err(), "{bad} must be rejected");
    }
}

#[test]
fn reviewer_max_rounds_is_coerced_and_rejects_negatives() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\norchestration:\n  reviewer:\n    max_rounds: \"2\"\n",
    );
    assert_eq!(config.get_orchestration().unwrap().reviewer.max_rounds, 2);

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\norchestration:\n  reviewer:\n    max_rounds: 0\n",
    );
    assert_eq!(config.get_orchestration().unwrap().reviewer.max_rounds, 0);

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\norchestration:\n  reviewer:\n    max_rounds: -1\n",
    );
    assert!(config.load().is_err());
}

#[test]
fn on_changes_requested_is_restricted_to_todo_or_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\norchestration:\n  reviewer:\n    on_changes_requested: todo\n",
    );
    assert_eq!(
        config
            .get_orchestration()
            .unwrap()
            .reviewer
            .on_changes_requested,
        OnChangesRequested::Todo
    );

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\norchestration:\n  reviewer:\n    on_changes_requested: archive\n",
    );
    assert!(config.load().is_err());
}

#[test]
fn unknown_backend_cap_key_warns_but_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  max_running_per_backend:
    claude: 2
    opencodex: 1
"#,
    );
    // Rejecting would make the board unloadable, and `load` runs on every
    // command — there would be no way back in to fix the typo.
    config
        .load()
        .expect("an ineffective cap must not break the board");
    let orch = config.get_orchestration().unwrap();
    assert_eq!(orch.max_running_per_backend.get("claude"), Some(&2));

    let warnings = config.warnings();
    assert_eq!(warnings.len(), 1, "one warning: {warnings:?}");
    assert!(
        warnings[0].contains("opencodex") && warnings[0].contains("caps nothing"),
        "the user must be told the cap does nothing: {warnings:?}"
    );
}

#[test]
fn a_backend_added_under_agents_is_a_known_cap_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
agents:
  mybot:
    command: mybot
orchestration:
  max_running_per_backend:
    mybot: 1
"#,
    );
    config.load().unwrap();
    assert!(
        config.warnings().is_empty(),
        "a configured agent is a real backend: {:?}",
        config.warnings()
    );
}

#[test]
fn isolation_defaults_parse_without_the_block() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(&dir, "columns:\n- name: To Do\n  id: todo\n");

    let iso = config.get_orchestration().unwrap().isolation;
    assert_eq!(iso.mode, IsolationMode::Auto);
    assert_eq!(iso.branch_prefix, "kanban/");
    assert_eq!(iso.integration_ref, "refs/kanban/integration");
    assert_eq!(iso.seed, IsolationSeed::Live);
    assert_eq!(iso.land, IsolationLand::Worktree);
    assert_eq!(iso.on_conflict, IsolationOnConflict::Review);
    assert_eq!(iso.cleanup, IsolationCleanup::OnLand);
    assert_eq!(iso.commit_message, "kanban: {task_id} {title}");
}

#[test]
fn partial_isolation_gets_sibling_defaults_deep_merged() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  isolation:
    mode: required
    branch_prefix: wip/
"#,
    );

    let iso = config.get_orchestration().unwrap().isolation;
    // user values kept
    assert_eq!(iso.mode, IsolationMode::Required);
    assert_eq!(iso.branch_prefix, "wip/");
    // sibling defaults filled in
    assert_eq!(iso.seed, IsolationSeed::Live);
    assert_eq!(iso.integration_ref, "refs/kanban/integration");
    assert_eq!(iso.commit_message, "kanban: {task_id} {title}");

    // the merged block survives save/reload
    let board = config.load().unwrap();
    config.save(&board).unwrap();
    let fresh = Config::new(dir.path());
    let iso = fresh.get_orchestration().unwrap().isolation;
    assert_eq!(iso.mode, IsolationMode::Required);
    assert_eq!(iso.branch_prefix, "wip/");
    assert_eq!(iso.cleanup, IsolationCleanup::OnLand);
}

#[test]
fn every_known_isolation_value_parses_through() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        r#"columns:
- name: To Do
  id: todo
orchestration:
  isolation:
    mode: off
    seed: head
    land: manual
    on_conflict: resolver
    cleanup: keep
    commit_message: "task {task_id}: {title}"
"#,
    );

    let iso = config.get_orchestration().unwrap().isolation;
    assert_eq!(iso.mode, IsolationMode::Off);
    assert_eq!(iso.seed, IsolationSeed::Head);
    assert_eq!(iso.land, IsolationLand::Manual);
    assert_eq!(iso.on_conflict, IsolationOnConflict::Resolver);
    assert_eq!(iso.cleanup, IsolationCleanup::Keep);
    assert_eq!(iso.commit_message, "task {task_id}: {title}");
}

#[test]
fn unknown_isolation_values_are_rejected() {
    for bad in [
        "mode: sometimes",
        "seed: dirty",
        "land: auto",
        "on_conflict: force",
        "cleanup: never",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(
            &dir,
            &format!(
                "columns:\n- name: To Do\n  id: todo\norchestration:\n  isolation:\n    {bad}\n"
            ),
        );
        let err = format!("{}", config.load().unwrap_err());
        assert!(
            err.contains("orchestration.isolation."),
            "{bad} must be rejected with a pointer at the key: {err}"
        );
    }
}

#[test]
fn isolation_must_be_a_mapping_with_string_free_form_keys() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\norchestration:\n  isolation: 5\n",
    );
    let err = format!("{}", config.load().unwrap_err());
    assert!(err.contains("must be a mapping"), "{err}");

    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &dir,
        "columns:\n- name: To Do\n  id: todo\norchestration:\n  isolation:\n    branch_prefix: [wip]\n",
    );
    let err = format!("{}", config.load().unwrap_err());
    assert!(err.contains("branch_prefix must be a string"), "{err}");
}
