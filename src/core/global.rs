//! Machine-wide settings at the store root (`<store>/config.yaml`).
//!
//! Unlike the per-project `.kanban/config.yaml` this file is shared by every
//! board on the machine, so the Projects screen — which has no board context —
//! is where it is edited. Like [`crate::core::config::BoardConfig`] the file is
//! kept as a loose YAML mapping so keys this version does not know survive a
//! load/save round trip.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

use super::config::as_bool;
use super::error::{KanbanError, Result};
use super::project::ProjectStore;
use super::storage::atomic_write_text;

/// The settings file directly under the store root.
pub const GLOBAL_CONFIG_FILE: &str = "config.yaml";

/// Projects-list ordering: alphabetical by display name (the default).
pub const PROJECT_SORT_NAME: &str = "name";
/// Projects-list ordering: newest project first.
pub const PROJECT_SORT_NEWEST: &str = "newest";
/// Projects-list ordering: unread rows first, then rows with running agents,
/// then newest first.
pub const PROJECT_SORT_SMART: &str = "smart";

/// Default `kanban daemon` tick interval when the store config omits one.
pub const DEFAULT_DAEMON_INTERVAL_SECS: u64 = 60;

/// Map any stored value onto a known ordering; unknown values read as `name`.
pub fn normalize_project_sort(value: &str) -> &'static str {
    match value {
        PROJECT_SORT_NEWEST => PROJECT_SORT_NEWEST,
        PROJECT_SORT_SMART => PROJECT_SORT_SMART,
        _ => PROJECT_SORT_NAME,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub tui: Mapping,
    /// Cross-project daemon cadence. Everything else in the orchestration
    /// plan stays per-project; the daemon spans boards, so its interval
    /// lives here (`daemon.interval`, seconds, default 60).
    #[serde(default, skip_serializing_if = "Mapping::is_empty")]
    pub daemon: Mapping,
    #[serde(flatten, default)]
    pub extras: Mapping,
}

impl GlobalConfig {
    /// Whatever its storage shape, the effective default is off.
    pub fn escape_to_projects(&self) -> bool {
        self.tui
            .get(Value::String("escape_to_projects".to_string()))
            .and_then(as_bool)
            .unwrap_or(false)
    }

    /// Command the Projects screen hands a work folder to. Unset (the usual
    /// case) means the platform default chain in [`crate::core::opener`]; a
    /// value here is for desktops where that picks the wrong application.
    pub fn file_manager(&self) -> Option<String> {
        self.tui
            .get(Value::String("file_manager".to_string()))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_string)
    }

    pub fn set_escape_to_projects(&mut self, enabled: bool) {
        self.tui.insert(
            Value::String("escape_to_projects".to_string()),
            Value::Bool(enabled),
        );
    }

    /// How the Projects screen orders its rows. Whatever its storage shape,
    /// the effective default is `name`.
    pub fn project_sort(&self) -> &'static str {
        self.tui
            .get(Value::String("project_sort".to_string()))
            .and_then(Value::as_str)
            .map(normalize_project_sort)
            .unwrap_or(PROJECT_SORT_NAME)
    }

    pub fn set_project_sort(&mut self, value: &str) {
        self.tui.insert(
            Value::String("project_sort".to_string()),
            Value::String(normalize_project_sort(value).to_string()),
        );
    }

    /// Seconds between daemon ticks. Missing, zero, or unparseable values
    /// read as 60 so a hand-edited file still yields a usable cadence.
    pub fn daemon_interval(&self) -> u64 {
        self.daemon
            .get(Value::String("interval".to_string()))
            .and_then(positive_u64)
            .unwrap_or(DEFAULT_DAEMON_INTERVAL_SECS)
    }
}

fn positive_u64(value: &Value) -> Option<u64> {
    let parsed = match value {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok())),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    parsed.filter(|n| *n > 0)
}

impl ProjectStore {
    pub fn global_config_path(&self) -> PathBuf {
        self.root().join(GLOBAL_CONFIG_FILE)
    }

    /// Load the machine-wide settings; a missing file yields defaults.
    pub fn load_global_config(&self) -> Result<GlobalConfig> {
        match fs::read_to_string(self.global_config_path()) {
            Ok(raw) => {
                if raw.trim().is_empty() {
                    return Ok(GlobalConfig::default());
                }
                Ok(serde_yaml_ng::from_str(&raw)?)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(GlobalConfig::default()),
            Err(err) => Err(err.into()),
        }
    }

    /// Persist machine-wide settings atomically. Callers that merge into the
    /// on-disk state hold [`ProjectStore::lock`] across the read-modify-write.
    pub fn save_global_config(&self, config: &GlobalConfig) -> Result<()> {
        let path = self.global_config_path();
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && metadata.file_type().is_symlink()
        {
            return Err(KanbanError::Permission(
                "Refusing to save through symlinked global config".into(),
            ));
        }
        fs::create_dir_all(self.root())?;
        atomic_write_text(&path, &serde_yaml_ng::to_string(config)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().expect("store");
        let store = ProjectStore::at(dir.path());
        let config = store.load_global_config().expect("load defaults");
        assert!(!config.escape_to_projects());
        assert!(!store.global_config_path().exists());
    }

    #[test]
    fn save_load_round_trip_and_unknown_keys_survive() {
        let dir = tempfile::tempdir().expect("store");
        let store = ProjectStore::at(dir.path());
        std::fs::write(
            store.global_config_path(),
            "tui:\n  escape_to_projects: false\nfuture_section:\n  keep: me\n",
        )
        .expect("seed config");

        let mut config = store.load_global_config().expect("load");
        config.set_escape_to_projects(true);
        store.save_global_config(&config).expect("save");

        let reloaded = store.load_global_config().expect("reload");
        assert!(reloaded.escape_to_projects());
        let raw = std::fs::read_to_string(store.global_config_path()).expect("raw");
        assert!(
            raw.contains("future_section") && raw.contains("keep: me"),
            "unknown keys must survive save: {raw}"
        );
    }

    #[test]
    fn project_sort_round_trip_and_normalization() {
        let dir = tempfile::tempdir().expect("store");
        let store = ProjectStore::at(dir.path());
        assert_eq!(store.load_global_config().unwrap().project_sort(), "name");

        let mut config = store.load_global_config().unwrap();
        config.set_project_sort("smart");
        store.save_global_config(&config).expect("save");
        assert_eq!(store.load_global_config().unwrap().project_sort(), "smart");

        // Unknown values normalize to the default rather than erroring, so a
        // hand-edited file still yields a usable ordering.
        let mut config = store.load_global_config().unwrap();
        config.set_project_sort("bogus");
        store.save_global_config(&config).expect("save");
        assert_eq!(store.load_global_config().unwrap().project_sort(), "name");
    }

    #[test]
    fn daemon_interval_defaults_and_rejects_zero() {
        let dir = tempfile::tempdir().expect("store");
        let store = ProjectStore::at(dir.path());
        assert_eq!(
            store.load_global_config().unwrap().daemon_interval(),
            DEFAULT_DAEMON_INTERVAL_SECS
        );

        std::fs::write(store.global_config_path(), "daemon:\n  interval: 15\n").expect("seed");
        assert_eq!(store.load_global_config().unwrap().daemon_interval(), 15);

        std::fs::write(store.global_config_path(), "daemon:\n  interval: 0\n").expect("seed");
        assert_eq!(
            store.load_global_config().unwrap().daemon_interval(),
            DEFAULT_DAEMON_INTERVAL_SECS
        );
    }
}
