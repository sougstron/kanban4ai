//! Per-project configuration (`.kanban/config.yaml`).
//!
//! Sections other than `columns` are kept as loose YAML mappings (like the
//! Python dicts) so user-added keys survive load/save; typed accessors coerce
//! values on read. All thresholds/rules used by business logic must come from
//! here — never hardcode them.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

use crate::core::error::{KanbanError, Result};
use crate::core::storage::atomic_write_text;

/// Written verbatim by `kanban init`; also the source of per-key fallbacks.
/// Mirrors the Python `DEFAULT_CONFIG` exactly.
pub const DEFAULT_CONFIG_YAML: &str = r#"columns:
- name: To Do
  id: todo
- name: In Progress
  id: in_progress
- name: Review
  id: review
- name: Done
  id: done
rules:
  one_task_per_instance: true
  user_only_review_to_done: true
  auto_move_on_assign: true
  auto_move_on_complete: true
  questions_go_to_review: false
  auto_launch_on_delegate: true
  auto_launch_chained: true
thresholds:
  context_embed_max_size: 5120
  context_warning: 51200
  context_auto_compact: 102400
  session_heartbeat_timeout: 1800
  context_summary_max_length: 5000
  tui_refresh_interval: 1
  question_poll_interval: 3
  question_wait_timeout: 600
  max_auto_resumes: 3
  waiting_min_eta: 10
  waiting_max_eta: 604800
  waiting_default_eta: 900
  waiting_eta_multiplier: 2
  waiting_note_max_chars: 1000
  agent_reply_max_chars: 4000
verification:
  command: null
  block_on_failure: true
tui:
  name: Kanban
  card_height_lines: 4
  card_line_max_symbols: 40
  max_tasks_per_column: 100
  theme: textual-dark
  task_sort: task_number
auto_launch:
  enabled: true
  use_tmux: true
  terminal_fallback: true
  auto_complete_on_exit: false
  default_agent: opencode
  model: openai/gpt-5.5
  models:
  - openai/gpt-5.5
  - opencode/deepseek-v4-flash-free
  - opencode-go/kimi-k2.7-code
  - opencode-go/deepseek-v4-flash
  - opencode-go/mimo-v2.5
  - opencode-go/minimax-m3
  agent: null
notifications:
  enabled: true
  questions: true
  completion: true
  chained_start: true
  waiting: true
  command: notify-send
  timeout: 3
  max_body_chars: 240
agents:
  opencode:
    command: opencode
    model: openai/gpt-5.5
    models:
    - openai/gpt-5.5
    - opencode/deepseek-v4-flash-free
    - opencode-go/kimi-k2.7-code
    - opencode-go/deepseek-v4-flash
    - opencode-go/mimo-v2.5
    - opencode-go/minimax-m3
    effort: null
    agent: null
    agent_options:
    - sisyphus
    - prometheus
    - hephaestus
    - atlas
    extra_args: []
  claude:
    command: claude
    model: sonnet
    models:
    - fable
    - opus
    - sonnet
    - haiku
    effort: null
    efforts:
    - low
    - medium
    - high
    - xhigh
    - max
    agent: null
    extra_args:
    - --dangerously-skip-permissions
  omp:
    command: omp
    model: null
    models:
    - openai-codex/gpt-5.6-sol
    - xai-oauth/grok-4.5
    effort: null
    efforts:
    - off
    - minimal
    - low
    - medium
    - high
    - xhigh
    - max
    agent: null
    extra_args: []
  pi:
    command: pi
    model: null
    models:
    - anthropic/claude-sonnet-5
    - openai-codex/gpt-5.6-sol
    effort: null
    efforts:
    - off
    - minimal
    - low
    - medium
    - high
    - xhigh
    - max
    agent: null
    extra_args: []
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardConfig {
    #[serde(default)]
    pub columns: Vec<Value>,
    #[serde(default)]
    pub rules: Mapping,
    #[serde(default)]
    pub thresholds: Mapping,
    #[serde(default)]
    pub tui: Mapping,
    #[serde(default)]
    pub auto_launch: Mapping,
    #[serde(default)]
    pub notifications: Mapping,
    #[serde(default)]
    pub agents: Mapping,
    #[serde(default)]
    pub verification: Mapping,
    #[serde(flatten, default)]
    pub extras: Mapping,
}

impl Default for BoardConfig {
    fn default() -> Self {
        serde_yaml_ng::from_str(DEFAULT_CONFIG_YAML).expect("built-in default config is valid")
    }
}

