use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::agent::backends::{AutoLaunchConfig, LaunchPlan};
use crate::core::error::{KanbanError, Result};
use crate::core::models::{MessageKind, MessageRole};
use crate::core::project::Roots;
use crate::core::thread::ThreadManager;

/// Default tmux pane size when the launcher has no controlling TTY (TUI raw
/// mode). Without `-x`/`-y`, `new-session -d` can fail or inherit a 80×24
/// that then writes through the live alternate screen.
const TMUX_DEFAULT_WIDTH: &str = "120";
const TMUX_DEFAULT_HEIGHT: &str = "40";

/// Start the planned agent run. The process runs in `roots.work_path`; every
/// file it is pointed at lives under `roots.data_root`.
///
/// `tmux new-session` is isolated from the caller's TTY. A non-zero tmux exit
/// (`Ok(false)`) takes the same background fallback as an I/O `Err`; neither
/// path writes to stderr, which would tear the TUI.
pub fn spawn_plan<'a>(
    roots: impl Into<Roots<'a>>,
    plan: &LaunchPlan,
    config: &AutoLaunchConfig,
) -> Result<bool> {
    let roots = roots.into();
    if let Some(parent) = plan.log_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if config.use_tmux && command_available("tmux") {
        match spawn_tmux(roots, plan) {
            Ok(true) => return Ok(true),
            Ok(false) => {
                let err = tmux_failure_error(plan, None);
                return fallback_or_err(roots, plan, config, err);
            }
            Err(err) => return fallback_or_err(roots, plan, config, err),
        }
    }
    if config.terminal_fallback {
        spawn_background(roots, plan)
    } else {
        Err(KanbanError::Invalid(
            "tmux is not available and terminal_fallback is disabled".to_string(),
        ))
    }
}

