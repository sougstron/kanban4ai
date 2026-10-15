//! Bounded, task-qualified runner output. Session identifiers stay machine-local.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::{KanbanError, Result};
use super::models::{Session, SessionStatus};
use super::session::SessionManager;
use super::storage::{Storage, atomic_write_text_raw};
use super::sync::{Journal, RunDescriptor, store_for_board};

pub const MAX_TAIL_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub text: String,
    pub at_ms: u64,
    pub node_id: String,
    /// Stable project UID, never a machine-local path or display alias.
    pub project: String,
    pub task: String,
    /// Reject a cached tail from an earlier run, including on the same node.
    pub generation: u64,
}

fn invalid(message: &str) -> KanbanError {
    KanbanError::Invalid(message.into())
}

fn component(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value.contains("..")
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
    {
        return Err(invalid("Invalid runner log identifier"));
    }
    Ok(())
}

pub fn cache_path(store: &Path, node_id: &str, project: &str, task: &str) -> Result<PathBuf> {
    for value in [node_id, project, task] {
        component(value)?;
    }
    Ok(store
        .join("sync/logs")
        .join(node_id)
        .join(project)
        .join(format!("{task}.json")))
}

/// A historical runner must not serve output after another node took ownership.
fn owned_latest_run(
    store: &Path,
    project: &str,
    task: &str,
    node_id: &str,
) -> Result<RunDescriptor> {
    if !super::sync::enabled(store) {
        return Err(invalid(
            "RUNNER_LOG_UNAVAILABLE: Runner has no run for this task",
        ));
    }
    let runs = Journal::open(store)?.runs(project, task)?;
    let run = runs
        .get(node_id)
        .ok_or_else(|| invalid("RUNNER_LOG_UNAVAILABLE: Runner has no run for this task"))?;
    if runs.values().any(|other| other.generation > run.generation) {
        return Err(invalid(
            "RUNNER_LOG_UNAVAILABLE: Runner no longer owns the latest run",
        ));
    }
    Ok(run.clone())
}

fn identity(board: &Path) -> Result<(&Path, &str)> {
    let store = store_for_board(board)
        .ok_or_else(|| invalid("RUNNER_LOG_UNAVAILABLE: Project is not registered"))?;
    let alias = board
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("Invalid project identifier"))?;
    Ok((store, alias))
}

/// Latest portable run, including finished/failed runs for task log viewers.
pub fn latest_run(board: &Path, task: &str) -> Result<RunDescriptor> {
    component(task)?;
    let (store, alias) = identity(board)?;
    if !super::sync::enabled(store) {
        return Err(invalid("RUNNER_LOG_UNAVAILABLE: Task has no runner log"));
    }
    Journal::open(store)?
        .runs(alias, task)?
        .into_values()
        .max_by(|a, b| (a.generation, &a.node_id).cmp(&(b.generation, &b.node_id)))
        .ok_or_else(|| invalid("RUNNER_LOG_UNAVAILABLE: Task has no runner log"))
}

pub fn validate_snapshot(store: &Path, snapshot: &Snapshot) -> Result<()> {
    if !super::sync::enabled(store) {
        return Err(invalid("Runner log owner is not paired with this store"));
    }
    cache_path(store, &snapshot.node_id, &snapshot.project, &snapshot.task)?;
    let settings = super::sync::relay::settings(store)?;
    if snapshot.text.len() > settings.log_tail_bytes || snapshot.text.len() > MAX_TAIL_BYTES {
        return Err(invalid("Runner log snapshot exceeds configured tail limit"));
    }
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    if snapshot.at_ms == 0
        || snapshot.at_ms.saturating_sub(now) > settings.max_skew.saturating_mul(1000)
    {
        return Err(invalid("Invalid runner log timestamp"));
    }
    let journal = Journal::open(store)?;
    if snapshot.node_id != journal.node.node_id
        && !settings.peers.iter().any(|peer| {
            peer.node.node_id == snapshot.node_id && peer.node.store_id == journal.node.store_id
        })
    {
        return Err(invalid("Runner log owner is not paired with this store"));
    }
    let state = journal.state()?;
    let alias = state
        .project_uids
        .iter()
        .find(|(_, uid)| **uid == snapshot.project)
        .map(|(alias, _)| alias)
        .ok_or_else(|| invalid("Runner log project is not registered"))?;
    if Storage::new(store.join("projects").join(alias))
        .load_task(&snapshot.task)?
        .is_none()
    {
        return Err(invalid("Runner log task does not exist"));
    }
    let run = owned_latest_run(store, alias, &snapshot.task, &snapshot.node_id)?;
    if run.started_at.is_none() {
        return Err(invalid("Runner log run has not started"));
    }
    if snapshot.generation != run.generation {
        return Err(invalid("Runner log belongs to an earlier run"));
    }
    Ok(())
}