impl BoardConfig {
    pub fn column_ids(&self) -> Vec<String> {
        self.columns
            .iter()
            .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    pub fn column_names(&self) -> Vec<String> {
        self.columns
            .iter()
            .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }
}

fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

/// Insert every key from `defaults` that `target` is missing.
fn merge_missing(target: &mut Mapping, defaults: &Mapping) {
    for (key, value) in defaults {
        if !target.contains_key(key) {
            target.insert(key.clone(), value.clone());
        }
    }
}

/// Add new built-in choices to an existing catalog while preserving its order
/// and any user-defined entries.
fn merge_missing_sequence_values(target: &mut Mapping, defaults: &Mapping, key: &str) {
    let yaml_key = Value::String(key.to_owned());
    let Some(default_values) = defaults.get(&yaml_key).and_then(Value::as_sequence) else {
        return;
    };
    let Some(values) = target.get_mut(&yaml_key).and_then(Value::as_sequence_mut) else {
        return;
    };
    for value in default_values {
        if !values.contains(value) {
            values.push(value.clone());
        }
    }
}

pub struct Config {
    pub project_path: PathBuf,
    pub kanban_dir: PathBuf,
    pub config_file: PathBuf,
    cache: RefCell<Option<BoardConfig>>,
}

impl Config {
    pub fn new(project_path: impl AsRef<Path>) -> Self {
        let project_path = project_path.as_ref().to_path_buf();
        let kanban_dir = project_path.join(".kanban");
        let config_file = kanban_dir.join("config.yaml");
        Config {
            project_path,
            kanban_dir,
            config_file,
            cache: RefCell::new(None),
        }
    }

