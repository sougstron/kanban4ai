//! The claude statusline bridge.
//!
//! Claude Code (>= 2.1.80) pipes `rate_limits` to the configured statusLine
//! command on every turn, piggybacked on Messages API responses. The usage
//! endpoint the claude limits row used to poll is instead rate-limited to a
//! handful of requests per OAuth access token and answers 429 for hours, so
//! the bridge is the live source: `kanban limits bridge install` wraps the
//! existing statusline command with a generated shim that tees the payload
//! into `<store>/claude-rate-limits.json`, and the hidden
//! `kanban statusline-bridge` performs the recording.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use clap::Subcommand;
use serde_json::{Value, json};

use crate::core::error::{KanbanError, Result};
use crate::core::limits;
use crate::core::project::store_root;
use crate::core::storage::atomic_write_text;

#[derive(Subcommand)]
pub enum LimitsBridge {
    /// Manage the Claude Code statusline bridge feeding the claude segment
    Bridge {
        #[command(subcommand)]
        action: BridgeAction,
    },
}

#[derive(Subcommand)]
pub enum BridgeAction {
    /// Wrap Claude Code's statusline command with the rate-limits bridge
    Install,
    /// Restore the statusline command the bridge replaced
    Remove,
}

const WRAPPER_NAME: &str = "claude-statusline-bridge.sh";
const ORIGINAL_NAME: &str = "claude-statusline-bridge.original";
const SETTINGS_BACKUP_NAME: &str = "settings.json.kanban4ai-bak";

/// `kanban statusline-bridge`: record the `rate_limits` of a statusline stdin
/// payload. Prints nothing and always succeeds — Claude Code renders the
/// command's stdout as the status line, and the bridge must never break it.
pub fn statusline_bridge(input: &mut impl Read) {
    let mut text = String::new();
    if input.read_to_string(&mut text).is_err() {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let windows = limits::parse_claude_statusline(&value);
    if !windows.is_empty() {
        limits::store_claude_bridge(&windows);
    }
}

fn claude_settings_path() -> Option<PathBuf> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))?;
    Some(dir.join("settings.json"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn read_settings(path: &PathBuf) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|err| {
            KanbanError::Invalid(format!("{} is not valid JSON: {err}", path.display()))
        }),
        Err(_) => Ok(json!({})),
    }
}

/// The statusline command currently configured, `None` when there is no
/// usable one. Refuses statusLine entries that are not `type: command`.
fn configured_statusline(settings: &Value) -> Result<Option<String>> {
    let Some(entry) = settings.get("statusLine") else {
        return Ok(None);
    };
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    if kind != "command" {
        return Err(KanbanError::Invalid(format!(
            "unsupported Claude Code statusLine type {kind:?}: only \"command\" can be bridged"
        )));
    }
    Ok(entry
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string))
}

