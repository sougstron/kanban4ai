use std::path::{Path, PathBuf};
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

/// Kill the tmux session hosting an agent job. Returns `false` when tmux is
/// unavailable or no such session exists (background jobs have no tmux host
/// to signal). The `=` prefix forces exact-name matching so a partial id can
/// never kill an unrelated session.
pub fn kill_session(session_id: &str) -> Result<bool> {
    if !command_available("tmux") {
        return Ok(false);
    }
    let status = Command::new("tmux")
        .args(["kill-session", "-t", &format!("={session_id}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
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
    let mut command_parts = std::iter::once(plan.command.as_str())
        .chain(plan.args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>();
    // Opencode agent resolution (`opencode agent list`) takes seconds, so it
    // is deferred into this script: the `--agent` value becomes a shell
    // variable filled by a `resolve-agent` callback right before the agent
    // command runs, keeping the launching process (the TUI) unblocked.
    if plan.resolve_agent.is_some()
        && let Some(flag_pos) = plan.args.iter().position(|arg| arg == "--agent")
        && let Some(value) = command_parts.get_mut(flag_pos + 2)
    {
        *value = "\"$KANBAN_AGENT\"".to_string();
    }
    let command_line = command_parts.join(" ");
    // Use the absolute path of the current binary so callbacks always
    // resolve to *this* kanban4ai executable regardless of PATH or
    // invocation name.  Falls back to bare "kanban" (the historical
    // contract) when no usable path can be resolved.
    let kanban_cmd = std::env::current_exe()
        .ok()
        .and_then(resolve_callback_binary)
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
    // Background heartbeat keyed on the wrapper shell's PID: agents can spend
    // longer than the heartbeat timeout inside subagents without running any
    // shell command, and a live process must never be marked crashed.
    let heartbeat_loop = format!(
        "( while kill -0 $$ 2>/dev/null; do {kanban_cmd} heartbeat --session {} >/dev/null 2>&1 || true; sleep {}; done ) & hb_pid=$!; ",
        shell_quote(&plan.session_id),
        plan.heartbeat_interval_secs
    );
    // Runs after the heartbeat loop starts, so the session stays alive while
    // the resolve callback waits on the opencode CLI. Falls back to the
    // requested name when the callback fails or prints nothing.
    let resolve_agent = plan
        .resolve_agent
        .as_deref()
        .map(|requested| {
            format!(
                "KANBAN_AGENT=\"$({kanban_cmd} resolve-agent --command {} {} 2>/dev/null)\"; [ -n \"$KANBAN_AGENT\" ] || KANBAN_AGENT={}; ",
                shell_quote(&plan.command),
                shell_quote(requested),
                shell_quote(requested),
            )
        })
        .unwrap_or_default();
    // The agent pipeline: `command | tee -a log`. For backends with a machine
    // transcript (claude stream-json) the raw JSONL is captured to a separate
    // file and reformatted to human text for the log by `kanban format-stream`,
    // so the log stays readable while the transcript feeds provenance harvest.
    // `PIPESTATUS[0]` is the agent command's status in both shapes (it stays
    // the head of the pipeline).
    let log_quoted = shell_quote(&plan.log_file.display().to_string());
    let run_pipeline = match &plan.transcript_file {
        Some(transcript) => format!(
            "{command_line} 2>&1 | tee -a {} | {kanban_cmd} format-stream | tee -a {log_quoted}",
            shell_quote(&transcript.display().to_string()),
        ),
        None => format!("{command_line} 2>&1 | tee -a {log_quoted}"),
    };
    format!(
        "set -o pipefail; cd {}; export KANBAN_SESSION={}; export KANBAN_TASK_ID={}; export KANBAN_CMD={}; mkdir -p {}; {}{}{run_pipeline}; status=${{PIPESTATUS[0]}}; kill $hb_pid 2>/dev/null; {}{}; exit $status",
        shell_quote(&project_path.display().to_string()),
        shell_quote(&plan.session_id),
        shell_quote(&plan.task_id),
        kanban_cmd,
        shell_quote(
            &plan
                .log_file
                .parent()
                .unwrap_or_else(|| Path::new(".kanban/logs"))
                .display()
                .to_string()
        ),
        heartbeat_loop,
        resolve_agent,
        auto_segment,
        reconcile
    )
}

/// Resolve the on-disk path of the running binary for wrapper-script
/// callbacks. When the executable is replaced while running (e.g. a
/// rebuild of this repo or a package upgrade), Linux reports
/// `/proc/self/exe` with a " (deleted)" suffix even though a fresh
/// binary exists at the original path; callbacks must target that fresh
/// file, not the nonexistent suffixed one. Returns `None` when no
/// existing path remains.
fn resolve_callback_binary(exe: PathBuf) -> Option<PathBuf> {
    if exe.exists() {
        return Some(exe);
    }
    let stripped = PathBuf::from(exe.to_str()?.strip_suffix(" (deleted)")?);
    stripped.exists().then_some(stripped)
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
            model: None,
            args,
            prompt: "test prompt".to_string(),
            log_file: log_dir.join(format!("{session_id}.log")),
            transcript_file: None,
            session_id: session_id.to_string(),
            auto_complete_on_exit: auto_complete,
            heartbeat_interval_secs: 100,
            resolve_agent: None,
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
        assert!(script.contains("export KANBAN_CMD="));
        assert!(script.contains("heartbeat --session ses-test-false"));
        assert!(script.contains("sleep 100"));
        assert!(script.contains("kill $hb_pid"));

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

    /// A plan with a transcript file (claude) reformats stdout through
    /// `format-stream` and captures raw JSONL to the transcript, while keeping
    /// the agent command at the head of the pipe (so `PIPESTATUS[0]` is its
    /// exit code). Still parses under `bash -n`.
    #[test]
    fn wrapper_script_pipes_transcript_through_format_stream() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = test_plan(
            dir.path(),
            "TASK-007",
            "ses-transcript",
            "/bin/echo",
            vec!["hi".to_string()],
            false,
        );
        plan.transcript_file = Some(dir.path().join("ses-transcript.transcript.jsonl"));
        let script = wrapper_script(dir.path(), &plan);

        assert!(script.contains("ses-transcript.transcript.jsonl"));
        assert!(script.contains("format-stream"));
        assert!(script.contains("status=${PIPESTATUS[0]}"));

        let output = Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .expect("bash -n should run");
        assert!(
            output.status.success(),
            "bash -n rejected transcript pipeline:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// With a deferred opencode agent the script must resolve the name via
    /// the `resolve-agent` callback (with the requested name as fallback)
    /// and pass the shell variable — not the literal name — to `--agent`.
    #[test]
    fn wrapper_script_defers_agent_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = test_plan(
            dir.path(),
            "TASK-004",
            "ses-resolve-test",
            "/bin/echo",
            vec![
                "run".to_string(),
                "--agent".to_string(),
                "hephaestus".to_string(),
                "prompt".to_string(),
            ],
            false,
        );
        plan.resolve_agent = Some("hephaestus".to_string());
        let script = wrapper_script(dir.path(), &plan);

        assert!(script.contains("resolve-agent --command /bin/echo hephaestus"));
        assert!(script.contains("|| KANBAN_AGENT=hephaestus"));
        assert!(script.contains("--agent \"$KANBAN_AGENT\""));
        assert!(!script.contains("--agent hephaestus"));

        let output = Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .expect("bash -n should run");
        assert!(
            output.status.success(),
            "bash -n rejected script with deferred agent resolution:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Without a deferred agent the `--agent` value must stay a quoted
    /// literal even when the flag is present (no accidental substitution).
    #[test]
    fn wrapper_script_keeps_literal_agent_without_deferral() {
        let dir = tempfile::tempdir().unwrap();
        let plan = test_plan(
            dir.path(),
            "TASK-005",
            "ses-literal-test",
            "/bin/echo",
            vec![
                "run".to_string(),
                "--agent".to_string(),
                "hephaestus".to_string(),
                "prompt".to_string(),
            ],
            false,
        );
        let script = wrapper_script(dir.path(), &plan);
        assert!(script.contains("--agent hephaestus"));
        assert!(!script.contains("KANBAN_AGENT"));
    }

    /// End-to-end through bash: when the resolve callback fails (the test
    /// binary is not the kanban CLI), the agent command still receives the
    /// requested name via the fallback assignment.
    #[test]
    fn background_launch_falls_back_to_requested_agent() {
        let project = tempfile::tempdir().unwrap();
        let logs_dir = project.path().join(".kanban/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let marker = "MARKER_AGENT_FALLBACK";
        let mut plan = test_plan(
            &logs_dir,
            "TASK-006",
            "ses-resolve-bg",
            "/bin/echo",
            vec!["--agent".to_string(), marker.to_string()],
            false,
        );
        plan.resolve_agent = Some(marker.to_string());

        let started = spawn_background(project.path(), &plan).unwrap();
        assert!(started, "spawn_background returned true");

        let log_path = plan.log_file;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut content = String::new();
        while Instant::now() < deadline {
            content = std::fs::read_to_string(&log_path).unwrap_or_default();
            if content.contains(marker) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            content.contains(&format!("--agent {marker}")),
            "echo should receive the fallback agent name; log content: {content:?}"
        );
    }

    /// An existing path is returned unchanged, even when it happens to
    /// end with the literal " (deleted)" marker.
    #[test]
    fn resolve_callback_binary_keeps_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("kanban4ai");
        std::fs::write(&plain, b"").unwrap();
        assert_eq!(resolve_callback_binary(plain.clone()), Some(plain));

        let literal = dir.path().join("kanban4ai (deleted)");
        std::fs::write(&literal, b"").unwrap();
        assert_eq!(resolve_callback_binary(literal.clone()), Some(literal));
    }

    /// A stale `/proc/self/exe` reading (" (deleted)" suffix after the
    /// binary was replaced in place) resolves to the fresh file at the
    /// original path.
    #[test]
    fn resolve_callback_binary_strips_deleted_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("kanban4ai");
        std::fs::write(&fresh, b"").unwrap();
        let reported = dir.path().join("kanban4ai (deleted)");
        assert_eq!(resolve_callback_binary(reported), Some(fresh));
    }

    /// When neither the reported path nor the stripped path exists the
    /// caller must fall back to the bare "kanban" contract.
    #[test]
    fn resolve_callback_binary_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_callback_binary(dir.path().join("kanban4ai (deleted)")),
            None
        );
        assert_eq!(resolve_callback_binary(dir.path().join("kanban4ai")), None);
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
