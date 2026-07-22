use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

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

/// The backend, model, effort, and agent persona a launch actually uses:
/// the task's own fields where set, the backend's configured defaults
/// otherwise. [`Operations`](crate::core::operations::Operations) writes these
/// back onto the task so its fields keep describing the last session that ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSettings {
    pub backend: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
}

pub fn resolve_launch_settings(config: &BoardConfig, task: &Task) -> Result<LaunchSettings> {
    let backend = resolve_backend_name(config, task);
    let backend_config = backend_config(config, &backend)?;
    let pick = |task_value: &Option<String>, default: Option<String>| {
        task_value
            .clone()
            .or(default)
            .filter(|value| !value.trim().is_empty())
    };
    Ok(LaunchSettings {
        model: pick(&task.ai_model, backend_config.model),
        effort: pick(&task.ai_effort, backend_config.effort),
        agent: pick(&task.agent_name, backend_config.agent),
        backend,
    })
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
    /// Machine transcript of the run (claude `--output-format stream-json`
    /// JSONL), harvested at exit into an input-provenance manifest. `None` for
    /// backends without a parseable transcript, whose stdout is teed straight
    /// to `log_file` unchanged.
    pub transcript_file: Option<PathBuf>,
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
    let LaunchSettings {
        backend,
        model,
        effort,
        agent,
    } = resolve_launch_settings(&config, task)?;
    let backend_config = backend_config(&config, &backend)?;
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

    let logs_dir = project_path.join(".kanban").join("logs");
    // Both claude and opencode emit a parseable JSONL transcript on stdout.
    let transcript_file = matches!(backend.as_str(), "claude" | "opencode")
        .then(|| logs_dir.join(format!("{session_id}.transcript.jsonl")));

    Ok(LaunchPlan {
        backend,
        task_id: task.id.clone(),
        command: backend_config.command,
        model,
        args,
        prompt,
        log_file: logs_dir.join(format!("{session_id}.log")),
        transcript_file,
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
        // stream-json + verbose makes claude emit one JSON event per line
        // (init metadata, assistant text, tool_use, result) so the run's real
        // inputs can be harvested; the wrapper reformats it back to human text
        // for the log via `kanban format-stream`.
        "claude" => vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ],
        // opencode emits one JSON event per line on stdout with `--format json`
        // (tool_use/text/step_*), the same capture-and-reformat contract as
        // claude, so its real inputs can be harvested too.
        "opencode" => vec![
            "run".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ],
        // omp/pi (the "pi" agent family) run non-interactively with `-p` and
        // take the prompt as a positional argument. They emit no parseable
        // transcript, so their stdout is teed to the log unchanged.
        "omp" | "pi" => vec!["-p".to_string()],
        _ => vec!["run".to_string()],
    };
    args.extend(config.extra_args.clone());
    if let Some(model) = model.filter(|value| !value.trim().is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(effort) = effort.filter(|value| !value.trim().is_empty()) {
        // claude exposes reasoning effort as --effort; opencode maps it onto
        // per-model variants selected with --variant; the pi family (omp/pi)
        // uses --thinking.
        let flag = match backend {
            "claude" => "--effort",
            "omp" | "pi" => "--thinking",
            _ => "--variant",
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

/// Model catalog reported by an agent backend: every launchable model id plus
/// the reasoning-effort variants each model accepts. Sourced from the backend
/// CLI (`opencode models --verbose`, `omp models --json`) or, for pi, its
/// on-disk `models-store.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendCatalog {
    pub models: Vec<String>,
    variants: HashMap<String, Vec<String>>,
}

impl BackendCatalog {
    pub fn variants_for(&self, model: &str) -> &[String] {
        self.variants
            .get(model)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

/// Backends whose model/effort catalog kanban polls (from the CLI, or from pi's
/// on-disk store) instead of relying solely on the configured `models` list.
pub fn backend_has_catalog(backend: &str) -> bool {
    matches!(backend, "opencode" | "omp" | "pi")
}

static CATALOG_CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<BackendCatalog>>>>> =
    OnceLock::new();
static CATALOG_WARMING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn catalog_cache_key(backend: &str, command: &str) -> String {
    format!("{backend}\u{0}{command}")
}

/// Model catalog for a backend, cached per backend+command for the process
/// lifetime. `None` when the backend has no catalog, its CLI/store is
/// unavailable, or no models were parsed — callers then fall back to the
/// configured `models` list.
pub fn backend_catalog(backend: &str, command: &str) -> Option<Arc<BackendCatalog>> {
    if !backend_has_catalog(backend) {
        return None;
    }
    let cache = CATALOG_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = catalog_cache_key(backend, command);
    if let Some(cached) = cache
        .lock()
        .ok()
        .and_then(|values| values.get(&key).cloned())
    {
        return cached;
    }

    let fetched = fetch_backend_catalog(backend, command)
        .filter(|catalog| !catalog.models.is_empty())
        .map(Arc::new);

    if let Ok(mut values) = cache.lock() {
        values.insert(key, fetched.clone());
    }
    fetched
}

fn fetch_backend_catalog(backend: &str, command: &str) -> Option<BackendCatalog> {
    match backend {
        "opencode" => run_capture(command, &["models", "--verbose"])
            .map(|t| parse_opencode_models_verbose(&t)),
        "omp" => run_capture(command, &["models", "--json"]).map(|t| parse_omp_models_json(&t)),
        // pi has no models subcommand; it maintains the catalog on disk.
        "pi" => fs::read_to_string(pi_models_store_path())
            .ok()
            .map(|t| parse_pi_models_store(&t)),
        _ => None,
    }
}

fn run_capture(command: &str, args: &[&str]) -> Option<String> {
    Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
}

/// Path to pi's on-disk model catalog. pi has no models subcommand, so its
/// catalog is read from the agent config directory (`PI_CODING_AGENT_DIR`,
/// default `~/.pi/agent`).
fn pi_models_store_path() -> PathBuf {
    let dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pi").join("agent"))
        })
        .unwrap_or_else(|| PathBuf::from(".pi").join("agent"));
    dir.join("models-store.json")
}

/// Start a best-effort background fetch of a backend's model catalog.
///
/// This keeps latency-sensitive UI paths on `cached_backend_catalog()` while
/// making the live catalog available shortly after startup.
pub fn warm_backend_catalog(backend: String, command: String) {
    if command.trim().is_empty()
        || !backend_has_catalog(&backend)
        || cached_backend_catalog(&backend, &command).is_some()
    {
        return;
    }
    let warming = CATALOG_WARMING.get_or_init(|| Mutex::new(HashSet::new()));
    let key = catalog_cache_key(&backend, &command);
    if let Ok(mut keys) = warming.lock() {
        if !keys.insert(key.clone()) {
            return;
        }
    } else {
        return;
    }

    thread::spawn(move || {
        let _ = backend_catalog(&backend, &command);
        if let Some(warming) = CATALOG_WARMING.get()
            && let Ok(mut keys) = warming.lock()
        {
            keys.remove(&catalog_cache_key(&backend, &command));
        }
    });
}

/// Return a previously fetched catalog without invoking the CLI or touching
/// disk. The TUI uses this on latency-sensitive paths (opening dialogs) so a
/// cold catalog fetch cannot block the event loop.
pub fn cached_backend_catalog(backend: &str, command: &str) -> Option<Arc<BackendCatalog>> {
    CATALOG_CACHE
        .get()
        .and_then(|cache| cache.lock().ok())
        .and_then(|values| {
            values
                .get(&catalog_cache_key(backend, command))
                .cloned()
                .flatten()
        })
}

/// Opencode-specific catalog accessors retained for the CLI's opencode paths
/// and existing tests; each delegates to the generic backend catalog.
pub fn opencode_catalog(command: &str) -> Option<Arc<BackendCatalog>> {
    backend_catalog("opencode", command)
}

pub fn warm_opencode_catalog(command: String) {
    warm_backend_catalog("opencode".to_string(), command);
}

pub fn cached_opencode_catalog(command: &str) -> Option<Arc<BackendCatalog>> {
    cached_backend_catalog("opencode", command)
}

/// Parse `opencode models --verbose`: each entry is a `provider/model` header
/// line followed by a pretty-printed JSON blob whose closing brace sits at
/// column zero. The `variants` object keys are the model's valid efforts.
pub fn parse_opencode_models_verbose(text: &str) -> BackendCatalog {
    let mut catalog = BackendCatalog::default();
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

/// Parse `omp models --json`: `{ "models": [ { "selector": "provider/id",
/// "thinking": ["low", ...] | null }, ... ] }`. The `thinking` array lists the
/// model's valid reasoning efforts.
pub fn parse_omp_models_json(text: &str) -> BackendCatalog {
    let mut catalog = BackendCatalog::default();
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return catalog;
    };
    let Some(models) = root.get("models").and_then(serde_json::Value::as_array) else {
        return catalog;
    };
    for model in models {
        let Some(selector) = model
            .get("selector")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let efforts = model
            .get("thinking")
            .and_then(serde_json::Value::as_array)
            .map(|arr| json_string_array(arr))
            .unwrap_or_default();
        catalog.models.push(selector.to_string());
        catalog
            .variants
            .insert(selector.to_string(), sort_efforts(efforts));
    }
    catalog
}

/// Parse pi's `models-store.json`: an object keyed by provider, each holding a
/// `models` array of `{ id, provider, thinkingLevelMap?: {..}, thinking?: [..] }`.
/// The launchable selector is `provider/id`; a model's efforts are the
/// `thinkingLevelMap` keys (or `thinking` array) it accepts.
pub fn parse_pi_models_store(text: &str) -> BackendCatalog {
    let mut catalog = BackendCatalog::default();
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return catalog;
    };
    let Some(providers) = root.as_object() else {
        return catalog;
    };
    for (provider_key, entry) in providers {
        let Some(models) = entry.get("models").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for model in models {
            let Some(id) = model
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let provider = model
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(provider_key.as_str());
            let selector = format!("{provider}/{id}");
            let efforts = model
                .get("thinkingLevelMap")
                .and_then(serde_json::Value::as_object)
                .map(|map| map.keys().cloned().collect::<Vec<_>>())
                .or_else(|| {
                    model
                        .get("thinking")
                        .and_then(serde_json::Value::as_array)
                        .map(|arr| json_string_array(arr))
                })
                .unwrap_or_default();
            if catalog
                .variants
                .insert(selector.clone(), sort_efforts(efforts))
                .is_none()
            {
                catalog.models.push(selector);
            }
        }
    }
    catalog
}

fn json_string_array(values: &[serde_json::Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// Reasoning efforts ordered weakest to strongest; unknown names go last,
/// alphabetically.
const EFFORT_ORDER: [&str; 8] = [
    "off", "none", "minimal", "low", "medium", "high", "xhigh", "max",
];

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
