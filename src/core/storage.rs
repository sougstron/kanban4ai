//! Filesystem layer for tasks. The only module that touches `.kanban/tasks/`.
//!
//! Invariants carried over from the Python implementation:
//! - every write is atomic (temp file in the same dir + rename);
//! - a task's status IS the subdirectory it lives in
//!   (`.kanban/tasks/<status>/TASK-NNN.md`), so moving a task moves the file;
//! - any read-modify-write cycle must hold the board-wide lock
//!   (`.kanban/.lock`, flock) so concurrent `kanban` processes cannot
//!   resurrect a moved task as a phantom duplicate.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use fs2::FileExt;

use crate::core::config::Config;
use crate::core::error::{KanbanError, Result};
use crate::core::models::{Task, TaskStatus};
use crate::core::thread::ThreadManager;
use crate::core::timefmt;

/// Atomically replace `path` with `content` (temp file + rename).
pub fn atomic_write_text(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| KanbanError::Invalid(format!("path has no parent: {}", path.display())))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp_")
        .tempfile_in(parent)?;
    tmp.write_all(content.as_bytes())?;
    tmp.persist(path).map_err(|e| KanbanError::Io(e.error))?;
    Ok(())
}

struct LockEntry {
    file: File,
    depth: usize,
}

thread_local! {
    /// Per-thread registry of held board locks, keyed by lock-file path.
    /// Makes the flock reentrant within one thread (nested `lock()` calls in
    /// the same call stack don't deadlock), same as the Python `_BoardLock`.
    static BOARD_LOCKS: RefCell<HashMap<PathBuf, LockEntry>> = RefCell::new(HashMap::new());
}

/// RAII guard for a reentrant flock; released (or de-nested) on drop.
pub struct BoardGuard {
    key: PathBuf,
}

impl Drop for BoardGuard {
    fn drop(&mut self) {
        BOARD_LOCKS.with(|locks| {
            let mut locks = locks.borrow_mut();
            let remove = match locks.get_mut(&self.key) {
                Some(entry) => {
                    entry.depth -= 1;
                    entry.depth == 0
                }
                None => false,
            };
            if remove && let Some(entry) = locks.remove(&self.key) {
                let _ = FileExt::unlock(&entry.file);
            }
        });
    }
}

/// Acquire an exclusive flock on `lock_path` (created if missing; its parent
/// directory must exist). Reentrant within a thread, like the board lock —
/// nested calls for the same file bump a depth counter instead of deadlocking.
pub fn lock_file_reentrant(lock_path: &Path) -> Result<BoardGuard> {
    // Canonicalize the parent so every alias of the same file maps to one key.
    let key = match (lock_path.parent(), lock_path.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(name),
        _ => lock_path.to_path_buf(),
    };
    BOARD_LOCKS
        .with(|locks| {
            let mut locks = locks.borrow_mut();
            if let Some(entry) = locks.get_mut(&key) {
                entry.depth += 1;
                return Ok(());
            }
            let file = File::create(lock_path)?;
            file.lock_exclusive()?;
            locks.insert(key.clone(), LockEntry { file, depth: 1 });
            Ok(())
        })
        .map(|_: ()| BoardGuard { key })
        .map_err(KanbanError::Io)
}

/// Change-detection signature: `(file_count, max_mtime_ns, total_bytes)`.
pub type Fingerprint = (u64, u128, u64);

pub struct Storage {
    pub project_path: PathBuf,
    pub kanban_dir: PathBuf,
    pub tasks_dir: PathBuf,
    pub context_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub threads_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub provenance_dir: PathBuf,
    pub backups_dir: PathBuf,
    pub assets_dir: PathBuf,
    /// Root of the per-task git worktrees (`.kanban/worktrees/<TASK-ID>`).
    /// Under the data root, never inside work_path, so a worktree can never
    /// be swept into the user's own commits or .gitignore.
    pub worktrees_dir: PathBuf,
}

