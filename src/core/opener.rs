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
    &["caja"],
];

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
    CANDIDATES
        .iter()
        .find(|candidate| which(candidate[0]).is_some())
        .map(|candidate| candidate.iter().map(|part| (*part).to_string()).collect())
        .ok_or_else(|| {
            KanbanError::Invalid(
                "No file manager found; set tui.file_manager in global settings".to_string(),
            )
        })
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
