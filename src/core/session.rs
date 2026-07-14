//! Agent session tracking (`.kanban/sessions/<id>.yaml`): linking sessions to
//! tasks, heartbeats, and crash detection via heartbeat timeout.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::core::error::Result;
use crate::core::models::{MessageKind, MessageRole, Session, SessionStatus};
use crate::core::thread::ThreadManager;
use crate::core::timefmt;

pub struct SessionManager {
    pub project_path: PathBuf,
    pub sessions_dir: PathBuf,
}

impl SessionManager {
    pub fn new(project_path: impl AsRef<Path>) -> Self {
        let project_path = project_path.as_ref().to_path_buf();
        let sessions_dir = project_path.join(".kanban").join("sessions");
        SessionManager {
            project_path,
            sessions_dir,
        }
    }

    fn session_file(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.yaml"))
    }

    pub fn link_session(&self, task_id: &str, session_id: &str) -> Result<Session> {
        let session = Session::new(session_id, task_id);
        self.save_session(&session)?;
        Ok(session)
    }

    pub fn unlink_session(&self, session_id: &str) {
        let _ = fs::remove_file(self.session_file(session_id));
    }

    pub fn get_task_by_session(&self, session_id: &str) -> Option<String> {
        let session = self.load_session(session_id)?;
        if session.status == SessionStatus::Active {
            Some(session.task_id)
        } else {
            None
        }
    }

    pub fn is_session_active(&self, session_id: &str) -> bool {
        matches!(
            self.load_session(session_id),
            Some(session) if session.status == SessionStatus::Active
        )
    }

    pub fn list_sessions(&self) -> Vec<Session> {
        let Ok(entries) = fs::read_dir(&self.sessions_dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "yaml"))
            .filter_map(|p| self.load_session_from_file(&p))
            .collect()
    }

    pub fn list_active_sessions(&self) -> Vec<Session> {
        self.list_sessions()
            .into_iter()
            .filter(|s| s.status == SessionStatus::Active)
            .collect()
    }

    pub fn heartbeat(&self, session_id: &str) -> Result<()> {
        if let Some(mut session) = self.load_session(session_id) {
            session.last_seen = timefmt::now();
            self.save_session(&session)?;
        }
        Ok(())
    }

    pub fn close_session(&self, session_id: &str) -> Result<()> {
        if let Some(mut session) = self.load_session(session_id) {
            session.status = SessionStatus::Closed;
            session.ended_at = Some(timefmt::now());
            self.save_session(&session)?;
        }
        Ok(())
    }

    /// Mark an active session crashed and record a system message on its task.
    pub fn crash_session(&self, session_id: &str) -> Result<()> {
        let Some(mut session) = self.load_session(session_id) else {
            return Ok(());
        };
        if session.status != SessionStatus::Active {
            return Ok(());
        }
        let now = timefmt::now();
        session.status = SessionStatus::Crashed;
        self.save_session(&session)?;
        self.post_stalled_message(&session, &now)?;
        Ok(())
    }

    fn post_stalled_message(&self, session: &Session, when: &chrono::NaiveDateTime) -> Result<()> {
        ThreadManager::new(&self.project_path)?.post(
            &session.task_id,
            MessageRole::System,
            MessageKind::System,
            &format!(
                "Session stalled and was marked crashed.\nSession: {}\nDetected at: {}",
                session.id,
                timefmt::format(when)
            ),
            None,
            vec![],
            Some("kanban".to_string()),
        )?;
        Ok(())
    }

    /// Crash every active session whose heartbeat is older than `timeout` seconds.
    pub fn check_sessions(&self, timeout: i64) -> Result<Vec<Session>> {
        let now = timefmt::now();
        let mut crashed = Vec::new();
        for session in self.list_active_sessions() {
            let elapsed = (now - session.last_seen).num_seconds();
            if elapsed > timeout {
                self.crash_session(&session.id)?;
                crashed.push(session);
            }
        }
        Ok(crashed)
    }

    pub fn save_session(&self, session: &Session) -> Result<()> {
        fs::create_dir_all(&self.sessions_dir)?;
        fs::write(
            self.session_file(&session.id),
            serde_yaml_ng::to_string(session)?,
        )?;
        Ok(())
    }

    pub fn load_session(&self, session_id: &str) -> Option<Session> {
        let file = self.session_file(session_id);
        if !file.exists() {
            return None;
        }
        self.load_session_from_file(&file)
    }

    pub fn load_session_from_file(&self, session_file: &Path) -> Option<Session> {
        let raw = fs::read_to_string(session_file).ok()?;
        serde_yaml_ng::from_str(&raw).ok()
    }
}

/// Approximate token count parsed from the agent's `.kanban/logs/<session>.log`.
pub fn estimate_session_tokens(project_path: &Path, session_id: &str) -> Option<i64> {
    let log_file = project_path
        .join(".kanban")
        .join("logs")
        .join(format!("{session_id}.log"));
    let text = fs::read_to_string(&log_file).ok()?;

    let patterns = [
        r"(?i)(?:total\s+)?tokens(?:\s+used)?\s*[:=]\s*([\d,]+)",
        r"(?i)([\d,]+)\s+(?:total\s+)?tokens\b",
    ];
    for pattern in patterns {
        if let Some(value) = last_capture(&text, pattern) {
            return parse_int(&value);
        }
    }

    let input =
        last_capture(&text, r"(?i)input\s+tokens\s*[:=]\s*([\d,]+)").and_then(|v| parse_int(&v));
    let output =
        last_capture(&text, r"(?i)output\s+tokens\s*[:=]\s*([\d,]+)").and_then(|v| parse_int(&v));
    if input.is_some() || output.is_some() {
        return Some(input.unwrap_or(0) + output.unwrap_or(0));
    }
    None
}

fn last_capture(text: &str, pattern: &str) -> Option<String> {
    let re = Regex::new(pattern).ok()?;
    re.captures_iter(text)
        .last()
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn parse_int(value: &str) -> Option<i64> {
    value.replace(',', "").parse().ok()
}