impl Storage {
    pub fn new(project_path: impl AsRef<Path>) -> Self {
        let project_path = project_path.as_ref().to_path_buf();
        let kanban_dir = project_path.join(".kanban");
        Storage {
            tasks_dir: kanban_dir.join("tasks"),
            context_dir: kanban_dir.join("context"),
            sessions_dir: kanban_dir.join("sessions"),
            threads_dir: kanban_dir.join("threads"),
            logs_dir: kanban_dir.join("logs"),
            provenance_dir: kanban_dir.join("provenance"),
            backups_dir: kanban_dir.join("backups"),
            assets_dir: kanban_dir.join("assets"),
            worktrees_dir: kanban_dir.join("worktrees"),
            kanban_dir,
            project_path,
        }
    }

    /// Acquire the board-wide lock. Wrap any read-modify-write cycle in this
    /// so concurrent `kanban` processes serialize. Reentrant within a thread.
    pub fn lock(&self) -> Result<BoardGuard> {
        fs::create_dir_all(&self.kanban_dir)?;
        lock_file_reentrant(&self.kanban_dir.join(".lock"))
    }

    pub fn init_board(&self) -> Result<()> {
        fs::create_dir_all(&self.kanban_dir)?;
        fs::create_dir_all(&self.tasks_dir)?;
        fs::create_dir_all(&self.context_dir)?;
        fs::create_dir_all(&self.sessions_dir)?;
        fs::create_dir_all(&self.threads_dir)?;
        fs::create_dir_all(&self.logs_dir)?;
        fs::create_dir_all(&self.provenance_dir)?;
        fs::create_dir_all(&self.backups_dir)?;
        fs::create_dir_all(self.assets_dir.join("images"))?;
        fs::create_dir_all(&self.worktrees_dir)?;
        for status in TaskStatus::ALL {
            fs::create_dir_all(self.tasks_dir.join(status.as_str()))?;
        }
        let config = Config::new(&self.project_path);
        if !config.exists() {
            config.init()?;
        }
        Ok(())
    }

    fn status_dirs(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.tasks_dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect()
    }

    fn get_task_path(&self, task_id: &str) -> Option<PathBuf> {
        for status_dir in self.status_dirs() {
            let task_file = status_dir.join(format!("{task_id}.md"));
            if task_file.exists() {
                return Some(task_file);
            }
        }
        None
    }

    pub fn parse_task_file(&self, filepath: &Path) -> Result<Task> {
        let content = fs::read_to_string(filepath)?;
        let mut parts = content.splitn(3, "---");
        let (_, frontmatter, body) = match (parts.next(), parts.next(), parts.next()) {
            (Some(head), Some(frontmatter), Some(body)) if head.trim().is_empty() => {
                (head, frontmatter, body)
            }
            _ => return Err(KanbanError::InvalidTaskFile(filepath.to_path_buf())),
        };
        let mut task: Task = serde_yaml_ng::from_str(frontmatter)?;
        task.description = body.trim().to_string();
        Ok(task)
    }

    fn write_task_file(&self, filepath: &Path, task: &Task) -> Result<()> {
        let frontmatter = timefmt::quote_yaml_timestamp_fields(
            &serde_yaml_ng::to_string(task)?,
            &["created_at", "updated_at", "completed_at", "restart_at"],
        );
        let content = format!("---\n{frontmatter}---\n{}\n", task.description);
        atomic_write_text(filepath, &content)
    }

    pub fn create_task(&self, new_task: NewTask) -> Result<Task> {
        self.create_task_in_status(new_task, TaskStatus::Todo)
    }

