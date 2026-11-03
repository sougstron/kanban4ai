//! Handing a folder to the desktop's own file manager.
//!
//! The TUI owns the terminal, so the opener is spawned detached with its
//! streams closed: a file manager that logs to stderr must not scribble over
//! the drawn frame, and the board must never block on a GUI that stays alive
//! for as long as the user keeps its window open.

use std::path::Path;
use std::process::{Command, Stdio};

use super::error::{KanbanError, Result};
use super::notifier::which;

/// Openers tried in order, first one on PATH wins. `xdg-open` and `gio` cover
/// any desktop that registers a handler; the rest are the file managers that
/// ship with the common desktops, for the minimal setups that have neither.
/// The generic openers are skipped when the registered folder handler is a
/// terminal (see [`GENERIC_OPENERS`]).
#[cfg(target_os = "macos")]
const CANDIDATES: &[&[&str]] = &[&["open"]];
#[cfg(not(target_os = "macos"))]
const CANDIDATES: &[&[&str]] = &[
    &["xdg-open"],
    &["gio", "open"],
    &["nautilus"],
    &["dolphin"],
    &["thunar"],
    &["nemo"],
    &["pcmanfm"],
    &["pcmanfm-qt"],
    &["caja"],
];

/// Openers that only forward to whatever handles `inode/directory`. Terminals
/// such as kitty register themselves for folders, which on window managers
/// without a desktop (Hyprland, Sway) leaves `xdg-open` launching a terminal.
#[cfg(not(target_os = "macos"))]
const GENERIC_OPENERS: &[&str] = &["xdg-open", "gio"];

/// The command that will be run for a folder: the configured override when it
/// resolves, otherwise the first available platform default. The folder itself
/// is appended by [`open_folder`], so this is what the settings decide.
pub fn folder_command(configured: Option<&str>) -> Result<Vec<String>> {
    if let Some(raw) = configured.map(str::trim).filter(|raw| !raw.is_empty()) {
        let parts = shlex::split(raw)
            .filter(|parts| !parts.is_empty())
            .ok_or_else(|| KanbanError::Invalid(format!("Unparsable file manager: {raw}")))?;
        if which(&parts[0]).is_none() {
            return Err(KanbanError::Invalid(format!(
                "File manager not found on PATH: {}",
                parts[0]
            )));
        }
        return Ok(parts);
    }
    let available = || {
        CANDIDATES
            .iter()
            .filter(|candidate| which(candidate[0]).is_some())
    };
    let skip_generic = folder_handler_is_terminal();
    available()
        .find(|candidate| !(skip_generic && is_generic_opener(candidate[0])))
        .or_else(|| available().next())
        .map(|candidate| candidate.iter().map(|part| (*part).to_string()).collect())
        .ok_or_else(|| {
            KanbanError::Invalid(
                "No file manager found; set tui.file_manager in global settings".to_string(),
            )
        })
}

#[cfg(target_os = "macos")]
fn is_generic_opener(_program: &str) -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
fn is_generic_opener(program: &str) -> bool {
    GENERIC_OPENERS.contains(&program)
}

#[cfg(target_os = "macos")]
fn folder_handler_is_terminal() -> bool {
    false
}