    pub fn exists(&self) -> bool {
        self.config_file.exists()
    }

    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.kanban_dir)?;
        fs::write(&self.config_file, DEFAULT_CONFIG_YAML)?;
        Ok(())
    }

    pub fn load(&self) -> Result<BoardConfig> {
        if let Some(cached) = self.cache.borrow().as_ref() {
            return Ok(cached.clone());
        }
        if !self.exists() {
            self.init()?;
        }
        let raw = fs::read_to_string(&self.config_file)?;
        let mut config: BoardConfig = if raw.trim().is_empty() {
            BoardConfig::default()
        } else {
            serde_yaml_ng::from_str(&raw)?
        };
        if config.columns.is_empty() {
            config.columns = BoardConfig::default().columns;
        }
        Self::validate(&mut config)?;
        *self.cache.borrow_mut() = Some(config.clone());
        Ok(config)
    }

    /// Discard this instance's cached view and reload the current file.
    /// Callers that perform a locked read-modify-write use this to avoid
    /// overwriting changes made by another process after an earlier read.
    pub fn load_fresh(&self) -> Result<BoardConfig> {
        *self.cache.borrow_mut() = None;
        self.load()
    }

    pub fn save(&self, config: &BoardConfig) -> Result<()> {
        fs::create_dir_all(&self.kanban_dir)?;
        atomic_write_text(&self.config_file, &serde_yaml_ng::to_string(config)?)?;
        *self.cache.borrow_mut() = Some(config.clone());
        Ok(())
    }

    fn validate(config: &mut BoardConfig) -> Result<()> {
        if config.column_ids().is_empty() {
            return Err(KanbanError::Invalid(
                "Config must have at least one column".into(),
            ));
        }

        Self::ensure_defaults(config);

        let rules = config.rules.clone();
        for (key, value) in &rules {
            if matches!(value, Value::String(_)) {
                match as_bool(value) {
                    Some(coerced) => {
                        config.rules.insert(key.clone(), Value::Bool(coerced));
                    }
                    None => {
                        return Err(KanbanError::Invalid(format!(
                            "Invalid boolean value for rule '{}': {:?}",
                            key.as_str().unwrap_or("?"),
                            value
                        )));
                    }
                }
            }
        }

        let thresholds = config.thresholds.clone();
        for (key, value) in &thresholds {
            if value.as_i64().is_none() {
                match as_int(value) {
                    Some(coerced) => {
                        config
                            .thresholds
                            .insert(key.clone(), Value::Number(coerced.into()));
                    }
                    None => {
                        return Err(KanbanError::Invalid(format!(
                            "Invalid integer value for threshold '{}': {:?}",
                            key.as_str().unwrap_or("?"),
                            value
                        )));
                    }
                }
            }
        }

        const BOOL_AUTO_LAUNCH: [&str; 4] = [
            "enabled",
            "use_tmux",
            "terminal_fallback",
            "auto_complete_on_exit",
        ];
        let auto_launch = config.auto_launch.clone();
        for (key, value) in &auto_launch {
            let Some(key_str) = key.as_str() else {
                continue;
            };
            if !BOOL_AUTO_LAUNCH.contains(&key_str) || !matches!(value, Value::String(_)) {
                continue;
            }
            match as_bool(value) {
                Some(coerced) => {
                    config.auto_launch.insert(key.clone(), Value::Bool(coerced));
                }
                None => {
                    return Err(KanbanError::Invalid(format!(
                        "Invalid boolean value for auto_launch '{key_str}': {value:?}"
                    )));
                }
            }
        }

        const BOOL_NOTIFICATIONS: [&str; 5] = [
            "enabled",
            "questions",
            "completion",
            "chained_start",
            "waiting",
        ];
        const INT_NOTIFICATIONS: [&str; 2] = ["timeout", "max_body_chars"];
        let notifications = config.notifications.clone();
        for (key, value) in &notifications {
            let Some(key_str) = key.as_str() else {
                continue;
            };
            if BOOL_NOTIFICATIONS.contains(&key_str) && matches!(value, Value::String(_)) {
                match as_bool(value) {
                    Some(coerced) => {
                        config
                            .notifications
                            .insert(key.clone(), Value::Bool(coerced));
                    }
                    None => {
                        return Err(KanbanError::Invalid(format!(
                            "Invalid boolean value for notifications '{key_str}': {value:?}"
                        )));
                    }
                }
            } else if INT_NOTIFICATIONS.contains(&key_str) && value.as_i64().is_none() {
                match as_int(value) {
                    Some(coerced) => {
                        config
                            .notifications
                            .insert(key.clone(), Value::Number(coerced.into()));
                    }
                    None => {
                        return Err(KanbanError::Invalid(format!(
                            "Invalid integer value for notifications '{key_str}': {value:?}"
                        )));
                    }
                }
            }
        }

        Ok(())
    }

    fn ensure_defaults(config: &mut BoardConfig) {
        let defaults = BoardConfig::default();
        merge_missing(&mut config.rules, &defaults.rules);
        merge_missing(&mut config.thresholds, &defaults.thresholds);
        merge_missing(&mut config.tui, &defaults.tui);
        merge_missing(&mut config.auto_launch, &defaults.auto_launch);
        merge_missing(&mut config.notifications, &defaults.notifications);
        merge_missing(&mut config.verification, &defaults.verification);
        for (backend, settings) in &defaults.agents {
            match config.agents.get_mut(backend) {
                Some(Value::Mapping(existing)) => {
                    if let Value::Mapping(default_settings) = settings {
                        merge_missing(existing, default_settings);
                        merge_missing_sequence_values(existing, default_settings, "models");
                    }
                }
                _ => {
                    config.agents.insert(backend.clone(), settings.clone());
                }
            }
        }
    }

    pub fn get_column_ids(&self) -> Result<Vec<String>> {
        Ok(self.load()?.column_ids())
    }

    pub fn get_column_names(&self) -> Result<Vec<String>> {
        Ok(self.load()?.column_names())
    }

    /// Threshold by key, falling back to the built-in default. Unknown keys are
    /// a programming error (mirrors the Python `KeyError`).
    pub fn get_threshold(&self, key: &str) -> Result<i64> {
        let config = self.load()?;
        if let Some(value) = config.thresholds.get(key).and_then(as_int) {
            return Ok(value);
        }
        BoardConfig::default()
            .thresholds
            .get(key)
            .and_then(as_int)
            .ok_or_else(|| KanbanError::Invalid(format!("unknown threshold: {key}")))
    }

    pub fn get_rule(&self, key: &str) -> Result<bool> {
        let config = self.load()?;
        if let Some(value) = config.rules.get(key).and_then(as_bool) {
            return Ok(value);
        }
        BoardConfig::default()
            .rules
            .get(key)
            .and_then(as_bool)
            .ok_or_else(|| KanbanError::Invalid(format!("unknown rule: {key}")))
    }

    pub fn get_notifications(&self) -> Result<Mapping> {
        Ok(self.load()?.notifications)
    }

    /// Verification gate command, if configured and non-empty. Empty or unset
    /// means the gate is disabled and completions bypass it.
    pub fn get_verification_command(&self) -> Result<Option<String>> {
        let config = self.load()?;
        let command = config
            .verification
            .get("command")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .filter(|s| !s.trim().is_empty());
        Ok(command)
    }

    /// Whether a failed verification gate should block the InProgress→Review
    /// transition. Defaults to true when the verification block is present.
    pub fn get_verification_block_on_failure(&self) -> Result<bool> {
        let config = self.load()?;
        if let Some(value) = config
            .verification
            .get("block_on_failure")
            .and_then(as_bool)
        {
            return Ok(value);
        }
        BoardConfig::default()
            .verification
            .get("block_on_failure")
            .and_then(as_bool)
            .ok_or_else(|| KanbanError::Invalid("unknown verification block_on_failure".into()))
    }
}
