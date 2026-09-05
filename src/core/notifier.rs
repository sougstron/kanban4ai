//! Desktop notifications via a configurable command (default `notify-send`).
//! Failures are silent: a missing binary or timeout never breaks board logic.

use std::process::{Command, Stdio};

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
}

impl DesktopNotifier {
    pub fn new(config: NotificationConfig) -> Self {
        DesktopNotifier { config }
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
            &format!("Kanban task stranded: {task_id}"),
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
            &format!("Kanban task failed: {task_id}"),
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
pub fn which(program: &str) -> Option<std::path::PathBuf> {
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

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
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
}