pub fn cache_snapshot(store: &Path, snapshot: &Snapshot) -> Result<()> {
    validate_snapshot(store, snapshot)?;
    let path = cache_path(store, &snapshot.node_id, &snapshot.project, &snapshot.task)?;
    fs::create_dir_all(path.parent().expect("qualified cache path"))?;
    atomic_write_text_raw(
        &path,
        &serde_json::to_string(snapshot)
            .map_err(|_| invalid("Cannot encode runner log snapshot"))?,
    )
}

pub fn cached_tail(board: &Path, task: &str, node_id: &str) -> Result<Snapshot> {
    let (store, alias) = identity(board)?;
    if !super::sync::enabled(store) {
        return Err(invalid(
            "RUNNER_LOG_UNAVAILABLE: No cached runner log is available",
        ));
    }
    let uid = Journal::open(store)?
        .state()?
        .project_uids
        .get(alias)
        .cloned()
        .ok_or_else(|| invalid("Runner log project is not registered"))?;
    let path = cache_path(store, node_id, &uid, task)?;
    let limit = super::sync::relay::settings(store)?
        .log_tail_bytes
        .saturating_mul(6)
        .saturating_add(4096);
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| invalid("RUNNER_LOG_UNAVAILABLE: No cached runner log is available"))?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid("Runner log cache exceeds configured limit"));
    }
    let snapshot: Snapshot =
        serde_json::from_slice(&bytes).map_err(|_| invalid("Invalid runner log cache"))?;
    if snapshot.node_id != node_id || snapshot.project != uid || snapshot.task != task {
        return Err(invalid("Runner log cache identity does not match"));
    }
    validate_snapshot(store, &snapshot)?;
    Ok(snapshot)
}

// Open each directory relative to an already-open parent, refusing symlinks and
// special files. A task's stored session is not permission to read arbitrary files.
#[cfg(unix)]
fn log_file(board: &Path, session: &str) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    let mut parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(board)?;
    for (name, directory) in [
        (".kanban".to_owned(), true),
        ("logs".to_owned(), true),
        (format!("{session}.log"), false),
    ] {
        let name =
            std::ffi::CString::new(name).map_err(|_| invalid("Invalid runner log identifier"))?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        parent = unsafe { File::from_raw_fd(fd) };
    }
    if !parent.metadata()?.is_file() {
        return Err(invalid("Runner log is not a regular file"));
    }
    Ok(parent)
}

#[cfg(not(unix))]
fn log_file(board: &Path, session: &str) -> Result<File> {
    let mut path = board.to_path_buf();
    for name in [
        ".kanban".to_owned(),
        "logs".to_owned(),
        format!("{session}.log"),
    ] {
        path.push(name);
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(invalid("Runner log symlinks are not allowed"));
        }
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("Runner log is not a regular file"));
    }
    Ok(file)
}

fn local_session(board: &Path, task: &str) -> Result<Session> {
    component(task)?;
    let current = Storage::new(board)
        .load_task(task)?
        .ok_or_else(|| invalid("Not found"))?;
    let id = current
        .session
        .ok_or_else(|| invalid("RUNNER_LOG_UNAVAILABLE: Task has no local session"))?;
    SessionManager::validate_session_id(&id).map_err(|_| invalid("Invalid runner session"))?;
    let session = SessionManager::new(board)
        .load_session(&id)
        .ok_or_else(|| invalid("RUNNER_LOG_UNAVAILABLE: Runner session is unavailable"))?;
    if session.task_id != task || session.id != id {
        return Err(invalid("Runner session does not belong to this task"));
    }
    Ok(session)
}

pub fn local_tail(board: &Path, task: &str) -> Result<Snapshot> {
    let _guard = Storage::new(board).lock()?;
    let (store, alias) = identity(board)?;
    let settings = super::sync::relay::settings(store)?;
    let journal = Journal::open(store)?;
    let session = local_session(board, task)?;
    let run = owned_latest_run(store, alias, task, &journal.node.node_id)?;
    if run.started_at.as_deref() != Some(super::timefmt::format(&session.started_at).as_str()) {
        return Err(invalid(
            "RUNNER_LOG_UNAVAILABLE: Session does not match the latest run",
        ));
    }
    let mut file = log_file(board, &session.id)
        .map_err(|_| invalid("RUNNER_LOG_UNAVAILABLE: Runner log cannot be read"))?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(
        length.saturating_sub(settings.log_tail_bytes as u64),
    ))?;
    let mut bytes = Vec::with_capacity(settings.log_tail_bytes.min(length as usize));
    file.take(settings.log_tail_bytes as u64)
        .read_to_end(&mut bytes)?;
    // Skip an incomplete UTF-8 code point at the start and replace malformed
    // bytes elsewhere; truncation must remain bounded even after replacement.
    let start = bytes
        .iter()
        .position(|b| b & 0xc0 != 0x80)
        .unwrap_or(bytes.len());
    let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if text.contains(&session.id) {
        text = text.replace(&session.id, "[session]");
    }
    if let Some(project) = super::project::ProjectStore::at(store).get(alias)? {
        for path in [board, project.work_path.as_path(), store] {
            let path = path.to_string_lossy();
            if !path.is_empty() && text.contains(path.as_ref()) {
                text = text.replace(path.as_ref(), "[project]");
            }
        }
    }
    if text.len() > settings.log_tail_bytes {
        let mut start = text.len() - settings.log_tail_bytes;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text.drain(..start);
    }
    let project = journal
        .state()?
        .project_uids
        .get(alias)
        .cloned()
        .ok_or_else(|| invalid("Runner log project is not registered"))?;
    let snapshot = Snapshot {
        text,
        at_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
        node_id: journal.node.node_id,
        project,
        task: task.into(),
        generation: run.generation,
    };
    validate_snapshot(store, &snapshot)?;
    Ok(snapshot)
}