    pub fn create_task_in_status(&self, new_task: NewTask, status: TaskStatus) -> Result<Task> {
        let _guard = self.lock()?;
        let task_id = self.get_next_id()?;
        let mut task = Task::new(task_id.clone(), new_task.title);
        task.description = new_task.description;
        task.ai_model = new_task.ai_model;
        task.ai_effort = new_task.ai_effort;
        task.agent_backend = new_task.agent_backend;
        task.agent_name = new_task.agent_name;
        task.interactive = new_task.interactive;
        task.use_designer = new_task.use_designer;
        task.use_reviewer = new_task.use_reviewer;
        task.use_orchestrator = new_task.use_orchestrator;
        task.chained_to = new_task.chained_to;
        task.depends_on = new_task.depends_on;
        task.needs = new_task.needs;
        task.parent_task = new_task.parent_task;
        task.role_profile = new_task.role_profile;
        task.status = status;
        if matches!(status, TaskStatus::Review | TaskStatus::Done) {
            task.completed_at = Some(task.created_at);
        }

        let filepath = self
            .tasks_dir
            .join(task.status.as_str())
            .join(format!("{task_id}.md"));
        if let Some(parent) = filepath.parent() {
            fs::create_dir_all(parent)?;
        }
        self.write_task_file(&filepath, &task)?;
        let threads = ThreadManager::new(&self.project_path)?;
        // The id is fresh (`max + 1`), so any thread already sitting on it is a
        // leftover of a deleted task; adopting it would show the new task the
        // old task's messages and feed them to its agent.
        threads.discard_thread(&task_id)?;
        threads.initialize_task_thread(&task)?;
        Ok(task)
    }

    pub fn load_task(&self, task_id: &str) -> Result<Option<Task>> {
        match self.get_task_path(task_id) {
            Some(path) => Ok(Some(self.parse_task_file(&path)?)),
            None => Ok(None),
        }
    }

    pub fn save_task(&self, task: &Task) -> Result<()> {
        {
            let _guard = self.lock()?;
            let new_path = self
                .tasks_dir
                .join(task.status.as_str())
                .join(format!("{}.md", task.id));
            if let Some(parent) = new_path.parent() {
                fs::create_dir_all(parent)?;
            }
            // Write the canonical copy first (atomic temp+rename) so the task is
            // never absent from disk, then self-heal by sweeping every status dir
            // and removing any other copy of this id. Under the board lock the
            // sweep can never delete a live concurrent copy.
            self.write_task_file(&new_path, task)?;
            for status_dir in self.status_dirs() {
                let other = status_dir.join(format!("{}.md", task.id));
                if other != new_path && other.exists() {
                    let _ = fs::remove_file(&other);
                }
            }
        }
        ThreadManager::new(&self.project_path)?.update_task_message(task)?;
        Ok(())
    }

