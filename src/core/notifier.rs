//! Desktop notifications via a configurable command (default `notify-send`).
//! Failures are silent: a missing binary or timeout never breaks board logic.
//!
//! A `cargo test` process, and a `kanban4ai` binary it spawned, does not
//! deliver a session notifier (`notify-send`, `dunstify`, `notify-desktop`)
//! to the desktop. A test that points `command` at its own program still
//! receives the alert.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use serde_yaml_ng::Mapping;

#[derive(Debug, Clone)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub questions: bool,
    pub completion: bool,
    pub chained_start: bool,
    pub waiting: bool,
    pub crash: bool,
    pub command: String,
    pub timeout: u64,
    pub max_body_chars: usize,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        NotificationConfig {
            enabled: true,
            questions: true,
            completion: true,
            chained_start: true,
            waiting: true,
            crash: true,
            command: "notify-send".to_string(),
            timeout: 3,
            max_body_chars: 240,
        }
    }
}

impl NotificationConfig {
    pub fn from_mapping(data: &Mapping) -> Self {
        let defaults = NotificationConfig::default();
        let get_bool =
            |key: &str, fallback: bool| data.get(key).and_then(|v| v.as_bool()).unwrap_or(fallback);
        NotificationConfig {
            enabled: get_bool("enabled", defaults.enabled),
            questions: get_bool("questions", defaults.questions),
            completion: get_bool("completion", defaults.completion),
            chained_start: get_bool("chained_start", defaults.chained_start),
            waiting: get_bool("waiting", defaults.waiting),
            crash: get_bool("crash", defaults.crash),
            command: data
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or(&defaults.command)
                .to_string(),
            timeout: data
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(defaults.timeout),
            max_body_chars: data
                .get("max_body_chars")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(defaults.max_body_chars),
        }
    }
}

pub struct DesktopNotifier {
    pub config: NotificationConfig,
    /// Registered project name shown in failure alerts ("task failed",
    /// "task stranded"); `None` leaves the titles as before.
    pub project: Option<String>,
}

impl DesktopNotifier {
    pub fn new(config: NotificationConfig, project: Option<String>) -> Self {
        DesktopNotifier { config, project }
    }

    /// Failure alerts may come from any of several boards, so the project
    /// name joins the task id when the board is registered.
    fn failure_title(&self, prefix: &str, task_id: &str) -> String {
        match &self.project {
            Some(name) => format!("{prefix}: {task_id} ({name})"),
            None => format!("{prefix}: {task_id}"),
        }
    }

    pub fn question(&self, task_id: &str, title: &str, body: &str) -> bool {
        if !self.config.questions {
            return false;
        }
        self.send(
            &format!("Kanban question: {task_id}"),
            &format!("{title}\n{body}"),
            "critical",
        )
    }

    pub fn completion(&self, task_id: &str, title: &str, status: &str) -> bool {
        if !self.config.completion {
            return false;
        }
        self.send(&format!("Kanban task {status}: {task_id}"), title, "normal")
    }

    pub fn waiting(&self, task_id: &str, title: &str, note: &str, deadline: &str) -> bool {
        if !self.config.waiting {
            return false;
        }
        self.send(
            &format!("Kanban task waiting: {task_id}"),
            &format!("{title}\n{note}\nRelaunch deadline: {deadline}"),
            "normal",
        )
    }

    pub fn stranded(&self, task_id: &str, title: &str, detail: &str) -> bool {
        self.send(
            &self.failure_title("Kanban task stranded", task_id),
            &format!("{title}\n{detail}"),
            "critical",
        )
    }

    /// A run failed: the agent crashed (non-zero exit, heartbeat timeout) or
    /// a launch never started. `detail` says what failed and what happens
    /// next (a scheduled retry, or that no retry is configured).
    pub fn crash(&self, task_id: &str, title: &str, detail: &str) -> bool {
        if !self.config.crash {
            return false;
        }
        self.send(
            &self.failure_title("Kanban task failed", task_id),
            &format!("{title}\n{detail}"),
            "critical",
        )
    }

