//! Relocate a board directory into the projects store.
//!
//! One path used by `init`, `project add`, and silent adoption: rename first,
//! verified copy on `EXDEV`, source removed last.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_yaml_ng::{Mapping, Value};

use crate::core::config::Config;
use crate::core::error::{KanbanError, Result};
use crate::core::project::AddOptions;
use crate::core::session::{SessionManager, SessionState};
use crate::core::storage::{Storage, atomic_write_text};

const DEFAULT_TUI_NAME: &str = "Kanban";
const DEFAULT_HEARTBEAT_TIMEOUT: i64 = 1800;

/// True when the board at `data_root` has a live or waiting agent session.
pub fn board_has_live_sessions(data_root: &Path) -> bool {
    let timeout = heartbeat_timeout(data_root);
    SessionManager::new(data_root)
        .list_sessions_with_state(timeout)
        .into_iter()
        .any(|(_, state)| matches!(state, SessionState::Live | SessionState::Waiting))
}

/// `tui.name` from an existing board, ignoring the default `"Kanban"`.
pub fn board_display_name(data_root: &Path) -> Option<String> {
    let config = Config::new(data_root);
    if !config.exists() {
        return None;
    }
    let loaded = config.load().ok()?;
    let name = loaded.tui.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() || name == DEFAULT_TUI_NAME {
        None
    } else {
        Some(name.to_string())
    }
}

/// Set `tui.name` on an existing board so a project renamed from the projects
/// list carries the same name in its own settings, which is where the list
/// reads it back from.
///
/// The file is edited as raw YAML rather than through `Config::load`/`save`:
/// loading fills compatibility defaults, and a rename has no business writing
/// keys the board never had. A board with no config yet is left alone — the
/// registry name is what the list falls back to.
pub fn set_board_display_name(data_root: &Path, name: &str) -> Result<()> {
    let config = Config::new(data_root);
    if !config.exists() {
        return Ok(());
    }
    let _guard = Storage::new(data_root).lock()?;
    let mut document: Value = serde_yaml_ng::from_str(&fs::read_to_string(&config.config_file)?)?;
    let Value::Mapping(root) = &mut document else {
        return Ok(());
    };
    let tui = root
        .entry(Value::String("tui".to_string()))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    let Value::Mapping(tui) = tui else {
        return Ok(());
    };
    tui.insert(
        Value::String("name".to_string()),
        Value::String(name.to_string()),
    );
    atomic_write_text(&config.config_file, &serde_yaml_ng::to_string(&document)?)
}

/// Move or copy `src` (a `.kanban` directory) to `dest`.
///
/// The source is never removed until `dest` exists and matches. A failed copy
/// deletes the incomplete destination and leaves the source untouched.
pub fn relocate_board(src: &Path, dest: &Path, opts: &AddOptions) -> Result<()> {
    if dest.exists() {
        return Err(KanbanError::Invalid(format!(
            "destination already exists: {}",
            dest.display()
        )));
    }
    if opts.copy || opts.force_copy {
        copy_board_verified(src, dest)?;
        if !opts.copy {
            fs::remove_dir_all(src)?;
        }
        return Ok(());
    }
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device(&err) => {
            copy_board_verified(src, dest)?;
            fs::remove_dir_all(src)?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

fn heartbeat_timeout(data_root: &Path) -> i64 {
    let config = Config::new(data_root);
    if !config.exists() {
        return DEFAULT_HEARTBEAT_TIMEOUT;
    }
    config
        .get_threshold("session_heartbeat_timeout")
        .unwrap_or(DEFAULT_HEARTBEAT_TIMEOUT)
}

fn is_cross_device(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::CrossesDevices || err.raw_os_error() == Some(libc::EXDEV)
}

fn copy_board_verified(src: &Path, dest: &Path) -> Result<()> {
    if let Err(err) = copy_tree(src, dest) {
        let _ = fs::remove_dir_all(dest);
        return Err(err.into());
    }
    if !same_inventory(src, dest) {
        let _ = fs::remove_dir_all(dest);
        return Err(KanbanError::Invalid(format!(
            "copied board does not match the source: {}",
            src.display()
        )));
    }
    Ok(())
}

fn copy_tree(src: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(entry.path())?;
            std::os::unix::fs::symlink(target, to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

fn same_inventory(src: &Path, dest: &Path) -> bool {
    match (inventory(src), inventory(dest)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn inventory(root: &Path) -> io::Result<Vec<(PathBuf, u64)>> {
    let mut files = Vec::new();
    collect_inventory(root, root, &mut files)?;
    files.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(files)
}

fn collect_inventory(root: &Path, dir: &Path, files: &mut Vec<(PathBuf, u64)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_inventory(root, &path, files)?;
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let size = if file_type.is_symlink() {
            0
        } else {
            entry.metadata()?.len()
        };
        files.push((rel, size));
    }
    Ok(())
}