    pub fn delete_task(&self, task_id: &str) -> Result<bool> {
        match self.get_task_path(task_id) {
            Some(path) => {
                fs::remove_file(path)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn task_exists(&self, task_id: &str) -> bool {
        self.get_task_path(task_id).is_some()
    }

    pub fn get_next_id(&self) -> Result<String> {
        let mut max_num: u64 = 0;
        for status_dir in self.status_dirs() {
            for num in task_numbers_in(&status_dir) {
                max_num = max_num.max(num);
            }
        }
        Ok(format!("TASK-{:03}", max_num + 1))
    }

    pub fn list_tasks(&self, status: Option<&str>) -> Result<Vec<Task>> {
        let status_dirs = match status {
            Some(status) => vec![self.tasks_dir.join(status)],
            None => self.status_dirs(),
        };
        let mut tasks = Vec::new();
        for status_dir in status_dirs {
            if !status_dir.is_dir() {
                continue;
            }
            for path in task_files_in(&status_dir) {
                if let Ok(task) = self.parse_task_file(&path) {
                    tasks.push(task);
                }
            }
        }
        Ok(tasks)
    }

    pub fn get_all_tasks(&self) -> Result<Vec<Task>> {
        self.list_tasks(None)
    }

    pub fn get_tasks_by_status(&self, status: &str) -> Result<Vec<Task>> {
        self.list_tasks(Some(status))
    }

    pub fn move_task(
        &self,
        task_id: &str,
        target_status: &str,
        is_agent: bool,
    ) -> Result<Option<Task>> {
        let Some(mut task) = self.load_task(task_id)? else {
            return Ok(None);
        };
        let previous_status = task.status;
        let next_status = TaskStatus::from_str(target_status)?;
        let now = timefmt::now();
        task.status = next_status;
        task.updated_at = now;
        if previous_status != next_status
            && matches!(next_status, TaskStatus::Review | TaskStatus::Done)
        {
            task.completed_at = Some(now);
        }
        // An agent moving a task into Review marks it unseen so the board shows
        // the review notifier until the user opens the detail. A human move
        // always clears it: the user is acting on the task directly.
        if is_agent && previous_status != TaskStatus::Review && next_status == TaskStatus::Review {
            task.review_unseen = true;
        } else if !is_agent {
            task.review_unseen = false;
        }
        self.save_task(&task)?;
        Ok(Some(task))
    }

    /// Cheap change-detection signature `(file_count, max_mtime_ns, total_bytes)`
    /// over every `TASK-*.md`, without parsing YAML. Status dir mtimes are
    /// included so a task moved between columns (rename, count unchanged) is
    /// still detected.
    ///
    /// The total size is part of the signature because mtime alone is not
    /// reliable enough: filesystems differ in timestamp granularity (some
    /// carry only whole seconds), so a write landing inside the same tick as
    /// the previous read would leave the signature unchanged and the TUI
    /// would sit on a stale board. Sizes come from the `stat` already being
    /// made for the mtime, so this costs nothing extra.
    pub fn tasks_fingerprint(&self) -> Fingerprint {
        let mut count: u64 = 0;
        let mut max_mtime: u128 = 0;
        let mut total_size: u64 = 0;
        for status_dir in self.status_dirs() {
            if let Some((mtime, _)) = stat_ns(&status_dir) {
                max_mtime = max_mtime.max(mtime);
            }
            for path in task_files_in(&status_dir) {
                count += 1;
                if let Some((mtime, size)) = stat_ns(&path) {
                    max_mtime = max_mtime.max(mtime);
                    total_size = total_size.saturating_add(size);
                }
            }
        }
        (count, max_mtime, total_size)
    }

    /// Cheap signature for every file source rendered by the TUI. In addition
    /// to tasks, detail and sessions views depend on sidecar thread/session
    /// files, so task-only change detection is not sufficient.
    pub fn tui_fingerprint(&self) -> Fingerprint {
        let (mut count, mut max_mtime, mut total_size) = self.tasks_fingerprint();
        for directory in [&self.threads_dir, &self.sessions_dir] {
            if let Some((mtime, _)) = stat_ns(directory) {
                max_mtime = max_mtime.max(mtime);
            }
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
                if !path.is_file() {
                    continue;
                }
                count += 1;
                if let Some((mtime, size)) = stat_ns(&path) {
                    max_mtime = max_mtime.max(mtime);
                    total_size = total_size.saturating_add(size);
                }
            }
        }
        (count, max_mtime, total_size)
    }
}

/// Parameters for [`Storage::create_task`].
#[derive(Debug, Default, Clone)]
pub struct NewTask {
    pub title: String,
    pub description: String,
    pub ai_model: Option<String>,
    pub ai_effort: Option<String>,
    pub agent_backend: Option<String>,
    pub agent_name: Option<String>,
    pub interactive: bool,
    pub use_designer: bool,
    pub use_reviewer: bool,
    pub use_orchestrator: bool,
    pub chained_to: Option<String>,
    /// DAG edges for an orchestrator-planned node (see [`Task::depends_on`]).
    pub depends_on: Vec<String>,
    pub needs: Option<String>,
    pub parent_task: Option<String>,
    pub role_profile: Option<String>,
}

impl NewTask {
    pub fn titled(title: impl Into<String>) -> Self {
        NewTask {
            title: title.into(),
            ..Default::default()
        }
    }
}

fn task_files_in(status_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(status_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && task_number(p).is_some())
        .collect()
}

fn task_numbers_in(status_dir: &Path) -> Vec<u64> {
    task_files_in(status_dir)
        .iter()
        .filter_map(|p| task_number(p))
        .collect()
}

/// `TASK-042.md` → `Some(42)`; anything else → `None`.
fn task_number(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let digits = name.strip_prefix("TASK-")?.strip_suffix(".md")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn stat_ns(path: &Path) -> Option<(u128, u64)> {
    let metadata = fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((mtime, metadata.len()))
}