    pub fn chained_start(&self, task_id: &str, title: &str, target_task_id: &str) -> bool {
        if !self.config.chained_start {
            return false;
        }
        self.send(
            &format!("Kanban chained task started: {task_id}"),
            &format!("{title}\nTriggered by {target_task_id} entering review"),
            "normal",
        )
    }

    pub fn send(&self, title: &str, body: &str, urgency: &str) -> bool {
        if !self.config.enabled {
            return false;
        }

        let Some(parts) = shlex::split(&self.config.command) else {
            return false;
        };
        let Some((program, args)) = parts.split_first() else {
            return false;
        };
        // Tests run inside the developer's desktop session and inherit its
        // D-Bus address, so a real notify-send pops a bubble they have to
        // dismiss. A custom command (the alert-spy scripts) is not a session
        // notifier and still runs.
        if session_notifier_suppressed(program) {
            return false;
        }
        if which(program).is_none() {
            return false;
        }

        let trimmed_body = self.trim_body(body);
        // Fire and forget: spawn detached, never block board logic on the
        // notification daemon (the Python version waited with a timeout; a
        // spawned notify-send exits immediately on its own).
        Command::new(program)
            .args(args)
            .args(["--urgency", urgency, title, &trimmed_body])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    }

    fn trim_body(&self, body: &str) -> String {
        let limit = self.config.max_body_chars.max(1);
        let chars: Vec<char> = body.chars().collect();
        if chars.len() <= limit {
            return body.to_string();
        }
        let mut trimmed: String = chars[..limit - 1].iter().collect();
        trimmed.push('…');
        trimmed
    }
}

/// Minimal PATH lookup (avoids a dependency for one call).
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = std::path::PathBuf::from(program);
        return if is_executable(&path) {
            Some(path)
        } else {
            None
        };
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// `notify-send` and the drop-in replacements that talk to the session
/// notification daemon. Anything else is a test spy or a user-supplied
/// program and must still be spawned.
fn is_session_notifier(program: &str) -> bool {
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    matches!(name, "notify-send" | "dunstify" | "notify-desktop")
}

fn session_notifier_suppressed(program: &str) -> bool {
    is_session_notifier(program) && in_cargo_test_process()
}

fn in_cargo_test_process() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(detect_cargo_test_process)
}

fn detect_cargo_test_process() -> bool {
    if std::env::current_exe().is_ok_and(|path| looks_like_cargo_test_exe(&path)) {
        return true;
    }
    // `assert_cmd` runs the real `target/debug/kanban4ai`, whose own path is
    // not a test artifact. Its parent (the harness in `target/*/deps`) is.
    #[cfg(target_os = "linux")]
    {
        let mut pid = std::process::id();
        for _ in 0..16 {
            let Some(parent) = parent_pid(pid) else {
                break;
            };
            if parent <= 1 || parent == pid {
                break;
            }
            if proc_exe(parent).is_some_and(|path| looks_like_cargo_test_exe(&path)) {
                return true;
            }
            pid = parent;
        }
    }
    false
}

