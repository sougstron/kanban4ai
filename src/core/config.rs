//! Per-project configuration (`.kanban/config.yaml`).
//!
//! Sections other than `columns` are kept as loose YAML mappings (like the
//! Python dicts) so user-added keys survive load/save; typed accessors coerce
//! values on read. All thresholds/rules used by business logic must come from
//! here — never hardcode them.

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::core::error::{KanbanError, Result};
use crate::core::models::{Role, Task};
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
  resume_after_last_answer: true
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
  agent_reply_max_chars: 32768
  agent_reply_message_max_chars: 8192
  limits_refresh_interval: 120
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
  show_limits: true
  hide_kanban_messages: false
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
  codex:
    command: codex
    model: gpt-5.5
    models:
    - gpt-5.5
    effort: null
    efforts:
    - low
    - medium
    - high
    - xhigh
    agent: null
    extra_args:
    - --dangerously-bypass-approvals-and-sandbox
    - --skip-git-repo-check
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
orchestration:
  queue_enabled: true
  max_running_total: 3
  max_running_per_backend:
    claude: 2
    codex: 2
    opencode: 2
    omp: 2
    pi: 2
  max_running_per_backend_model: {}
  max_running_per_role:
    orchestrator: 1
    designer: 1
    reviewer: 1
    executor: 3
  auto_restart:
    enabled: true
    delays_minutes:
    - 1
    - 30
    - 270
  designer:
    enabled: false
    backend: claude
    model: sonnet
    effort: null
    agent: null
  reviewer:
    enabled: false
    backend: claude
    model: sonnet
    effort: null
    agent: null
    on_changes_requested: in_progress
    max_rounds: 3
  orchestrator:
    max_subtasks: 12
    upstream_budget_chars: 4000
  roles: {}
  isolation:
    mode: auto
    branch_prefix: kanban/
    integration_ref: refs/kanban/integration
    seed: live
    land: worktree
    on_conflict: review
    cleanup: on_land
    commit_message: "kanban: {task_id} {title}"
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
    #[serde(default)]
    pub orchestration: Mapping,
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

/// Typed view of one role bot's launch settings (`orchestration.designer` /
/// `orchestration.reviewer`). `None` fields mean "inherit the backend's
/// configured default at launch time".
#[derive(Debug, Clone)]
pub struct BotSettings {
    pub enabled: bool,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
}

/// What happens to a task after the reviewer bot requests changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnChangesRequested {
    InProgress,
    Todo,
}

/// Typed snapshot of the whole `orchestration:` section. Business logic reads
/// this — never the raw `Mapping`, and no threshold is hardcoded elsewhere.
#[derive(Debug, Clone)]
pub struct OrchestrationSettings {
    pub queue_enabled: bool,
    /// Total concurrently running agents. `0` means unlimited.
    pub max_running_total: i64,
    /// Per-backend caps keyed by backend name. `0` means unlimited.
    pub max_running_per_backend: HashMap<String, i64>,
    /// Per `<backend>/<model>` pair caps (see [`Self::backend_model_key`]).
    /// `0` means unlimited.
    pub max_running_per_backend_model: HashMap<String, i64>,
    /// Per-role caps keyed by `designer` / `reviewer` / `executor`.
    /// `0` means unlimited.
    pub max_running_per_role: HashMap<String, i64>,
    pub auto_restart_enabled: bool,
    /// Crash-restart backoff schedule in minutes; entry `n` is the delay
    /// before attempt `n + 1`. Exhausting it leaves the task crashed.
    pub auto_restart_delays_minutes: Vec<i64>,
    pub designer: BotSettings,
    pub reviewer: ReviewerSettings,
    pub orchestrator: OrchestratorSettings,
    /// Named model rosters the orchestrator assigns to the nodes it plans
    /// (`orchestration.roles`). Ordered map so the roster list handed to the
    /// orchestrator prompt is stable between runs.
    pub roles: BTreeMap<String, Vec<RoleCandidate>>,
    pub isolation: IsolationSettings,
}

/// `orchestration.orchestrator`: bounds on a planning pass. There is no
/// `enabled` key — the orchestrator is a per-task opt-in
/// ([`Task::use_orchestrator`]) and runs on the task's own backend/model, so
/// nothing here selects a bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorSettings {
    /// Most nodes one plan may create. A refused plan costs one message; an
    /// accepted 200-node plan costs 200 sessions.
    pub max_subtasks: i64,
    /// Character budget for the whole *Upstream results* section a dependent
    /// task is prompted with, split across its dependencies.
    pub upstream_budget_chars: i64,
}

