//! Agent session tracking (`.kanban/sessions/<id>.yaml`): linking sessions to
//! tasks, heartbeats, and crash detection via heartbeat timeout.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::core::error::{KanbanError, Result};
use crate::core::models::{MessageKind, MessageRole, Session, SessionStatus};
use crate::core::stats;
use crate::core::storage::{Storage, atomic_write_text};
use crate::core::telemetry;
use crate::core::thread::ThreadManager;
use crate::core::timefmt;

/// Display liveness of a session, computed from its record and heartbeat age.
/// Unlike `SessionStatus` this never requires mutating the session file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Live,
    /// The agent declared a wait (`kanban waiting`) whose deadline has not
    /// passed yet; the session counts as alive even without heartbeats.
    Waiting,
    Crashed,
}

fn state_of(
    session: &Session,
    heartbeat_timeout: i64,
    now: &chrono::NaiveDateTime,
) -> SessionState {
    if session.status != SessionStatus::Active {
        return SessionState::Crashed;
    }
    if session.wait_until.is_some_and(|deadline| *now <= deadline) {
        return SessionState::Waiting;
    }
    if (*now - session.last_seen).num_seconds() <= heartbeat_timeout {
        SessionState::Live
    } else {
        SessionState::Crashed
    }
}

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

    pub fn validate_session_id(session_id: &str) -> Result<()> {
        let valid = !session_id.is_empty()
            && session_id.len() <= 128
            && session_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
            && !session_id.contains("..");
        if valid {
            Ok(())
        } else {
            Err(KanbanError::Invalid(format!(
                "Invalid session id: {session_id}"
            )))
        }
    }

    pub fn link_session(&self, task_id: &str, session_id: &str) -> Result<Session> {
        Self::validate_session_id(session_id)?;
        let session = Session::new(session_id, task_id);
        self.save_session(&session)?;
        self.record_stats_running_start(task_id);
        Ok(session)
    }

    pub fn link_named_session(
        &self,
        task_id: &str,
        session_id: &str,
        name: &str,
    ) -> Result<Session> {
        Self::validate_session_id(session_id)?;
        let session = Session::named(session_id, task_id, name);
        self.save_session(&session)?;
        self.record_stats_running_start(task_id);
        Ok(session)
    }

    /// Every session start is a `Running` phase beginning, whatever role it
    /// runs as (executor/designer/reviewer) or path claimed it (dispatcher,
    /// direct launch, revoke). Tags come off the task's current launch fields
    /// — best effort: a task that vanished between the caller's own lookup
    /// and this one just yields untagged stats instead of failing the launch.
    fn record_stats_running_start(&self, task_id: &str) {
        let tags = Storage::new(&self.project_path)
            .load_task(task_id)
            .ok()
            .flatten()
            .map(|task| stats::Tags::from_task(&task))
            .unwrap_or_default();
        stats::record_enter(&self.project_path, task_id, stats::Phase::Running, &tags);
    }

    /// End whichever phase a session was actually in when it stopped
    /// (`Waiting` if a declared wait was still pending, `Running` otherwise),
    /// and tally its final tokens. Called from [`Self::close_session`] and
    /// [`Self::crash_session`] before the session record is mutated, so
    /// `session` still carries its pre-close `wait_until`/status.
    fn record_stats_session_end(&self, session: &Session) {
        if session.status != SessionStatus::Active {
            return; // already ended — never double-record a re-close.
        }
        let phase = if session.wait_until.is_some() {
            stats::Phase::Waiting
        } else {
            stats::Phase::Running
        };
        stats::record_exit(&self.project_path, &session.task_id, phase);

        let Some(task) = Storage::new(&self.project_path)
            .load_task(&session.task_id)
            .ok()
            .flatten()
        else {
            return;
        };
        let backend = task.agent_backend.as_deref().unwrap_or("claude");
        let progress = telemetry::read_session_progress(&self.project_path, &session.id, backend);
        if let Some(tokens) = progress.tokens {
            stats::record_usage(
                &self.project_path,
                &session.task_id,
                &session.id,
                tokens,
                &stats::Tags::from_task(&task),
            );
        }
    }

    pub fn unlink_session(&self, session_id: &str) {
        if Self::validate_session_id(session_id).is_err() {
            return;
        }
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

    /// Non-closed sessions with their computed display state: `Live` for an
    /// active session with a fresh heartbeat, `Crashed` for explicitly crashed
    /// ones and actives whose heartbeat went stale. Read-only: unlike
    /// `check_sessions`, nothing is marked crashed on disk.
    pub fn list_sessions_with_state(&self, heartbeat_timeout: i64) -> Vec<(Session, SessionState)> {
        let now = timefmt::now();
        self.list_sessions()
            .into_iter()
            .filter(|s| s.status != SessionStatus::Closed)
            .map(|s| {
                let state = state_of(&s, heartbeat_timeout, &now);
                (s, state)
            })
            .collect()
    }

    /// Display state of one session; `None` for missing or closed sessions.
    pub fn session_state(&self, session_id: &str, heartbeat_timeout: i64) -> Option<SessionState> {
        let session = self.load_session(session_id)?;
        if session.status == SessionStatus::Closed {
            return None;
        }
        Some(state_of(&session, heartbeat_timeout, &timefmt::now()))
    }

    pub fn heartbeat(&self, session_id: &str) -> Result<()> {
        Self::validate_session_id(session_id)?;
        let _guard = Storage::new(&self.project_path).lock()?;
        if let Some(mut session) = self.load_session(session_id) {
            session.last_seen = timefmt::now();
            self.save_session(&session)?;
        }
        Ok(())
    }

    /// Record a wait the agent declared: the session stays alive (no crash
    /// detection) until `wait_until` passes. Returns `false` when the session
    /// is missing or not active.
    pub fn set_wait(
        &self,
        session_id: &str,
        wait_until: chrono::NaiveDateTime,
        note: Option<String>,
    ) -> Result<bool> {
        Self::validate_session_id(session_id)?;
        let _guard = Storage::new(&self.project_path).lock()?;
        let Some(mut session) = self.load_session(session_id) else {
            return Ok(false);
        };
        if session.status != SessionStatus::Active {
            return Ok(false);
        }
        // Only a *fresh* wait is a Running→Waiting transition; renewing an
        // already-declared wait (a new ETA before the old one expires) must
        // not re-close a Running span that already ended at the first call.
        let was_waiting = session.wait_until.is_some();
        session.wait_until = Some(wait_until);
        session.wait_note = note;
        session.wait_exited = false;
        session.last_seen = timefmt::now();
        self.save_session(&session)?;
        if !was_waiting {
            stats::record_exit(&self.project_path, &session.task_id, stats::Phase::Running);
            stats::record_enter(
                &self.project_path,
                &session.task_id,
                stats::Phase::Waiting,
                &stats::Tags::default(),
            );
        }
        Ok(true)
    }

    /// Mark that the agent process exited while its declared wait is still
    /// pending. The session stays active; the deadline relaunch picks it up.
    pub fn mark_wait_exited(&self, session_id: &str) -> Result<()> {
        Self::validate_session_id(session_id)?;
        let _guard = Storage::new(&self.project_path).lock()?;
        if let Some(mut session) = self.load_session(session_id)
            && session.status == SessionStatus::Active
        {
            session.wait_exited = true;
            self.save_session(&session)?;
        }
        Ok(())
    }

    pub fn close_session(&self, session_id: &str) -> Result<()> {
        Self::validate_session_id(session_id)?;
        let _guard = Storage::new(&self.project_path).lock()?;
        if let Some(mut session) = self.load_session(session_id) {
            self.record_stats_session_end(&session);
            session.status = SessionStatus::Closed;
            session.ended_at = Some(timefmt::now());
            self.save_session(&session)?;
        }
        Ok(())
    }

    /// Mark an active session crashed and record a system message on its task.
    pub fn crash_session(&self, session_id: &str) -> Result<()> {
        Self::validate_session_id(session_id)?;
        let _guard = Storage::new(&self.project_path).lock()?;
        let Some(mut session) = self.load_session(session_id) else {
            return Ok(());
        };
        if session.status != SessionStatus::Active {
            return Ok(());
        }
        self.record_stats_session_end(&session);
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

    /// Crash every active session whose heartbeat is older than `timeout`
    /// seconds. Sessions inside a declared, unexpired wait are exempt: the
    /// agent said it is waiting, so a silent heartbeat is not a crash.
    pub fn check_sessions(&self, timeout: i64) -> Result<Vec<Session>> {
        let now = timefmt::now();
        let mut crashed = Vec::new();
        for session in self.list_active_sessions() {
            if session.wait_until.is_some_and(|deadline| now <= deadline) {
                continue;
            }
            let elapsed = (now - session.last_seen).num_seconds();
            if elapsed > timeout {
                self.crash_session(&session.id)?;
                crashed.push(session);
            }
        }
        Ok(crashed)
    }

    pub fn save_session(&self, session: &Session) -> Result<()> {
        Self::validate_session_id(&session.id)?;
        let _guard = Storage::new(&self.project_path).lock()?;
        fs::create_dir_all(&self.sessions_dir)?;
        atomic_write_text(
            &self.session_file(&session.id),
            timefmt::quote_yaml_timestamp_fields(
                &serde_yaml_ng::to_string(session)?,
                &["started_at", "last_seen", "ended_at", "wait_until"],
            )
            .as_str(),
        )?;
        Ok(())
    }

    pub fn load_session(&self, session_id: &str) -> Option<Session> {
        if Self::validate_session_id(session_id).is_err() {
            return None;
        }
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
    if SessionManager::validate_session_id(session_id).is_err() {
        return None;
    }
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