/// Cargo names test harnesses `{name}-{16 hex}` and puts them in `deps/`.
/// `target/debug/kanban4ai` and the installed binary do not match.
fn looks_like_cargo_test_exe(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((stem, hash)) = name.rsplit_once('-') else {
        return false;
    };
    if stem.is_empty() || hash.len() != 16 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    path.parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        == Some("deps")
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn proc_exe(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_alerts_default_on_and_can_be_toggled_off() {
        assert!(NotificationConfig::default().crash);

        let mapping: Mapping = serde_yaml_ng::from_str("crash: false\nwaiting: false").unwrap();
        let config = NotificationConfig::from_mapping(&mapping);
        assert!(!config.crash);
        assert!(!config.waiting);
    }

    #[test]
    fn failure_titles_name_the_project_and_fall_back_to_the_task_id() {
        let config = NotificationConfig::default();

        let named = DesktopNotifier::new(config.clone(), Some("api-server".to_string()));
        assert_eq!(
            named.failure_title("Kanban task failed", "TASK-7"),
            "Kanban task failed: TASK-7 (api-server)"
        );
        assert_eq!(
            named.failure_title("Kanban task stranded", "TASK-7"),
            "Kanban task stranded: TASK-7 (api-server)"
        );

        let unnamed = DesktopNotifier::new(config, None);
        assert_eq!(
            unnamed.failure_title("Kanban task failed", "TASK-7"),
            "Kanban task failed: TASK-7"
        );
    }

    #[test]
    fn cargo_test_binaries_live_in_deps_with_a_hash_suffix() {
        assert!(looks_like_cargo_test_exe(Path::new(
            "target/debug/deps/scheduler_test-0123456789abcdef"
        )));
        assert!(looks_like_cargo_test_exe(Path::new(
            "target/release/deps/kanban4ai-0123456789ABCDEF"
        )));
        assert!(!looks_like_cargo_test_exe(Path::new(
            "target/debug/kanban4ai"
        )));
        assert!(!looks_like_cargo_test_exe(Path::new(
            "/home/shmon/.local/opt/kanban4ai/0.8.8/kanban4ai"
        )));
        assert!(!looks_like_cargo_test_exe(Path::new(
            "target/debug/deps/libindoc-aed9338965289d28.so"
        )));
        assert!(!looks_like_cargo_test_exe(Path::new(
            "/usr/bin/notify-send"
        )));
    }

    #[test]
    fn only_session_notifiers_are_hidden_during_tests() {
        assert!(is_session_notifier("notify-send"));
        assert!(is_session_notifier("/usr/bin/notify-send"));
        assert!(is_session_notifier("dunstify"));
        assert!(is_session_notifier("/usr/bin/notify-desktop"));
        assert!(!is_session_notifier("/tmp/board/notify-spy"));
        assert!(!is_session_notifier("echo"));
    }

    #[test]
    fn this_harness_is_a_cargo_test_process() {
        let exe = std::env::current_exe().expect("current exe");
        assert!(
            looks_like_cargo_test_exe(&exe),
            "cargo test binary {exe:?} must match session-notifier suppression"
        );
        assert!(in_cargo_test_process());
    }

    /// `kanban4ai` spawned by a test is not itself under `deps/`. Suppression
    /// depends on `/proc/<pid>/exe` of the parent harness matching the same
    /// predicate as `current_exe`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_spawned_child_sees_this_harness_as_its_parent() {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg("readlink /proc/$PPID/exe")
            .output()
            .expect("spawn sh");
        assert!(output.status.success(), "readlink failed");
        let path = String::from_utf8(output.stdout).expect("utf8");
        let path = path.trim();
        assert!(
            looks_like_cargo_test_exe(Path::new(path)),
            "parent exe {path:?} should be the cargo test harness"
        );
    }

    #[cfg(unix)]
    fn write_marker_script(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\ntouch \"$(dirname \"$0\")/called\"\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn cargo_test_does_not_deliver_notify_send_to_the_desktop() {
        let dir = tempfile::tempdir().unwrap();
        let spy = write_marker_script(dir.path(), "notify-send");
        let notifier = DesktopNotifier::new(
            NotificationConfig {
                command: spy.display().to_string(),
                ..NotificationConfig::default()
            },
            None,
        );

        assert!(!notifier.send("Kanban task failed: TASK-1", "body", "critical"));

        let marker = dir.path().join("called");
        for _ in 0..20 {
            assert!(
                !marker.exists(),
                "notify-send must not run inside cargo test"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_custom_notification_command_still_runs_under_cargo_test() {
        let dir = tempfile::tempdir().unwrap();
        let spy = write_marker_script(dir.path(), "notify-spy");
        let notifier = DesktopNotifier::new(
            NotificationConfig {
                command: spy.display().to_string(),
                ..NotificationConfig::default()
            },
            None,
        );

        assert!(notifier.send("Kanban task failed: TASK-1", "body", "normal"));

        let marker = dir.path().join("called");
        for _ in 0..20 {
            if marker.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("custom notification command was not executed");
    }
}