/// One entry in an `orchestration.roles` roster: a launch the orchestrator may
/// assign to a node. Candidates are tried in order — the next one takes over
/// when the current one fails on a provider limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCandidate {
    pub backend: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
}

impl RoleCandidate {
    /// `claude/sonnet`-style label for prompts, logs and the detail view.
    pub fn label(&self) -> String {
        match (self.backend.as_deref(), self.model.as_deref()) {
            (Some(backend), Some(model)) => format!("{backend}/{model}"),
            (Some(backend), None) => backend.to_string(),
            (None, Some(model)) => model.to_string(),
            (None, None) => "default".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewerSettings {
    pub enabled: bool,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
    pub on_changes_requested: OnChangesRequested,
    /// Consecutive bot-review bounces before falling through to human Review.
    /// `0` means unlimited.
    pub max_rounds: i64,
}

/// `orchestration.isolation.mode`: when a task's agent runs in an isolated
/// git worktree. `auto` isolates when the work path is a git repo new enough
/// for `merge-tree` (>= 2.38) and the project is registered, else falls back
/// to the shared-directory behavior; `required` refuses to launch when
/// isolation is unavailable; `off` is always the shared directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationMode {
    Auto,
    Off,
    Required,
}

/// `orchestration.isolation.seed`: what a task branch starts from — a
/// snapshot of the dirty work path (`live`) or committed `HEAD` (`head`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationSeed {
    Live,
    Head,
}

/// `orchestration.isolation.land`: how a finished task branch reaches the
/// user's working tree — materialized automatically (`worktree`) or left to
/// the human (`manual`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLand {
    Worktree,
    Manual,
}

/// `orchestration.isolation.on_conflict`: what happens when the merge back
/// conflicts — hand to human Review (`review`) or merge into the task's own
/// worktree for the resolver flow (`resolver`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationOnConflict {
    Review,
    Resolver,
}

/// `orchestration.isolation.cleanup`: remove the worktree and branch when the
/// branch has landed (`on_land`) or keep them (`keep`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationCleanup {
    OnLand,
    Keep,
}

macro_rules! isolation_choice {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        impl $name {
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($value => Some($name::$variant),)+
                    _ => None,
                }
            }

            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $value),+
                }
            }
        }
    };
}

isolation_choice!(IsolationMode {
    Auto => "auto",
    Off => "off",
    Required => "required",
});

isolation_choice!(IsolationSeed {
    Live => "live",
    Head => "head",
});

isolation_choice!(IsolationLand {
    Worktree => "worktree",
    Manual => "manual",
});

isolation_choice!(IsolationOnConflict {
    Review => "review",
    Resolver => "resolver",
});

isolation_choice!(IsolationCleanup {
    OnLand => "on_land",
    Keep => "keep",
});

/// Typed view of `orchestration.isolation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationSettings {
    pub mode: IsolationMode,
    pub branch_prefix: String,
    pub integration_ref: String,
    pub seed: IsolationSeed,
    pub land: IsolationLand,
    pub on_conflict: IsolationOnConflict,
    pub cleanup: IsolationCleanup,
    /// Template for agent commits inside the task worktree; `{task_id}` and
    /// `{title}` are substituted at launch time.
    pub commit_message: String,
}

impl OrchestrationSettings {
    pub fn designer_enabled_for(&self, task: &Task) -> bool {
        self.designer.enabled || task.use_designer
    }

    pub fn reviewer_enabled_for(&self, task: &Task) -> bool {
        self.reviewer.enabled || task.use_reviewer
    }
}

impl OrchestrationSettings {
    /// The candidate a node with this role profile and roster position runs
    /// on. An index past the end of the roster clamps to the last entry, so a
    /// task whose failover ran out keeps a usable assignment instead of
    /// silently losing its profile.
    pub fn role_candidate(&self, profile: &str, index: u32) -> Option<&RoleCandidate> {
        let roster = self.roles.get(profile).filter(|r| !r.is_empty())?;
        Some(&roster[(index as usize).min(roster.len() - 1)])
    }

    /// Whether a further candidate exists after `index` — i.e. whether a
    /// limit failure can fail over instead of waiting for the quota window.
    pub fn has_next_role_candidate(&self, profile: &str, index: u32) -> bool {
        self.roles
            .get(profile)
            .is_some_and(|roster| (index as usize + 1) < roster.len())
    }
}