/// Follow only the selected local session; a replacement run ends the stream.
pub fn local_follow_token(board: &Path, task: &str) -> Result<String> {
    Ok(local_session(board, task)?.id)
}

pub fn local_follow_tail(
    board: &Path,
    task: &str,
    token: &str,
) -> Result<(Option<Snapshot>, bool)> {
    let _guard = Storage::new(board).lock()?;
    let Some(task_state) = Storage::new(board).load_task(task)? else {
        return Ok((None, false));
    };
    if task_state.session.as_deref() != Some(token) {
        return Ok((None, false));
    }
    let session = local_session(board, task)?;
    let snapshot = local_tail(board, task)?;
    Ok((Some(snapshot), session.status == SessionStatus::Active))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::project::ProjectStore;
    use crate::core::storage::NewTask;

    fn fixture(limit: usize) -> (tempfile::TempDir, PathBuf, String, String) {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let work = temp.path().join("work");
        fs::create_dir_all(&store).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::write(
            store.join("config.yaml"),
            format!("sync:\n  log_tail_bytes: {limit}\n"),
        )
        .unwrap();
        crate::core::sync::enable_sync(&store).unwrap();
        let project = ProjectStore::at(&store).add(&work, None).unwrap().project;
        let storage = Storage::new(&project.data_root);
        storage.init_board().unwrap();
        let mut task = crate::core::operations::Operations::for_project(&project)
            .create_task(NewTask::titled("Log test"))
            .unwrap();
        let session = "ses-runner-log-test".to_owned();
        SessionManager::new(&project.data_root)
            .link_session(&task.id, &session)
            .unwrap();
        task.session = Some(session.clone());
        storage.save_task(&task).unwrap();
        fs::create_dir_all(project.data_root.join(".kanban/logs")).unwrap();
        (temp, project.data_root, task.id, session)
    }

    #[test]
    fn tail_is_bounded_utf8_and_finished_run_remains_readable() {
        let (_temp, board, task, session) = fixture(17);
        fs::write(
            board.join(format!(".kanban/logs/{session}.log")),
            "é".repeat(100),
        )
        .unwrap();
        let snapshot = local_tail(&board, &task).unwrap();
        assert!(snapshot.text.len() <= 17);
        assert_eq!(snapshot.text, "é".repeat(8));
        SessionManager::new(&board).close_session(&session).unwrap();
        assert_eq!(local_tail(&board, &task).unwrap().text, snapshot.text);
        let (last, running) = local_follow_tail(&board, &task, &session).unwrap();
        assert!(last.is_some());
        assert!(!running);
    }

    #[test]
    fn snapshot_redacts_private_identity_and_rejects_old_generation() {
        let (_temp, board, task, session) = fixture(65536);
        fs::write(
            board.join(format!(".kanban/logs/{session}.log")),
            format!("session={session}\nboard={}\n", board.display()),
        )
        .unwrap();
        let snapshot = local_tail(&board, &task).unwrap();
        assert!(!snapshot.text.contains(&session));
        assert!(!snapshot.text.contains(board.to_str().unwrap()));
        let store = store_for_board(&board).unwrap();
        cache_snapshot(store, &snapshot).unwrap();
        assert_eq!(
            cached_tail(&board, &task, &snapshot.node_id).unwrap(),
            snapshot
        );
        let mut old = snapshot;
        old.generation = old.generation.saturating_sub(1);
        assert!(validate_snapshot(store, &old).is_err());
        assert!(local_tail(&board, "../outside").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_log_is_rejected() {
        let (temp, board, task, session) = fixture(65536);
        let outside = temp.path().join("private");
        fs::write(&outside, "not runner output").unwrap();
        std::os::unix::fs::symlink(outside, board.join(format!(".kanban/logs/{session}.log")))
            .unwrap();
        assert!(local_tail(&board, &task).is_err());
    }
}
