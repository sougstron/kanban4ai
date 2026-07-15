use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use serde_yaml_ng::{Mapping, Value};

use crate::agent::prompt::build_agent_prompt;
use crate::core::config::{BoardConfig, Config};
use crate::core::error::{KanbanError, Result};
use crate::core::models::Task;
use crate::core::storage::atomic_write_text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoLaunchConfig {
    pub enabled: bool,
    pub use_tmux: bool,
    pub terminal_fallback: bool,
    pub auto_complete_on_exit: bool,
    pub default_agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBackendConfig {
    pub name: String,
    pub command: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub backend: String,
    pub task_id: String,
    pub command: String,
    pub model: Option<String>,
    pub args: Vec<String>,
    pub prompt: String,
    pub log_file: PathBuf,
    pub session_id: String,
    pub auto_complete_on_exit: bool,
    /// Interval for the wrapper's background heartbeat, derived from the
    /// board's `session_heartbeat_timeout` so a live agent process is never
    /// marked crashed while it works without calling heartbeat itself.
    pub heartbeat_interval_secs: i64,
    /// Requested opencode agent name whose registered form must be resolved
    /// via `opencode agent list` inside the wrapper script at run time.
    /// Resolving here would block the caller (the TUI event loop) on a
    /// multi-second opencode CLI startup; deferring it moves that wait into
    /// the spawned session. `args` carries the requested name as the
    /// `--agent` value; the wrapper substitutes the resolved one.
    pub resolve_agent: Option<String>,
}

pub fn build_launch_plan(
    project_path: &Path,
    task: &Task,
    session_id: &str,
    revert: bool,
) -> Result<LaunchPlan> {
    let loader = Config::new(project_path);
    let config = loader.load()?;
    let heartbeat_interval_secs =
        (loader.get_threshold("session_heartbeat_timeout")? / 3).clamp(10, 600);
    let auto_launch = auto_launch_config(&config);
    let backend = resolve_backend_name(&config, task);
    let backend_config = backend_config(&config, &backend)?;
    let model = task
        .ai_model
        .clone()
        .or_else(|| backend_config.model.clone());
    let effort = task
        .ai_effort
        .clone()
        .or_else(|| backend_config.effort.clone());
    let agent = task
        .agent_name
        .clone()
        .or_else(|| backend_config.agent.clone())
        .filter(|value| !value.trim().is_empty());
    let resolve_agent = if backend == "opencode" {
        agent.clone()
    } else {
        None
    };
    let prompt = build_agent_prompt(project_path, task, session_id, revert)?;
    let args = backend_args(
        &backend,
        &backend_config,
        model.as_deref(),
        effort.as_deref(),
        agent.as_deref(),
        &prompt,
    );

    Ok(LaunchPlan {
        backend,
        task_id: task.id.clone(),
        command: backend_config.command,
        model,
        args,
        prompt,
        log_file: project_path
            .join(".kanban")
            .join("logs")
            .join(format!("{session_id}.log")),
        session_id: session_id.to_string(),
        auto_complete_on_exit: auto_launch.auto_complete_on_exit,
        heartbeat_interval_secs,
        resolve_agent,
    })
}

pub fn auto_launch_config(config: &BoardConfig) -> AutoLaunchConfig {
    AutoLaunchConfig {
        enabled: mapping_bool(&config.auto_launch, "enabled", true),
        use_tmux: mapping_bool(&config.auto_launch, "use_tmux", true),
        terminal_fallback: mapping_bool(&config.auto_launch, "terminal_fallback", true),
        auto_complete_on_exit: mapping_bool(&config.auto_launch, "auto_complete_on_exit", false),
        default_agent: mapping_string(&config.auto_launch, "default_agent")
            .unwrap_or_else(|| "opencode".to_string()),
    }
}

pub fn resolve_backend_name(config: &BoardConfig, task: &Task) -> String {
    let requested = task
        .agent_backend
        .clone()
        .unwrap_or_else(|| auto_launch_config(config).default_agent);
    if config.agents.contains_key(requested.as_str()) {
        requested
    } else {
        "opencode".to_string()
    }
}

pub fn backend_config(config: &BoardConfig, name: &str) -> Result<AgentBackendConfig> {
    let value = config
        .agents
        .get(name)
        .or_else(|| config.agents.get("opencode"))
        .ok_or_else(|| KanbanError::Invalid("missing opencode agent backend config".to_string()))?;
    let mapping = value
        .as_mapping()
        .ok_or_else(|| KanbanError::Invalid(format!("agent backend '{name}' must be a mapping")))?;
    Ok(AgentBackendConfig {
        name: name.to_string(),
        command: mapping_string(mapping, "command").unwrap_or_else(|| name.to_string()),
        model: mapping_string(mapping, "model"),
        effort: mapping_string(mapping, "effort"),
        agent: mapping_string(mapping, "agent"),
        extra_args: mapping_sequence(mapping, "extra_args"),
    })
}

fn backend_args(
    backend: &str,
    config: &AgentBackendConfig,
    model: Option<&str>,
    effort: Option<&str>,
    agent: Option<&str>,
    prompt: &str,
) -> Vec<String> {
    let mut args = match backend {
        "claude" => vec!["--print".to_string()],
        _ => vec!["run".to_string()],
    };
    args.extend(config.extra_args.clone());
    if let Some(model) = model.filter(|value| !value.trim().is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(effort) = effort.filter(|value| !value.trim().is_empty()) {
        // claude exposes reasoning effort as --effort; opencode maps it onto
        // per-model variants selected with --variant.
        let flag = if backend == "claude" {
            "--effort"
        } else {
            "--variant"
        };
        args.push(flag.to_string());
        args.push(effort.to_string());
    }
    if backend == "opencode"
        && let Some(agent) = agent.filter(|value| !value.trim().is_empty())
    {
        args.push("--agent".to_string());
        args.push(agent.to_string());
    }
    if backend == "opencode" {
        args.push("--title".to_string());
        args.push(prompt_title(prompt));
    }
    args.push(prompt.to_string());
    args
}

fn prompt_title(prompt: &str) -> String {
    prompt
        .lines()
        .find_map(|line| line.strip_prefix("Task: "))
        .unwrap_or("kanban task")
        .chars()
        .take(80)
        .collect()
}

fn mapping_bool(mapping: &Mapping, key: &str, default: bool) -> bool {
    mapping.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn mapping_string(mapping: &Mapping, key: &str) -> Option<String> {
    mapping
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn mapping_sequence(mapping: &Mapping, key: &str) -> Vec<String> {
    mapping
        .get(key)
        .and_then(Value::as_sequence)
        .map(|seq| {
            seq.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

static OPENCODE_AGENT_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// Match a configured agent name against `opencode agent list` and return
/// the registered form `--agent` expects, falling back to the requested name
/// when the CLI is unavailable or lists no match. Starting the opencode CLI
/// takes seconds, so this runs from the wrapper script's hidden
/// `resolve-agent` callback inside the spawned session — never on the
/// launching (TUI) side.
pub fn resolve_opencode_agent(command: &str, requested: &str) -> String {
    let key = format!("{command}\n{requested}");
    let cache = OPENCODE_AGENT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .ok()
        .and_then(|values| values.get(&key).cloned())
    {
        return cached;
    }

    let resolved = Command::new(command)
        .args(["agent", "list"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| parse_opencode_agent_list(&text, requested))
        .unwrap_or_else(|| requested.to_string());

    if let Ok(mut values) = cache.lock() {
        values.insert(key, resolved.clone());
    }
    resolved
}

/// Model catalog reported by the opencode CLI: every launchable
/// `provider/model` id plus the reasoning-effort variants each model accepts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpencodeCatalog {
    pub models: Vec<String>,
    variants: HashMap<String, Vec<String>>,
}

impl OpencodeCatalog {
    pub fn variants_for(&self, model: &str) -> &[String] {
        self.variants
            .get(model)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

static OPENCODE_CATALOG_CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<OpencodeCatalog>>>>> =
    OnceLock::new();

/// Model list and variants from `opencode models --verbose`, cached per
/// command for the process lifetime. `None` when the CLI is unavailable —
/// callers fall back to the configured `models` list.
pub fn opencode_catalog(command: &str) -> Option<Arc<OpencodeCatalog>> {
    let cache = OPENCODE_CATALOG_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .ok()
        .and_then(|values| values.get(command).cloned())
    {
        return cached;
    }

    let fetched = Command::new(command)
        .args(["models", "--verbose"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| parse_opencode_models_verbose(&text))
        .filter(|catalog| !catalog.models.is_empty())
        .map(Arc::new);

    if let Ok(mut values) = cache.lock() {
        values.insert(command.to_string(), fetched.clone());
    }
    fetched
}

/// Return a previously fetched opencode catalog without invoking the CLI.
///
/// The TUI uses this on latency-sensitive paths (opening dialogs) so a cold
/// `opencode models --verbose` subprocess cannot block the event loop.
pub fn cached_opencode_catalog(command: &str) -> Option<Arc<OpencodeCatalog>> {
    OPENCODE_CATALOG_CACHE
        .get()
        .and_then(|cache| cache.lock().ok())
        .and_then(|values| values.get(command).cloned().flatten())
}

/// Parse `opencode models --verbose`: each entry is a `provider/model` header
/// line followed by a pretty-printed JSON blob whose closing brace sits at
/// column zero. The `variants` object keys are the model's valid efforts.
pub fn parse_opencode_models_verbose(text: &str) -> OpencodeCatalog {
    let mut catalog = OpencodeCatalog::default();
    let mut current: Option<String> = None;
    let mut json = String::new();
    let mut in_json = false;
    for line in text.lines() {
        if in_json {
            json.push_str(line);
            json.push('\n');
            if line == "}" {
                in_json = false;
                if let (Some(model), Ok(value)) = (
                    current.take(),
                    serde_json::from_str::<serde_json::Value>(&json),
                ) {
                    let efforts = value
                        .get("variants")
                        .and_then(serde_json::Value::as_object)
                        .map(|variants| variants.keys().cloned().collect())
                        .unwrap_or_default();
                    catalog.variants.insert(model, sort_efforts(efforts));
                }
            }
        } else if line == "{" && current.is_some() {
            in_json = true;
            json.clear();
            json.push_str("{\n");
        } else if is_model_header(line) {
            current = Some(line.to_string());
            catalog.models.push(line.to_string());
        }
    }
    catalog
}

fn is_model_header(line: &str) -> bool {
    !line.is_empty() && line.contains('/') && !line.chars().any(char::is_whitespace)
}

/// Reasoning efforts ordered weakest to strongest; unknown names go last,
/// alphabetically.
const EFFORT_ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

pub fn sort_efforts(mut efforts: Vec<String>) -> Vec<String> {
    let rank = |effort: &String| {
        let lower = effort.to_lowercase();
        (
            EFFORT_ORDER
                .iter()
                .position(|known| *known == lower)
                .unwrap_or(EFFORT_ORDER.len()),
            lower,
        )
    };
    efforts.sort_by_key(rank);
    efforts.dedup();
    efforts
}

/// Selector order for opencode models: the project default first, then up to
/// three most recently used models, then the rest alphabetically.
pub fn sort_opencode_models(
    models: &[String],
    default_model: Option<&str>,
    recent: &[String],
) -> Vec<String> {
    let default_model = default_model.map(str::trim).filter(|d| !d.is_empty());
    let mut ordered: Vec<String> = Vec::new();
    if let Some(default) = default_model {
        ordered.push(default.to_string());
    }
    let known: HashSet<&str> = models.iter().map(String::as_str).collect();
    let mut recent_used = 0;
    for model in recent {
        if recent_used == 3 {
            break;
        }
        if !known.contains(model.as_str()) || ordered.iter().any(|existing| existing == model) {
            continue;
        }
        ordered.push(model.clone());
        recent_used += 1;
    }
    let mut rest: Vec<String> = models
        .iter()
        .filter(|model| !ordered.iter().any(|existing| existing == *model))
        .cloned()
        .collect();
    rest.sort();
    rest.dedup();
    ordered.extend(rest);
    ordered
}

const RECENT_MODELS_LIMIT: usize = 10;

fn recent_models_file(project_path: &Path) -> PathBuf {
    project_path.join(".kanban").join("recent_models")
}

/// Most-recently-used opencode models, newest first (`.kanban/recent_models`).
pub fn recent_models(project_path: &Path) -> Vec<String> {
    fs::read_to_string(recent_models_file(project_path))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Move `model` to the top of the recent-models history. Failures are ignored:
/// the history only affects selector ordering.
pub fn record_recent_model(project_path: &Path, model: &str) {
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    let mut models = recent_models(project_path);
    models.retain(|existing| existing != model);
    models.insert(0, model.to_string());
    models.truncate(RECENT_MODELS_LIMIT);
    let _ = atomic_write_text(
        &recent_models_file(project_path),
        &(models.join("\n") + "\n"),
    );
}

pub fn parse_opencode_agent_list(text: &str, requested: &str) -> Option<String> {
    let requested_lower = requested.to_lowercase();
    text.lines().find_map(|line| {
        let cleaned = line
            .trim()
            .trim_start_matches(['-', '*', '•'])
            .trim()
            .trim_matches('`')
            .trim();
        if cleaned.is_empty() || !cleaned.to_lowercase().contains(&requested_lower) {
            return None;
        }
        // `opencode agent list` prints the registered name verbatim followed
        // by a mode marker, e.g. "Hephaestus - Deep Agent (primary)". The
        // full name — which may contain spaces and invisible ordering
        // characters — is what `--agent` expects, so only the marker is
        // dropped; picking a single whitespace token here produces a name
        // opencode rejects, silently falling back to its default agent.
        if let Some(name) = strip_agent_mode_suffix(cleaned) {
            return Some(name.to_string());
        }
        cleaned
            .split_whitespace()
            .find(|token| token.to_lowercase().contains(&requested_lower))
            .map(|token| token.trim_matches('`').to_string())
            .or_else(|| Some(cleaned.to_string()))
    })
}

fn strip_agent_mode_suffix(line: &str) -> Option<&str> {
    let rest = line.strip_suffix(')')?;
    let (name, mode) = rest.rsplit_once('(')?;
    matches!(mode.trim(), "primary" | "subagent" | "all").then(|| name.trim_end())
}