pub fn bridge_install() -> Result<()> {
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .map_err(|err| KanbanError::Invalid(format!("cannot resolve this binary's path: {err}")))?;
    let Some(settings_path) = claude_settings_path() else {
        return Err(KanbanError::Invalid(
            "no HOME or CLAUDE_CONFIG_DIR to locate Claude Code settings".to_string(),
        ));
    };
    let store = store_root()?;
    fs::create_dir_all(&store)?;
    let wrapper_path = store.join(WRAPPER_NAME);
    let original_path = store.join(ORIGINAL_NAME);

    let mut settings = read_settings(&settings_path)?;
    if !settings.is_object() {
        return Err(KanbanError::Invalid(format!(
            "{} does not hold a JSON object",
            settings_path.display()
        )));
    }
    let current = configured_statusline(&settings)?.unwrap_or_default();
    let original = if current.contains("statusline-bridge") {
        // Already wrapped: the sidecar keeps the pre-bridge command.
        fs::read_to_string(&original_path)
            .map_err(|err| {
                KanbanError::Invalid(format!(
                    "bridge is installed but {} is unreadable ({err}); restore the statusLine \
                     entry in {} by hand before reinstalling",
                    original_path.display(),
                    settings_path.display()
                ))
            })?
            .trim_end()
            .to_string()
    } else {
        current
    };

    let backup = settings_path.with_file_name(SETTINGS_BACKUP_NAME);
    if settings_path.exists() && !backup.exists() {
        fs::copy(&settings_path, &backup)?;
    }

    let mut script = String::from(
        "#!/bin/sh\n# Managed by kanban4ai: records the rate_limits Claude Code pipes here\n\
         # into the board's claude limits row. Restore with: kanban limits bridge remove\n\
         payload=$(cat)\n",
    );
    script.push_str(&format!(
        "printf '%s' \"$payload\" | {} statusline-bridge >/dev/null 2>&1\n",
        shell_quote(&exe.display().to_string())
    ));
    if !original.is_empty() {
        script.push_str(&format!("printf '%s' \"$payload\" | {original}\n"));
    }
    atomic_write_text(&wrapper_path, &script)?;
    fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755))?;
    fs::write(&original_path, format!("{original}\n"))?;

    // Keep any sibling statusLine keys (refreshInterval, padding, …): only
    // the command is swapped.
    let statusline = settings
        .as_object_mut()
        .expect("read_settings holds an object")
        .entry("statusLine")
        .or_insert_with(|| json!({}));
    if !statusline.is_object() {
        return Err(KanbanError::Invalid(
            "the statusLine entry in Claude Code settings is not an object".to_string(),
        ));
    }
    statusline["type"] = json!("command");
    statusline["command"] = json!(format!(
        "sh {}",
        shell_quote(&wrapper_path.display().to_string())
    ));
    atomic_write_text(
        &settings_path,
        &serde_json::to_string_pretty(&settings)
            .map_err(|err| KanbanError::Invalid(format!("cannot serialize settings: {err}")))?,
    )?;

    if original.is_empty() {
        println!(
            "Installed the claude statusline bridge into {}",
            settings_path.display()
        );
    } else {
        println!(
            "Installed the claude statusline bridge into {} (wrapping: {original})",
            settings_path.display()
        );
    }
    println!("The claude limits row fills in from your next interactive Claude Code session.");
    Ok(())
}

pub fn bridge_remove() -> Result<()> {
    let Some(settings_path) = claude_settings_path() else {
        return Err(KanbanError::Invalid(
            "no HOME or CLAUDE_CONFIG_DIR to locate Claude Code settings".to_string(),
        ));
    };
    let store = store_root()?;
    let wrapper_path = store.join(WRAPPER_NAME);
    let original_path = store.join(ORIGINAL_NAME);
    let original = fs::read_to_string(&original_path)
        .unwrap_or_default()
        .trim_end()
        .to_string();

    let mut removed_settings = false;
    if settings_path.exists() {
        let mut settings = read_settings(&settings_path)?;
        let wrapped = configured_statusline(&settings)?
            .is_some_and(|command| command.contains("statusline-bridge"));
        if wrapped {
            if original.is_empty() {
                settings
                    .as_object_mut()
                    .expect("read_settings holds an object")
                    .remove("statusLine");
            } else {
                let statusline = settings
                    .as_object_mut()
                    .expect("read_settings holds an object")
                    .get_mut("statusLine")
                    .expect("configured_statusline found the entry");
                statusline["command"] = json!(original);
            }
            atomic_write_text(
                &settings_path,
                &serde_json::to_string_pretty(&settings).map_err(|err| {
                    KanbanError::Invalid(format!("cannot serialize settings: {err}"))
                })?,
            )?;
            let _ = fs::remove_file(settings_path.with_file_name(SETTINGS_BACKUP_NAME));
            removed_settings = true;
        }
    }
    let _ = fs::remove_file(&wrapper_path);
    let _ = fs::remove_file(&original_path);

    if removed_settings {
        if original.is_empty() {
            println!(
                "Removed the claude statusline bridge from {}",
                settings_path.display()
            );
        } else {
            println!(
                "Removed the claude statusline bridge from {} (restored: {original})",
                settings_path.display()
            );
        }
    } else {
        println!("The claude statusline bridge is not installed.");
    }
    Ok(())
}