/// Parse `orchestration.roles` into ordered rosters. Two spellings per entry:
/// a mapping (`{backend: claude, model: sonnet}`) or the `<backend>/<model>`
/// shorthand string; a bare string with no slash is a backend on its own
/// defaults. Malformed entries are dropped here and rejected with a message by
/// [`Config::validate_orchestration`].
fn parse_role_rosters(mapping: &Mapping) -> BTreeMap<String, Vec<RoleCandidate>> {
    let field = |m: &Mapping, key: &str| -> Option<String> {
        m.get(Value::String(key.to_owned()))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let mut rosters = BTreeMap::new();
    for (name, value) in mapping {
        let Some(name) = name.as_str().map(str::to_owned) else {
            continue;
        };
        // Both `roles.x: [..]` and `roles.x.models: [..]` are accepted; the
        // second leaves room for future per-profile keys.
        let items = match value {
            Value::Sequence(items) => Some(items),
            Value::Mapping(m) => m.get("models").and_then(Value::as_sequence),
            _ => None,
        };
        let Some(items) = items else {
            continue;
        };
        let candidates: Vec<RoleCandidate> = items
            .iter()
            .filter_map(|item| match item {
                Value::Mapping(m) => Some(RoleCandidate {
                    backend: field(m, "backend"),
                    model: field(m, "model"),
                    effort: field(m, "effort"),
                    agent: field(m, "agent"),
                }),
                Value::String(spec) => {
                    let spec = spec.trim();
                    if spec.is_empty() {
                        return None;
                    }
                    let (backend, model) = match spec.split_once('/') {
                        Some((backend, model)) => (backend, Some(model.to_owned())),
                        None => (spec, None),
                    };
                    Some(RoleCandidate {
                        backend: Some(backend.to_owned()),
                        model,
                        effort: None,
                        agent: None,
                    })
                }
                _ => None,
            })
            .collect();
        if !candidates.is_empty() {
            rosters.insert(name, candidates);
        }
    }
    rosters
}

impl ReviewerSettings {
    /// Launch settings for the reviewer bot (backend/model/effort/agent).
    pub fn bot(&self) -> BotSettings {
        BotSettings {
            enabled: self.enabled,
            backend: self.backend.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            agent: self.agent.clone(),
        }
    }
}

pub(crate) fn as_bool(value: &Value) -> Option<bool> {
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

/// Like [`merge_missing`], but recurses into nested mappings so a
/// partially-specified section gets its sibling defaults filled in. Used only
/// for `orchestration`; every other section keeps the exact shallow semantics
/// it has always had.
fn merge_missing_deep(target: &mut Mapping, defaults: &Mapping) {
    for (key, value) in defaults {
        match target.get_mut(key) {
            Some(Value::Mapping(existing)) => {
                if let Value::Mapping(default_value) = value {
                    merge_missing_deep(existing, default_value);
                }
            }
            Some(_) => {}
            None => {
                target.insert(key.clone(), value.clone());
            }
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

fn sub_mapping_mut<'a>(section: &'a mut Mapping, key: &str) -> Option<&'a mut Mapping> {
    match section.get_mut(Value::String(key.to_owned())) {
        Some(Value::Mapping(m)) => Some(m),
        _ => None,
    }
}

/// Coerce a boolean config key in place (string spellings accepted, like the
/// `rules:` section). Present-but-uncoercible is a config error.
fn coerce_bool_field(map: &mut Mapping, key: &str, path: &str) -> Result<()> {
    let yaml_key = Value::String(key.to_owned());
    let Some(value) = map.get(&yaml_key) else {
        return Ok(());
    };
    if matches!(value, Value::Bool(_) | Value::Null) {
        return Ok(());
    }
    match as_bool(value) {
        Some(coerced) => {
            map.insert(yaml_key, Value::Bool(coerced));
            Ok(())
        }
        None => Err(KanbanError::Invalid(format!(
            "Invalid boolean value for {path}.{key}: {value:?}"
        ))),
    }
}

/// Coerce a scalar non-negative int cap in place. `0` means unlimited;
/// negative or unparseable values are a config error.
fn coerce_int_cap(map: &mut Mapping, key: &str) -> Result<()> {
    let yaml_key = Value::String(key.to_owned());
    let Some(value) = map.get(&yaml_key) else {
        return Ok(());
    };
    if matches!(value, Value::Null) {
        return Ok(());
    }
    let parsed = as_int(value).ok_or_else(|| {
        KanbanError::Invalid(format!(
            "Invalid integer value for orchestration.{key}: {value:?}"
        ))
    })?;
    if parsed < 0 {
        return Err(KanbanError::Invalid(format!(
            "orchestration.{key} must not be negative (0 means unlimited): {parsed}"
        )));
    }
    map.insert(yaml_key, Value::Number(parsed.into()));
    Ok(())
}

/// Validate one cap mapping (`max_running_per_backend`, `..._per_backend_model`,
/// `..._per_role`): every value is a coerced non-negative int; for
/// `max_running_per_backend_model` every key must be `<known-backend>/<model>`
/// — split on the FIRST slash only, because model ids themselves contain
/// slashes (`opencode/openai/gpt-5.5`). A bare model id would silently never
/// match any census key, so it is rejected outright. `..._per_role` keys are a
/// closed set (`executor` / `designer` / `reviewer`) and are checked for the
/// same reason: a typo would cap nothing and look like it worked.
///
/// `max_running_per_backend` keys are checked too, but only as a *warning*:
/// backends are user-extensible, so a cap left behind for an agent that was
/// since dropped from `agents:` would otherwise make the whole board
/// unloadable — and `load` runs on every command, so there would be no way
/// back in to fix it. The cap still does nothing; the user is told so.
fn validate_cap_map(
    section: &mut Mapping,
    key: &str,
    known_backends: &[String],
    warnings: &mut Vec<String>,
) -> Result<()> {
    let yaml_key = Value::String(key.to_owned());
    let check_keys = key == "max_running_per_backend_model";
    let check_roles = key == "max_running_per_role";
    let warn_backends = key == "max_running_per_backend";
    match section.get_mut(&yaml_key) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Mapping(caps)) => {
            let snapshot: Vec<(Value, Value)> =
                caps.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (cap_key, value) in snapshot {
                let Some(key_str) = cap_key.as_str() else {
                    return Err(KanbanError::Invalid(format!(
                        "orchestration '{key}' keys must be strings: {cap_key:?}"
                    )));
                };
                if check_keys {
                    let Some((backend, model)) =
                        OrchestrationSettings::parse_backend_model_key(key_str)
                    else {
                        return Err(KanbanError::Invalid(format!(
                            "orchestration '{key}' entry '{key_str}' must be '<backend>/<model>' (a bare model id would silently never match)"
                        )));
                    };
                    if model.is_empty() {
                        return Err(KanbanError::Invalid(format!(
                            "orchestration '{key}' entry '{key_str}' has an empty model id"
                        )));
                    }
                    if !known_backends.iter().any(|b| b == backend) {
                        return Err(KanbanError::Invalid(format!(
                            "orchestration '{key}' entry '{key_str}' names unknown backend '{backend}' (known: {})",
                            known_backends.join(", ")
                        )));
                    }
                }
                if warn_backends && !known_backends.iter().any(|b| b == key_str) {
                    warnings.push(format!(
                        "orchestration '{key}' entry '{key_str}' names unknown backend '{key_str}' and caps nothing (known: {})",
                        known_backends.join(", ")
                    ));
                }
                if check_roles && key_str.parse::<Role>().is_err() {
                    return Err(KanbanError::Invalid(format!(
                        "orchestration '{key}' entry '{key_str}' is not a role (known: executor, orchestrator, designer, reviewer)"
                    )));
                }
                let parsed = as_int(&value).ok_or_else(|| {
                    KanbanError::Invalid(format!(
                        "Invalid integer value for orchestration '{key}' entry '{key_str}': {value:?}"
                    ))
                })?;
                if parsed < 0 {
                    return Err(KanbanError::Invalid(format!(
                        "orchestration '{key}' entry '{key_str}' must not be negative (0 means unlimited): {parsed}"
                    )));
                }
                caps.insert(cap_key, Value::Number(parsed.into()));
            }
            Ok(())
        }
        Some(other) => Err(KanbanError::Invalid(format!(
            "orchestration '{key}' must be a mapping, got: {other:?}"
        ))),
    }
}

/// `auto_restart.delays_minutes`: a sequence of positive ints (minutes before
/// each crash-restart attempt). Zero/negative or unparseable entries are a
/// config error.
/// One closed-set `orchestration.isolation` key: the value must parse into
/// the typed choice or be absent/null. An unknown spelling (a typo, or a
/// value for a mode later tasks do not implement) is rejected rather than
/// silently falling back to the default.
fn validate_isolation_choice<T>(
    iso: &Mapping,
    key: &str,
    allowed: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<()> {
    let Some(value) = iso.get(Value::String(key.to_owned())) else {
        return Ok(());
    };
    if matches!(value, Value::Null) {
        return Ok(());
    }
    if value.as_str().and_then(parse).is_some() {
        return Ok(());
    }
    Err(KanbanError::Invalid(format!(
        "orchestration.isolation.{key} must be one of {allowed}, got: {value:?}"
    )))
}

/// One free-form `orchestration.isolation` string key (`branch_prefix`,
/// `integration_ref`, `commit_message`): must be a string when present. A
/// non-string would otherwise be silently replaced by the default.
fn validate_isolation_string(iso: &Mapping, key: &str) -> Result<()> {
    let Some(value) = iso.get(Value::String(key.to_owned())) else {
        return Ok(());
    };
    if matches!(value, Value::Null | Value::String(_)) {
        return Ok(());
    }
    Err(KanbanError::Invalid(format!(
        "orchestration.isolation.{key} must be a string, got: {value:?}"
    )))
}

/// `orchestration.roles`: every profile must be a non-empty roster whose
/// entries name a known backend. A roster the orchestrator would find empty is
/// an error (it would silently drop the assignment); an unknown backend is a
/// warning, matching `max_running_per_backend` — the board still runs, the
/// profile just falls back to the task's own settings.
fn validate_role_rosters(
    orch: &Mapping,
    known_backends: &[String],
    warnings: &mut Vec<String>,
) -> Result<()> {
    let yaml_key = Value::String("roles".to_owned());
    let Some(roles) = orch.get(&yaml_key).filter(|v| !matches!(v, Value::Null)) else {
        return Ok(());
    };
    let Some(roles) = roles.as_mapping() else {
        return Err(KanbanError::Invalid(format!(
            "orchestration.roles must be a mapping of profile name to model roster, got: {roles:?}"
        )));
    };
    let parsed = parse_role_rosters(roles);
    for (name, _) in roles {
        let Some(name) = name.as_str() else {
            return Err(KanbanError::Invalid(
                "orchestration.roles profile names must be strings".to_string(),
            ));
        };
        let Some(candidates) = parsed.get(name) else {
            return Err(KanbanError::Invalid(format!(
                "orchestration.roles '{name}' has no usable model: give a list of \
                 '<backend>/<model>' strings or {{backend, model}} mappings"
            )));
        };
        for candidate in candidates {
            let Some(backend) = candidate.backend.as_deref() else {
                continue;
            };
            if !known_backends.iter().any(|known| known == backend) {
                warnings.push(format!(
                    "orchestration.roles '{name}' entry '{}' names unknown backend '{backend}' \
                     and falls back to the task's own settings (known: {})",
                    candidate.label(),
                    known_backends.join(", ")
                ));
            }
        }
    }
    Ok(())
}

/// `orchestration.isolation`: strict validation of the worktree-isolation
/// block. Unknown values for the closed-set keys and non-string values for
/// the free-form keys are config errors.
fn validate_isolation(orch: &Mapping) -> Result<()> {
    let yaml_key = Value::String("isolation".to_owned());
    let Some(iso) = orch.get(&yaml_key) else {
        return Ok(());
    };
    if matches!(iso, Value::Null) {
        return Ok(());
    }
    let Some(iso) = iso.as_mapping() else {
        return Err(KanbanError::Invalid(format!(
            "orchestration.isolation must be a mapping, got: {iso:?}"
        )));
    };
    validate_isolation_choice(
        iso,
        "mode",
        "'auto', 'off', 'required'",
        IsolationMode::parse,
    )?;
    validate_isolation_choice(iso, "seed", "'live', 'head'", IsolationSeed::parse)?;
    validate_isolation_choice(iso, "land", "'worktree', 'manual'", IsolationLand::parse)?;
    validate_isolation_choice(
        iso,
        "on_conflict",
        "'review', 'resolver'",
        IsolationOnConflict::parse,
    )?;
    validate_isolation_choice(iso, "cleanup", "'on_land', 'keep'", IsolationCleanup::parse)?;
    validate_isolation_string(iso, "branch_prefix")?;
    validate_isolation_string(iso, "integration_ref")?;
    validate_isolation_string(iso, "commit_message")
}

fn validate_delays_minutes(auto_restart: &mut Mapping) -> Result<()> {
    let yaml_key = Value::String("delays_minutes".to_owned());
    match auto_restart.get_mut(&yaml_key) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Sequence(items)) => {
            let mut parsed_values = Vec::with_capacity(items.len());
            for item in items.iter() {
                let parsed = as_int(item).ok_or_else(|| {
                    KanbanError::Invalid(format!(
                        "Invalid integer value for orchestration.auto_restart.delays_minutes: {item:?}"
                    ))
                })?;
                if parsed <= 0 {
                    return Err(KanbanError::Invalid(format!(
                        "orchestration.auto_restart.delays_minutes entries must be positive minutes, got: {parsed}"
                    )));
                }
                parsed_values.push(parsed);
            }
            *items = parsed_values
                .into_iter()
                .map(|n| Value::Number(n.into()))
                .collect();
            Ok(())
        }
        Some(other) => Err(KanbanError::Invalid(format!(
            "orchestration.auto_restart.delays_minutes must be a sequence of positive integers, got: {other:?}"
        ))),
    }
}

