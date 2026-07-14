use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use serde_yaml_ng::{Mapping, Value};

use crate::agent::prompt::build_agent_prompt;
use crate::core::config::{BoardConfig, Config};
use crate::core::error::{KanbanError, Result};
use crate::core::models::Task;

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
    pub agent: Option<String>,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub backend: String,
    pub task_id: String,
    pub command: String,
    pub args: Vec<String>,
    pub prompt: String,
    pub log_file: PathBuf,
    pub session_id: String,
    pub auto_complete_on_exit: bool,
}

pub fn build_launch_plan(
    project_path: &Path,
    task: &Task,
    session_id: &str,
    revert: bool,
) -> Result<LaunchPlan> {
    let config = Config::new(project_path).load()?;
    let auto_launch = auto_launch_config(&config);
    let backend = resolve_backend_name(&config, task);
    let backend_config = backend_config(&config, &backend)?;
    let model = task
        .ai_model
        .clone()
        .or_else(|| backend_config.model.clone());
    let agent = task
        .agent_name
        .clone()
        .or_else(|| backend_config.agent.clone())
        .filter(|value| !value.trim().is_empty());
    let agent = if backend == "opencode" {
        agent.map(|name| resolve_opencode_agent(&backend_config.command, &name))
    } else {
        agent
    };
    let prompt = build_agent_prompt(project_path, task, session_id, revert);
    let args = backend_args(
        &backend,
        &backend_config,
        model.as_deref(),
        agent.as_deref(),
        &prompt,
    );

    Ok(LaunchPlan {
        backend,
        task_id: task.id.clone(),
        command: backend_config.command,
        args,
        prompt,
        log_file: project_path
            .join(".kanban")
            .join("logs")
            .join(format!("{session_id}.log")),
        session_id: session_id.to_string(),
        auto_complete_on_exit: auto_launch.auto_complete_on_exit,
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
        agent: mapping_string(mapping, "agent"),
        extra_args: mapping_sequence(mapping, "extra_args"),
    })
}

fn backend_args(
    backend: &str,
    config: &AgentBackendConfig,
    model: Option<&str>,
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

fn resolve_opencode_agent(command: &str, requested: &str) -> String {
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
        cleaned
            .split_whitespace()
            .find(|token| token.to_lowercase().contains(&requested_lower))
            .map(|token| token.trim_matches('`').to_string())
            .or_else(|| Some(cleaned.to_string()))
    })
}
