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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub tui: Mapping,
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
}