impl OrchestrationSettings {
    /// Canonical census key for a backend/model pair — one spelling shared by
    /// config keys (`max_running_per_backend_model`), the running-agent
    /// census, the settings UI and the docs.
    pub fn backend_model_key(backend: &str, model: &str) -> String {
        format!("{backend}/{model}")
    }

    /// Split a `<backend>/<model>` key on the FIRST slash only: model ids
    /// themselves contain slashes, so `opencode/openai/gpt-5.5` is backend
    /// `opencode`, model `openai/gpt-5.5`.
    pub fn parse_backend_model_key(key: &str) -> Option<(&str, &str)> {
        key.split_once('/')
    }

    /// Build the typed snapshot from a loaded (defaults-merged) mapping.
    /// Every key falls back to the built-in default so callers never see a
    /// missing value.
    pub(crate) fn from_mapping(mapping: &Mapping) -> Self {
        let defaults = BoardConfig::default().orchestration;
        let bool_at = |key: &str| -> bool {
            mapping
                .get(key)
                .and_then(as_bool)
                .or_else(|| defaults.get(key).and_then(as_bool))
                .unwrap_or(false)
        };
        let int_at = |key: &str| -> i64 {
            mapping
                .get(key)
                .and_then(as_int)
                .or_else(|| defaults.get(key).and_then(as_int))
                .unwrap_or(0)
        };
        let str_opt_at = |section: &Mapping, key: &str| -> Option<String> {
            section
                .get(Value::String(key.to_owned()))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
        };
        let cap_map_at = |key: &str| -> HashMap<String, i64> {
            match mapping
                .get(Value::String(key.to_owned()))
                .or_else(|| defaults.get(key))
            {
                Some(Value::Mapping(m)) => m
                    .iter()
                    .filter_map(|(k, v)| {
                        let name = k.as_str()?.to_owned();
                        as_int(v).map(|n| (name, n))
                    })
                    .collect(),
                _ => HashMap::new(),
            }
        };
        let bot_at = |section: &Mapping| -> BotSettings {
            BotSettings {
                enabled: section.get("enabled").and_then(as_bool).unwrap_or(false),
                backend: str_opt_at(section, "backend"),
                model: str_opt_at(section, "model"),
                effort: str_opt_at(section, "effort"),
                agent: str_opt_at(section, "agent"),
            }
        };
        let default_section =
            |key: &str| -> Option<&Mapping> { defaults.get(key).and_then(Value::as_mapping) };
        let designer = mapping
            .get("designer")
            .and_then(Value::as_mapping)
            .or_else(|| default_section("designer"))
            .expect("built-in defaults contain orchestration.designer");
        let reviewer = mapping
            .get("reviewer")
            .and_then(Value::as_mapping)
            .or_else(|| default_section("reviewer"))
            .expect("built-in defaults contain orchestration.reviewer");
        let on_changes_requested = reviewer
            .get("on_changes_requested")
            .and_then(Value::as_str)
            .and_then(|s| match s {
                "todo" => Some(OnChangesRequested::Todo),
                "in_progress" => Some(OnChangesRequested::InProgress),
                _ => None,
            })
            .unwrap_or(OnChangesRequested::InProgress);
        let max_rounds = reviewer
            .get("max_rounds")
            .and_then(as_int)
            .or_else(|| {
                default_section("reviewer")
                    .and_then(|m| m.get("max_rounds"))
                    .and_then(as_int)
            })
            .unwrap_or(3);
        let delays = mapping
            .get("auto_restart")
            .and_then(|v| v.get("delays_minutes"))
            .and_then(Value::as_sequence);
        let auto_restart_enabled = mapping
            .get("auto_restart")
            .and_then(|v| v.get("enabled"))
            .and_then(as_bool)
            .or_else(|| {
                defaults
                    .get("auto_restart")
                    .and_then(|v| v.get("enabled"))
                    .and_then(as_bool)
            })
            .unwrap_or(true);
        let auto_restart_delays_minutes = delays
            .map(|items| items.iter().filter_map(as_int).collect::<Vec<i64>>())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                defaults
                    .get("auto_restart")
                    .and_then(|v| v.get("delays_minutes"))
                    .and_then(Value::as_sequence)
                    .map(|items| items.iter().filter_map(as_int).collect())
                    .unwrap_or_default()
            });
        let isolation = mapping
            .get("isolation")
            .and_then(Value::as_mapping)
            .or_else(|| default_section("isolation"))
            .expect("built-in defaults contain orchestration.isolation");
        let choice_at = |key: &str, default: &str| -> String {
            isolation
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| default.to_owned())
        };

        let orchestrator = mapping
            .get("orchestrator")
            .and_then(Value::as_mapping)
            .or_else(|| default_section("orchestrator"));
        let orchestrator_int = |key: &str, fallback: i64| -> i64 {
            orchestrator
                .and_then(|m| m.get(key))
                .and_then(as_int)
                .or_else(|| {
                    default_section("orchestrator")
                        .and_then(|m| m.get(key))
                        .and_then(as_int)
                })
                .unwrap_or(fallback)
        };
        let roles = mapping
            .get("roles")
            .and_then(Value::as_mapping)
            .map(parse_role_rosters)
            .unwrap_or_default();

        OrchestrationSettings {
            queue_enabled: bool_at("queue_enabled"),
            max_running_total: int_at("max_running_total"),
            max_running_per_backend: cap_map_at("max_running_per_backend"),
            max_running_per_backend_model: cap_map_at("max_running_per_backend_model"),
            max_running_per_role: cap_map_at("max_running_per_role"),
            auto_restart_enabled,
            auto_restart_delays_minutes,
            designer: bot_at(designer),
            reviewer: ReviewerSettings {
                enabled: reviewer.get("enabled").and_then(as_bool).unwrap_or(false),
                backend: str_opt_at(reviewer, "backend"),
                model: str_opt_at(reviewer, "model"),
                effort: str_opt_at(reviewer, "effort"),
                agent: str_opt_at(reviewer, "agent"),
                on_changes_requested,
                max_rounds,
            },
            orchestrator: OrchestratorSettings {
                max_subtasks: orchestrator_int("max_subtasks", 12),
                upstream_budget_chars: orchestrator_int("upstream_budget_chars", 4000),
            },
            roles,
            isolation: IsolationSettings {
                mode: isolation
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(IsolationMode::parse)
                    .unwrap_or(IsolationMode::Auto),
                branch_prefix: choice_at("branch_prefix", "kanban/"),
                integration_ref: choice_at("integration_ref", "refs/kanban/integration"),
                seed: isolation
                    .get("seed")
                    .and_then(Value::as_str)
                    .and_then(IsolationSeed::parse)
                    .unwrap_or(IsolationSeed::Live),
                land: isolation
                    .get("land")
                    .and_then(Value::as_str)
                    .and_then(IsolationLand::parse)
                    .unwrap_or(IsolationLand::Worktree),
                on_conflict: isolation
                    .get("on_conflict")
                    .and_then(Value::as_str)
                    .and_then(IsolationOnConflict::parse)
                    .unwrap_or(IsolationOnConflict::Review),
                cleanup: isolation
                    .get("cleanup")
                    .and_then(Value::as_str)
                    .and_then(IsolationCleanup::parse)
                    .unwrap_or(IsolationCleanup::OnLand),
                commit_message: choice_at("commit_message", "kanban: {task_id} {title}"),
            },
        }
    }
}

