use std::path::Path;
use std::process::{Command, Stdio};

use crate::agent::backends::{AutoLaunchConfig, LaunchPlan};
use crate::core::error::{KanbanError, Result};

pub fn spawn_plan(
    project_path: &Path,
    plan: &LaunchPlan,
    config: &AutoLaunchConfig,
) -> Result<bool> {
    if let Some(parent) = plan.log_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if config.use_tmux && command_available("tmux") {
        return spawn_tmux(project_path, plan).or_else(|err| {
            if config.terminal_fallback {
                eprintln!(
                    "Warning: tmux launch failed ({err}); falling back to background process."
                );
                spawn_background(project_path, plan)
            } else {
                Err(err)
            }
        });
    }
    if config.terminal_fallback {
        spawn_background(project_path, plan)
    } else {
        eprintln!("Warning: tmux is not available and terminal_fallback is disabled.");
        Ok(false)
    }
}

pub fn attach_to_session(session_id: &str) -> Result<bool> {
    if !command_available("tmux") {
        eprintln!("tmux is not available; cannot attach to session {session_id}");
        return Ok(false);
    }
    let status = Command::new("tmux")
        .args(["attach-session", "-t", session_id])
        .status()?;
    Ok(status.success())
}

fn spawn_tmux(project_path: &Path, plan: &LaunchPlan) -> Result<bool> {
    let script = wrapper_script(project_path, plan);
    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &plan.session_id,
            "--",
            "bash",
            "-c",
            &script,
        ])
        .status()?;
    Ok(status.success())
}

fn spawn_background(project_path: &Path, plan: &LaunchPlan) -> Result<bool> {
    let script = wrapper_script(project_path, plan);
    Command::new("bash")
        .args(["-c", &script])
        .current_dir(project_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(true)
}

fn wrapper_script(project_path: &Path, plan: &LaunchPlan) -> String {
    let command_line = shell_join(
        std::iter::once(plan.command.as_str())
            .chain(plan.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .as_slice(),
    );
    // Use the absolute path of the current binary so callbacks always
    // resolve to *this* kanban4ai executable regardless of PATH or
    // invocation name.  Falls back to bare "kanban" (the historical
    // contract) when current_exe is unavailable.
    let kanban_cmd = std::env::current_exe()
        .ok()
        .map(|p| shell_quote(&p.display().to_string()))
        .unwrap_or_else(|| "kanban".to_string());
    let auto_segment = if plan.auto_complete_on_exit {
        format!(
            "if [ \"$status\" -eq 0 ]; then {kanban_cmd} done {} --session {} --agent; fi; ",
            shell_quote(&plan.task_id),
            shell_quote(&plan.session_id)
        )
    } else {
        String::new()
    };
    let reconcile = format!(
        "{kanban_cmd} agent-exit {} --session {} --status \"$status\"",
        shell_quote(&plan.task_id),
        shell_quote(&plan.session_id)
    );
    format!(
        "set -o pipefail; cd {}; export KANBAN_SESSION={}; export KANBAN_TASK_ID={}; mkdir -p {}; {} 2>&1 | tee -a {}; status=${{PIPESTATUS[0]}}; {}{}; exit $status",
        shell_quote(&project_path.display().to_string()),
        shell_quote(&plan.session_id),
        shell_quote(&plan.task_id),
        shell_quote(
            &plan
                .log_file
                .parent()
                .unwrap_or_else(|| Path::new(".kanban/logs"))
                .display()
                .to_string()
        ),
        command_line,
        shell_quote(&plan.log_file.display().to_string()),
        auto_segment,
        reconcile
    )
}

fn command_available(command: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let path = dir.join(command);
        path.is_file() && is_executable(&path)
    })
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn shell_join(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "@%_+=:,./-".contains(ch))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

impl From<String> for KanbanError {
    fn from(value: String) -> Self {
        KanbanError::Invalid(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn test_plan(
        log_dir: &Path,
        task_id: &str,
        session_id: &str,
        command: &str,
        args: Vec<String>,
        auto_complete: bool,
    ) -> LaunchPlan {
        LaunchPlan {
            backend: "test".to_string(),
            task_id: task_id.to_string(),
            command: command.to_string(),
            args,
            prompt: "test prompt".to_string(),
            log_file: log_dir.join(format!("{session_id}.log")),
            session_id: session_id.to_string(),
            auto_complete_on_exit: auto_complete,
        }
    }

    /// Verify the generated shell script parses without error under
    /// `bash -n` when `auto_complete_on_exit` is **false** (the case that
    /// previously produced a `; ;` syntax error).
    #[test]
    fn wrapper_script_syntax_auto_complete_false() {
        let dir = tempfile::tempdir().unwrap();
        let plan = test_plan(
            dir.path(),
            "TASK-001",
            "ses-test-false",
            "/bin/echo",
            vec!["hello".to_string()],
            false,
        );
        let script = wrapper_script(dir.path(), &plan);

        let output = Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .expect("bash -n should run");
        assert!(
            output.status.success(),
            "bash -n rejected script with auto_complete_on_exit=false:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Same syntax check with `auto_complete_on_exit` **true** to keep
    /// that path covered as well.
    #[test]
    fn wrapper_script_syntax_auto_complete_true() {
        let dir = tempfile::tempdir().unwrap();
        let plan = test_plan(
            dir.path(),
            "TASK-002",
            "ses-test-true",
            "/bin/echo",
            vec!["world".to_string()],
            true,
        );
        let script = wrapper_script(dir.path(), &plan);

        let output = Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .expect("bash -n should run");
        assert!(
            output.status.success(),
            "bash -n rejected script with auto_complete_on_exit=true:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Spawn a real background process via `spawn_background` using
    /// `/bin/echo` with a unique marker and
    /// `auto_complete_on_exit=false`.  No global state mutation: the
    /// wrapper script invokes `current_exe()` for the callback commands,
    /// so the marker is already in the log before those callbacks run
    /// (their success is irrelevant).  Polls the log (bounded timeout)
    /// for the marker as proof that execution reached bash.
    #[test]
    fn background_launch_writes_log() {
        let project = tempfile::tempdir().unwrap();
        let logs_dir = project.path().join(".kanban/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let marker = "MARKER_BG_OK_UNIQUE";
        let plan = test_plan(
            &logs_dir,
            "TASK-003",
            "ses-bg-test",
            "/bin/echo",
            vec![marker.to_string()],
            false,
        );

        let started = spawn_background(project.path(), &plan).unwrap();
        assert!(started, "spawn_background returned true");

        let log_path = plan.log_file;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut found = false;
        while Instant::now() < deadline {
            if let Ok(content) = std::fs::read_to_string(&log_path)
                && content.contains(marker)
            {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(
            found,
            "Background log should contain marker after polling 10s.\n\
             Log path: {}\nLog exists: {}",
            log_path.display(),
            log_path.exists()
        );
    }
}