fn fallback_or_err(
    roots: Roots<'_>,
    plan: &LaunchPlan,
    config: &AutoLaunchConfig,
    err: KanbanError,
) -> Result<bool> {
    if !config.terminal_fallback {
        return Err(err);
    }
    post_launch_note(
        roots,
        plan,
        &format!("⚠ tmux launch failed ({err}); falling back to background process."),
    );
    spawn_background(roots, plan)
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

/// Whether a live tmux session with this exact name exists. `false` when tmux
/// is unavailable (background launches have no tmux host). The `=` prefix forces
/// exact-name matching so a partial id never matches an unrelated session.
pub fn session_exists(session_id: &str) -> bool {
    if !command_available("tmux") {
        return false;
    }
    Command::new("tmux")
        .args(["has-session", "-t", &format!("={session_id}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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

/// Run a command in the foreground, inheriting the terminal, and wait for it to
/// exit. Used to reopen a stopped background agent's conversation
/// (`claude --resume <id>`) after the TUI has suspended itself. Returns whether
/// the child exited successfully.
pub fn run_foreground(command: &str, args: &[String], cwd: Option<&Path>) -> Result<bool> {
    let mut cmd = Command::new(command);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    Ok(cmd.status()?.success())
}

fn spawn_tmux(roots: Roots<'_>, plan: &LaunchPlan) -> Result<bool> {
    let script = wrapper_script(roots, plan);
    let work_path = roots.work_path.display().to_string();
    let err_path = tmux_err_path(plan);
    if let Some(parent) = err_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let err_file = std::fs::File::create(&err_path)?;
    let status = Command::new("tmux")
        .args(tmux_new_session_args(&plan.session_id, &work_path, &script))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .status()?;
    if status.success() {
        let _ = std::fs::remove_file(&err_path);
        return Ok(true);
    }
    Ok(false)
}

fn tmux_new_session_args<'a>(
    session_id: &'a str,
    work_path: &'a str,
    script: &'a str,
) -> [&'a str; 14] {
    [
        "new-session",
        "-d",
        "-x",
        TMUX_DEFAULT_WIDTH,
        "-y",
        TMUX_DEFAULT_HEIGHT,
        "-c",
        work_path,
        "-s",
        session_id,
        "--",
        "bash",
        "-c",
        script,
    ]
}

fn tmux_err_path(plan: &LaunchPlan) -> PathBuf {
    plan.log_file.with_extension("tmux.err")
}

fn tmux_failure_error(plan: &LaunchPlan, status_code: Option<i32>) -> KanbanError {
    let detail = std::fs::read_to_string(tmux_err_path(plan)).unwrap_or_default();
    let detail = detail.trim();
    let code = status_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "nonzero".to_string());
    if detail.is_empty() {
        KanbanError::Invalid(format!(
            "tmux new-session failed for {} (exit {code})",
            plan.session_id
        ))
    } else {
        KanbanError::Invalid(format!(
            "tmux new-session failed for {} (exit {code}): {detail}",
            plan.session_id
        ))
    }
}

fn post_launch_note(roots: Roots<'_>, plan: &LaunchPlan, body: &str) {
    let Ok(tm) = ThreadManager::new(roots.data_root) else {
        return;
    };
    let _ = tm.post(
        &plan.task_id,
        MessageRole::System,
        MessageKind::AgentStep,
        body,
        None,
        vec![],
        Some("kanban".to_string()),
    );
}

fn spawn_background<'a>(roots: impl Into<Roots<'a>>, plan: &LaunchPlan) -> Result<bool> {
    let roots = roots.into();
    let script = wrapper_script(roots, plan);
    Command::new("bash")
        .args(["-c", &script])
        .current_dir(roots.work_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(true)
}

fn wrapper_script<'a>(roots: impl Into<Roots<'a>>, plan: &LaunchPlan) -> String {
    let roots = roots.into();
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
    if let Some(prompt_file) = &plan.prompt_file {
        command_parts.push(format!(
            "\"$(cat -- {})\"",
            shell_quote(&prompt_file.display().to_string())
        ));
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
    // Codex may read additional prompt text from stdin even when a positional
    // prompt is present; the pi family (pi/omp) also probes stdin under `-p`.
    // All three backends would hang forever on an inherited tmux pane TTY, so
    // close stdin for their non-interactive runs.
    let stdin_redirect = if matches!(plan.backend.as_str(), "codex" | "pi" | "omp") {
        " < /dev/null"
    } else {
        ""
    };
    let run_pipeline = match &plan.transcript_file {
        Some(transcript) => format!(
            "{command_line}{stdin_redirect} 2>&1 | tee -a {} | {kanban_cmd} format-stream | tee -a {log_quoted}",
            shell_quote(&transcript.display().to_string()),
        ),
        None => format!("{command_line}{stdin_redirect} 2>&1 | tee -a {log_quoted}"),
    };
    // The agent runs in the work folder, so it can no longer reach the board
    // by a relative `.kanban/…` path: export where the data lives (and which
    // project it belongs to) so callbacks and habits both resolve.
    let project_export = roots
        .project_id
        .map(|id| format!("export KANBAN_PROJECT={}; ", shell_quote(id)))
        .unwrap_or_default();
    let data_dir_export = format!(
        "export KANBAN_DATA_DIR={}; ",
        shell_quote(&roots.kanban_dir().display().to_string())
    );
    // Always the absolute log dir under data_root. A relative `.kanban/logs`
    // fallback would mkdir in the work folder after `cd` below.
    let mkdir_logs = plan
        .log_file
        .parent()
        .map(|parent| format!("mkdir -p {}; ", shell_quote(&parent.display().to_string())))
        .unwrap_or_default();
    format!(
        "set -o pipefail; cd {}; export KANBAN_SESSION={}; export KANBAN_TASK_ID={}; export KANBAN_CMD={}; {project_export}{data_dir_export}{mkdir_logs}{}{}{run_pipeline}; status=${{PIPESTATUS[0]}}; kill $hb_pid 2>/dev/null; {}{}; exit $status",
        shell_quote(&roots.work_path.display().to_string()),
        shell_quote(&plan.session_id),
        shell_quote(&plan.task_id),
        kanban_cmd,
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
            prompt_file: None,
            log_file: log_dir.join(format!("{session_id}.log")),
            transcript_file: None,
            session_id: session_id.to_string(),
            auto_complete_on_exit: auto_complete,
            heartbeat_interval_secs: 100,
            resolve_agent: None,
            resumed_backend_session: None,
        }
    }

    /// With the board in the store, the wrapper must `cd` into the *work*
    /// folder and hand the agent the board's location through the
    /// environment — a relative `.kanban/…` would otherwise land in the repo.
    #[test]
    fn wrapper_script_runs_in_the_work_folder_and_exports_the_project() {
        let data_root = tempfile::tempdir().unwrap();
        let work_path = tempfile::tempdir().unwrap();
        let logs_dir = data_root.path().join(".kanban/logs");
        let plan = test_plan(
            &logs_dir,
            "TASK-001",
            "ses-split",
            "/bin/echo",
            vec!["hello".to_string()],
            false,
        );
        let roots = Roots::new(data_root.path(), work_path.path(), Some("my-project"));

        let script = wrapper_script(roots, &plan);

        assert!(script.contains(&format!(
            "cd {}",
            shell_quote(&work_path.path().display().to_string())
        )));
        assert!(script.contains("export KANBAN_PROJECT=my-project"));
        assert!(script.contains(&format!(
            "export KANBAN_DATA_DIR={}",
            shell_quote(&data_root.path().join(".kanban").display().to_string())
        )));
        assert!(script.contains(&format!(
            "mkdir -p {}",
            shell_quote(&logs_dir.display().to_string())
        )));

        let output = Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .expect("bash -n should run");
        assert!(
            output.status.success(),
            "generated script must be valid bash: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A board used in place has no registration, so no `KANBAN_PROJECT`.
    #[test]
    fn wrapper_script_omits_project_export_for_an_in_place_board() {
        let dir = tempfile::tempdir().unwrap();
        let plan = test_plan(
            dir.path(),
            "TASK-001",
            "ses-in-place",
            "/bin/echo",
            vec![],
            false,
        );

        let script = wrapper_script(dir.path(), &plan);

        assert!(!script.contains("KANBAN_PROJECT"));
        assert!(script.contains("export KANBAN_DATA_DIR="));
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

    /// The assembled prompt is read from disk at run time. The body must not
    /// appear in the `bash -c` script that tmux receives as argv.
    #[test]
    fn wrapper_script_reads_prompt_from_file_not_argv() {
        let dir = tempfile::tempdir().unwrap();
        let prompt_file = dir.path().join("ses-file.prompt.txt");
        let body = "PROMPT_BODY_MUST_NOT_APPEAR_IN_SCRIPT\nline two";
        std::fs::write(&prompt_file, body).unwrap();
        let mut plan = test_plan(
            dir.path(),
            "TASK-008",
            "ses-file",
            "/bin/echo",
            vec!["run".to_string()],
            false,
        );
        plan.prompt = body.to_string();
        plan.prompt_file = Some(prompt_file.clone());
        let script = wrapper_script(dir.path(), &plan);

        assert!(script.contains(&format!(
            "\"$(cat -- {})\"",
            shell_quote(&prompt_file.display().to_string())
        )));
        assert!(
            !script.contains("PROMPT_BODY_MUST_NOT_APPEAR_IN_SCRIPT"),
            "prompt body leaked into wrapper argv: {script}"
        );

        let output = Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
            .expect("bash -n should run");
        assert!(
            output.status.success(),
            "bash -n rejected prompt-file script:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// End-to-end: `/bin/echo` receives the file contents as its last
    /// argument, so the log contains the prompt body.
    #[test]
    fn background_launch_passes_prompt_file_contents() {
        let project = tempfile::tempdir().unwrap();
        let logs_dir = project.path().join(".kanban/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let marker = "MARKER_PROMPT_FILE_OK";
        let prompt_file = logs_dir.join("ses-prompt-file.prompt.txt");
        std::fs::write(&prompt_file, marker).unwrap();

        let mut plan = test_plan(
            &logs_dir,
            "TASK-009",
            "ses-prompt-file",
            "/bin/echo",
            vec![],
            false,
        );
        plan.prompt_file = Some(prompt_file);

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
            content.contains(marker),
            "echo should receive the prompt file contents; log content: {content:?}"
        );
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

    fn auto_cfg(use_tmux: bool, fallback: bool) -> AutoLaunchConfig {
        AutoLaunchConfig {
            enabled: true,
            use_tmux,
            terminal_fallback: fallback,
            auto_complete_on_exit: false,
            default_agent: "test".to_string(),
        }
    }

    #[test]
    fn tmux_new_session_args_set_size_cwd_and_session() {
        let args = tmux_new_session_args("ses-1", "/tmp/work", "echo hi");
        assert_eq!(
            args,
            [
                "new-session",
                "-d",
                "-x",
                "120",
                "-y",
                "40",
                "-c",
                "/tmp/work",
                "-s",
                "ses-1",
                "--",
                "bash",
                "-c",
                "echo hi",
            ]
        );
    }

    #[test]
    fn tmux_failure_error_includes_captured_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let plan = test_plan(
            dir.path(),
            "TASK-199",
            "ses-err",
            "/bin/echo",
            vec![],
            false,
        );
        std::fs::write(
            tmux_err_path(&plan),
            "open terminal failed: not a terminal\n",
        )
        .unwrap();
        let err = tmux_failure_error(&plan, Some(1));
        let text = err.to_string();
        assert!(text.contains("ses-err"), "{text}");
        assert!(text.contains("exit 1"), "{text}");
        assert!(
            text.contains("open terminal failed: not a terminal"),
            "{text}"
        );
    }

    /// Hold a live tmux session so the next `new-session -s` with the same
    /// name returns non-zero (`Ok(false)`). Skips when tmux cannot host it.
    fn occupy_tmux_session(session_id: &str) -> Option<OccupiedTmux> {
        if !command_available("tmux") {
            return None;
        }
        let status = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-x",
                "80",
                "-y",
                "24",
                "-s",
                session_id,
                "--",
                "sleep",
                "30",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !status.success() || !session_exists(session_id) {
            return None;
        }
        Some(OccupiedTmux {
            session_id: session_id.to_string(),
        })
    }

    struct OccupiedTmux {
        session_id: String,
    }

    impl Drop for OccupiedTmux {
        fn drop(&mut self) {
            let _ = kill_session(&self.session_id);
        }
    }

    /// A duplicate tmux session name makes `new-session` fail. With
    /// `terminal_fallback` that `Ok(false)` must take the same background
    /// path as an I/O `Err`, write the agent log, and record the tmux
    /// error on the thread — never on stderr.
    #[test]
    fn spawn_plan_falls_back_when_tmux_session_already_exists() {
        let session_id = format!("kb4ai-t200-fb-{}", std::process::id());
        let Some(_hold) = occupy_tmux_session(&session_id) else {
            return;
        };
        let project = tempfile::tempdir().unwrap();
        let logs_dir = project.path().join(".kanban/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();

        let marker = "MARKER_TMUX_FALLBACK_OK";
        let plan = test_plan(
            &logs_dir,
            "TASK-200",
            &session_id,
            "/bin/echo",
            vec![marker.to_string()],
            false,
        );

        let started = spawn_plan(project.path(), &plan, &auto_cfg(true, true)).unwrap();
        assert!(started, "Ok(false) from tmux must fall back to background");

        let log_path = plan.log_file.clone();
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
            content.contains(marker),
            "fallback background job must write the log; got {content:?}"
        );

        let thread = ThreadManager::new(project.path())
            .unwrap()
            .load("TASK-200")
            .unwrap();
        assert!(
            thread.messages.iter().any(|message| {
                message.kind == MessageKind::AgentStep
                    && message.body.contains("tmux launch failed")
            }),
            "fallback must post the tmux error on the thread: {:?}",
            thread.messages
        );
    }

    #[test]
    fn spawn_plan_without_fallback_returns_tmux_error() {
        let session_id = format!("kb4ai-t200-nf-{}", std::process::id());
        let Some(_hold) = occupy_tmux_session(&session_id) else {
            return;
        };
        let project = tempfile::tempdir().unwrap();
        let logs_dir = project.path().join(".kanban/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let plan = test_plan(
            &logs_dir,
            "TASK-200",
            &session_id,
            "/bin/echo",
            vec![],
            false,
        );

        let err = spawn_plan(project.path(), &plan, &auto_cfg(true, false)).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains(&format!("tmux new-session failed for {session_id}")),
            "{text}"
        );
    }
}