impl Default for OrchestrationSettings {
    fn default() -> Self {
        Self::from_mapping(&BoardConfig::default().orchestration)
    }
}

pub struct Config {
    pub project_path: PathBuf,
    pub kanban_dir: PathBuf,
    pub config_file: PathBuf,
    cache: RefCell<Option<BoardConfig>>,
    /// Non-fatal problems found while validating the loaded config — a setting
    /// that is merely ineffective rather than wrong (see [`Self::warnings`]).
    /// Filled by the load that populated `cache`, so a cache hit keeps them.
    warnings: RefCell<Vec<String>>,
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
            warnings: RefCell::new(Vec::new()),
        }
    }

    /// Settings that loaded cleanly but will not do what they look like they
    /// do — currently a concurrency cap keyed by a backend the board does not
    /// know. These are reported rather than rejected: a cap left behind for an
    /// agent the user has since removed from `agents:` must not make an
    /// existing board unloadable, and `load` runs on every command.
    ///
    /// Empty until the config has been loaded at least once.
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.borrow().clone()
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
        let mut warnings = Vec::new();
        Self::validate(&mut config, &mut warnings)?;
        *self.warnings.borrow_mut() = warnings;
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

    fn validate(config: &mut BoardConfig, warnings: &mut Vec<String>) -> Result<()> {
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

        Self::validate_orchestration(config, warnings)?;

        Ok(())
    }

    /// Coercion and validation for the `orchestration:` section. Runs after
    /// [`Self::ensure_defaults`], so the full default structure is present.
    fn validate_orchestration(config: &mut BoardConfig, warnings: &mut Vec<String>) -> Result<()> {
        // Known backends for `<backend>/<model>` key checks: the built-ins
        // plus anything the user configured under `agents:`.
        let known_backends: Vec<String> = ["opencode", "claude", "codex", "omp", "pi"]
            .into_iter()
            .map(str::to_owned)
            .chain(
                config
                    .agents
                    .keys()
                    .filter_map(|k| k.as_str().map(str::to_owned)),
            )
            .collect();
        let orch = &mut config.orchestration;

        coerce_bool_field(orch, "queue_enabled", "orchestration")?;
        coerce_int_cap(orch, "max_running_total")?;

        validate_cap_map(orch, "max_running_per_backend", &known_backends, warnings)?;
        validate_cap_map(
            orch,
            "max_running_per_backend_model",
            &known_backends,
            warnings,
        )?;
        validate_cap_map(orch, "max_running_per_role", &known_backends, warnings)?;

        if let Some(auto_restart) = sub_mapping_mut(orch, "auto_restart") {
            coerce_bool_field(auto_restart, "enabled", "orchestration.auto_restart")?;
            validate_delays_minutes(auto_restart)?;
        }
        for role in ["designer", "reviewer"] {
            let Some(bot) = sub_mapping_mut(orch, role) else {
                continue;
            };
            coerce_bool_field(bot, "enabled", &format!("orchestration.{role}"))?;
        }
        if let Some(reviewer) = sub_mapping_mut(orch, "reviewer") {
            let yaml_key = Value::String("on_changes_requested".into());
            if let Some(value) = reviewer
                .get(&yaml_key)
                .filter(|v| !matches!(v, Value::Null))
            {
                match value.as_str() {
                    Some("in_progress") | Some("todo") => {}
                    other => {
                        return Err(KanbanError::Invalid(format!(
                            "orchestration.reviewer.on_changes_requested must be 'in_progress' or 'todo', got {other:?}"
                        )));
                    }
                }
            }
            let max_key = Value::String("max_rounds".into());
            if let Some(value) = reviewer
                .get(&max_key)
                .filter(|v| !matches!(v, Value::Null))
                .cloned()
            {
                let parsed = as_int(&value).ok_or_else(|| {
                    KanbanError::Invalid(format!(
                        "Invalid integer value for orchestration.reviewer.max_rounds: {value:?}"
                    ))
                })?;
                if parsed < 0 {
                    return Err(KanbanError::Invalid(format!(
                        "orchestration.reviewer.max_rounds must not be negative (0 means unlimited): {parsed}"
                    )));
                }
                reviewer.insert(max_key, Value::Number(parsed.into()));
            }
        }

        if let Some(orchestrator) = sub_mapping_mut(orch, "orchestrator") {
            for key in ["max_subtasks", "upstream_budget_chars"] {
                let yaml_key = Value::String(key.into());
                let Some(value) = orchestrator
                    .get(&yaml_key)
                    .filter(|v| !matches!(v, Value::Null))
                    .cloned()
                else {
                    continue;
                };
                let parsed = as_int(&value).ok_or_else(|| {
                    KanbanError::Invalid(format!(
                        "Invalid integer value for orchestration.orchestrator.{key}: {value:?}"
                    ))
                })?;
                if parsed <= 0 {
                    return Err(KanbanError::Invalid(format!(
                        "orchestration.orchestrator.{key} must be positive: {parsed}"
                    )));
                }
                orchestrator.insert(yaml_key, Value::Number(parsed.into()));
            }
        }

        validate_role_rosters(orch, &known_backends, warnings)?;
        validate_isolation(orch)?;

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
        merge_missing_deep(&mut config.orchestration, &defaults.orchestration);
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

    /// Typed snapshot of the `orchestration:` section. Business logic (queue
    /// dispatcher, crash restarts, role bots) reads this — never the raw
    /// `Mapping`.
    pub fn get_orchestration(&self) -> Result<OrchestrationSettings> {
        Ok(OrchestrationSettings::from_mapping(
            &self.load()?.orchestration,
        ))
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