/// Whether the desktop entry registered for `inode/directory` is a terminal.
/// Unknown handlers count as fine, so `xdg-open` stays the default.
#[cfg(not(target_os = "macos"))]
fn folder_handler_is_terminal() -> bool {
    if which("xdg-mime").is_none() {
        return false;
    }
    let Ok(output) = Command::new("xdg-mime")
        .args(["query", "default", "inode/directory"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let desktop_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if desktop_id.is_empty() {
        return false;
    }
    application_dirs()
        .into_iter()
        .map(|dir| dir.join(&desktop_id))
        .find_map(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|entry| desktop_entry_is_terminal(&entry))
}

/// `applications/` folders in XDG lookup order, user data first.
#[cfg(not(target_os = "macos"))]
fn application_dirs() -> Vec<std::path::PathBuf> {
    let non_empty = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let mut dirs = Vec::new();
    match non_empty("XDG_DATA_HOME") {
        Some(home) => dirs.push(std::path::PathBuf::from(home)),
        None => {
            if let Some(home) = non_empty("HOME") {
                dirs.push(std::path::PathBuf::from(home).join(".local/share"));
            }
        }
    }
    let system = non_empty("XDG_DATA_DIRS")
        .unwrap_or_else(|| std::ffi::OsString::from("/usr/local/share:/usr/share"));
    dirs.extend(std::env::split_paths(&system));
    dirs.into_iter()
        .map(|dir| dir.join("applications"))
        .collect()
}

/// A desktop entry is a terminal when it says `Terminal=true` or lists the
/// `TerminalEmulator` category in its `[Desktop Entry]` group.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn desktop_entry_is_terminal(entry: &str) -> bool {
    let mut in_main_group = false;
    for line in entry.lines().map(str::trim) {
        if line.starts_with('[') {
            in_main_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_main_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Terminal" if value.trim() == "true" => return true,
            "Categories" if value.split(';').any(|c| c.trim() == "TerminalEmulator") => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Open `path` in the native file manager. Returns once the opener has been
/// spawned; the caller is never told what the file manager did with it.
pub fn open_folder(path: &Path, configured: Option<&str>) -> Result<()> {
    if !path.is_dir() {
        return Err(KanbanError::Invalid(format!(
            "Folder is missing: {}",
            path.display()
        )));
    }
    let command = folder_command(configured)?;
    let (program, args) = command.split_first().expect("command is non-empty");
    Command::new(program)
        .args(args)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| KanbanError::Invalid(format!("Could not start {program}: {err}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_command_wins_over_the_platform_defaults() {
        let command = folder_command(Some("true --flag")).expect("configured command");
        assert_eq!(command, vec!["true".to_string(), "--flag".to_string()]);
    }

    #[test]
    fn blank_configuration_falls_back_to_a_platform_default() {
        let command = folder_command(Some("   ")).expect("platform default");
        assert!(
            CANDIDATES
                .iter()
                .any(|candidate| candidate[0] == command[0]),
            "unexpected default opener: {command:?}"
        );
    }

    #[test]
    fn a_configured_command_that_is_not_installed_is_reported() {
        let err = folder_command(Some("kanban4ai-no-such-file-manager"))
            .expect_err("missing file manager");
        assert!(
            err.to_string().contains("not found on PATH"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_terminal_desktop_entry_is_recognised() {
        let kitty = "[Desktop Entry]\nName=kitty URL Launcher\nExec=kitty +open %U\n\
                     Categories=System;TerminalEmulator;\nMimeType=inode/directory;\n";
        assert!(desktop_entry_is_terminal(kitty));
        assert!(desktop_entry_is_terminal(
            "[Desktop Entry]\nExec=ranger %F\nTerminal=true\n"
        ));
    }

    #[test]
    fn a_file_manager_desktop_entry_is_not_a_terminal() {
        let dolphin = "[Desktop Entry]\nExec=dolphin %u\nTerminal=false\n\
                       Categories=Qt;KDE;System;FileTools;FileManager;\n\
                       [Desktop Action new]\nCategories=TerminalEmulator;\n";
        assert!(!desktop_entry_is_terminal(dolphin));
    }

    #[test]
    fn a_missing_folder_is_reported_before_anything_is_spawned() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("gone");
        let err = open_folder(&missing, Some("true")).expect_err("missing folder");
        assert!(
            err.to_string().contains("Folder is missing"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn an_existing_folder_is_handed_to_the_configured_command() {
        let dir = tempfile::tempdir().expect("temp dir");
        open_folder(dir.path(), Some("true")).expect("spawn opener");
    }
}
