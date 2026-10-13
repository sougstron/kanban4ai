//! Portable task failures. Peers annotate a run; only its runner owns the descriptor.
use crate::core::error::Result;
use crate::core::project::ProjectStore;
use crate::core::storage::Storage;
use crate::core::sync::{Journal, NodeRole};
use crate::core::thread::ThreadManager;
use crate::core::timefmt;
use std::path::Path;

pub const POWERED_OFF: &str = "RUN_NODE_POWERED_OFF";
pub const UNREACHABLE: &str = "RUN_NODE_UNREACHABLE";
pub const BACKEND_MISSING: &str = "RUNNER_BACKEND_MISSING";
pub const REPO_MISSING: &str = "RUNNER_REPO_MISSING";

pub fn record(board: &Path, task_id: &str, code: &str, message: &str) -> Result<()> {
    crate::core::sync::with_actor("kanban", || {
        let storage = Storage::new(board);
        let _guard = storage.lock()?;
        let Some(mut task) = storage.load_task(task_id)? else {
            return Ok(());
        };
        let body = format!("{code} — {message}");
        let threads = ThreadManager::new(board)?;
        // The run identity in remote messages makes polling idempotent, even
        // after unrelated messages or another peer's copy of the same note.
        if !threads
            .load(task_id)?
            .messages
            .iter()
            .any(|m| m.body == body)
        {
            threads.post_board_note(task_id, &body)?;
        }
        if task.run_failure.as_deref() != Some(code) {
            task.run_failure = Some(code.into());
            task.updated_at = timefmt::now();
            storage.save_task(&task)?;
        }
        Ok(())
    })
}

pub fn clear(board: &Path, task_id: &str) -> Result<()> {
    let storage = Storage::new(board);
    let _guard = storage.lock()?;
    if let Some(mut task) = storage.load_task(task_id)?
        && task.run_failure.take().is_some()
    {
        task.updated_at = timefmt::now();
        storage.save_task(&task)?;
    }
    Ok(())
}

/// Called by the sync worker as well as explicit probes. A transport outage
/// alone is not proof of a dead runner: require an expired run heartbeat.
pub fn observe(
    root: &Path,
    node_id: &str,
    online: bool,
    bye: Option<(chrono::DateTime<chrono::Utc>, &str)>,
    timeout: i64,
) -> Result<()> {
    let journal = Journal::open(root)?;
    if journal.node.node_id == node_id {
        return Ok(());
    }
    let now = timefmt::now();
    for project in ProjectStore::at(root).list()? {
        let storage = Storage::new(&project.data_root);
        let _guard = storage.lock()?;
        for task in storage.list_unarchived_tasks()? {
            let Some(run) = journal.runs(&project.id, &task.id)?.remove(node_id) else {
                continue;
            };
            let heartbeat = timefmt::parse(&run.heartbeat)
                .map_err(|e| crate::core::error::KanbanError::Invalid(e.to_string()))?;
            let marker = format!("[run {node_id}/{}]", run.generation);
            let threads = ThreadManager::new(&project.data_root)?;
            let previous = threads
                .load(&task.id)?
                .messages
                .into_iter()
                .rev()
                .find(|m| {
                    m.body.contains(&marker)
                        && (m.body.starts_with(POWERED_OFF) || m.body.starts_with(UNREACHABLE))
                });
            if !run.active() {
                if previous.is_some()
                    && matches!(task.run_failure.as_deref(), Some(POWERED_OFF | UNREACHABLE))
                {
                    clear(&project.data_root, &task.id)?;
                }
                continue;
            }
            if online {
                if previous.is_some_and(|m| heartbeat > m.created_at)
                    && matches!(task.run_failure.as_deref(), Some(POWERED_OFF | UNREACHABLE))
                {
                    clear(&project.data_root, &task.id)?;
                }
                continue;
            }
            let node = match run.role {
                NodeRole::Server => "сервер",
                NodeRole::Laptop => "ноут",
                NodeRole::Mobile => "телефон",
            };
            let (code, reason) = if let Some((at, reason)) = bye {
                let at = at.with_timezone(&chrono::Local);
                if heartbeat > at.naive_local() {
                    continue;
                }
                let verb = if reason == "sleep" {
                    "уснул"
                } else {
                    "выключился"
                };
                (
                    POWERED_OFF,
                    format!("прервано: {node} {verb} в {}", at.format("%H:%M")),
                )
            } else {
                if (now - heartbeat).num_seconds() < timeout {
                    continue;
                }
                (
                    UNREACHABLE,
                    format!(
                        "прервано: heartbeat узла {} пропал без объявления",
                        run.name
                    ),
                )
            };
            record(
                &project.data_root,
                &task.id,
                code,
                &format!(
                    "{reason}. После возвращения узла — crash-restart по настройкам; можно перезапустить на другом узле. {marker}"
                ),
            )?;
        }
    }
    Ok(())
}

/// Missing credentials are distinct from exhausted quota and other hard errors.
pub fn authentication_error(event: &serde_json::Value, message: &str) -> bool {
    let status = crate::core::provenance::stream_error_data(event)
        .and_then(|d| d.get("statusCode").or_else(|| d.get("status")))
        .and_then(serde_json::Value::as_i64);
    let message = message.to_ascii_lowercase();
    // OpenCode also uses 401 for spent credits, which is not a missing login.
    if [
        "insufficient balance",
        "insufficient credits",
        "payment required",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
    {
        return false;
    }
    status == Some(401)
        || [
            "not logged in",
            "not authenticated",
            "authentication required",
            "authentication failed",
            "login required",
            "please log in",
            "please login",
            "invalid api key",
            "missing api key",
            "no api key",
            "unauthorized",
        ]
        .iter()
        .any(|pattern| message.contains(pattern))
}

/// Some CLIs fail before opening their JSON stream (missing login). Inspect
/// only diagnostic lines or explicit error events, never normal agent output.
pub fn login_diagnostic(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    while reader.read_line(&mut line).ok()? != 0 {
        let event = serde_json::from_str::<serde_json::Value>(&line).ok();
        let diagnostic = match &event {
            Some(event)
                if event.get("type").and_then(serde_json::Value::as_str) == Some("error")
                    || event.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) =>
            {
                crate::core::provenance::stream_error(event)
                    .map(|e| e.message)
                    .or_else(|| {
                        event
                            .get("result")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .or_else(|| {
                        event
                            .get("errors")
                            .and_then(serde_json::Value::as_array)
                            .and_then(|errors| {
                                errors
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .find(|message| authentication_error(event, message))
                            })
                            .map(str::to_owned)
                    })
            }
            Some(event)
                if event
                    .get("message")
                    .unwrap_or(event)
                    .get("stopReason")
                    .and_then(serde_json::Value::as_str)
                    == Some("error") =>
            {
                event
                    .get("message")
                    .unwrap_or(event)
                    .get("errorMessage")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            }
            None => {
                let text = line.trim();
                let lower = text.to_ascii_lowercase();
                [
                    "error:",
                    "error ",
                    "not logged in",
                    "no api key",
                    "authentication ",
                    "unauthorized",
                    "please log in",
                    "please login",
                ]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
                .then(|| text.to_owned())
            }
            _ => None,
        };
        if let Some(message) = diagnostic
            && authentication_error(event.as_ref().unwrap_or(&serde_json::Value::Null), &message)
        {
            return Some(message);
        }
        line.clear();
    }
    None
}
