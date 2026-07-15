//! Configuration compatibility behavior from the earlier implementation.

mod common;

use std::fs;

use kanban4ai::core::config::Config;

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
        300
    );

    let board = config.load().unwrap();
    assert!(board.agents.contains_key("opencode"));
    assert!(board.agents.contains_key("claude"));
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
