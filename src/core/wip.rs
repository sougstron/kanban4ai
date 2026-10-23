//! WIP checkpoints never move a task branch, HEAD, or the agent's index.
use super::{
    error::{KanbanError, Result},
    models::Task,
    project::{Project, ProjectStore},
    session::SessionManager,
    storage::{BoardGuard, Storage, atomic_write_text, lock_file_reentrant},
    sync::Journal,
    vcs,
};
use std::{fs, path::Path};

fn invalid(message: &str) -> KanbanError {
    KanbanError::Invalid(message.into())
}

pub fn reference(task: &str) -> String {
    format!("refs/kanban/wip/{task}")
}

/// The caller holds the board lock, including while stopping the source agent.
pub fn checkpoint(project: &Project, task: &Task) -> Result<String> {
    let _guard = Storage::new(&project.data_root).lock()?;
    let _push = push_lock(project, &task.id)?;
    capture(project, task)?.push(project, &task.id)
}

/// A checkpoint recorded locally whose refs still have to reach the remote.
struct Captured {
    repo: vcs::GitRepo,
    remote: String,
    retained: String,
    tip: String,
}

/// The local half of a checkpoint: snapshot the worktree and move the WIP
/// refs. Needs the board lock; touches no network.
fn capture(project: &Project, task: &Task) -> Result<Captured> {
    let path = task
        .worktree
        .as_ref()
        .map(|rel| project.data_root.join(".kanban/worktrees").join(rel))
        .ok_or_else(|| {
            invalid("RUNNER_REPO_MISSING: continuation requires an isolated worktree")
        })?;
    if !path.is_dir() {
        return Err(invalid("RUNNER_REPO_MISSING: source worktree is missing"));
    }
    vcs::populate_worktree(&path)?;
    let repo = vcs::detect(&path).ok_or_else(|| invalid("RUNNER_REPO_MISSING"))?;
    let tip = repo.snapshot_wip("HEAD")?;
    repo.set_ref(&reference(&task.id), &tip)?;
    let root = super::sync::store_for_board(&project.data_root)
        .ok_or_else(|| invalid("not a synchronized project"))?;
    let journal = Journal::open(root)?;
    let runs = journal.runs(&project.id, &task.id)?;
    let run = runs
        .get(&journal.node.node_id)
        .ok_or_else(|| invalid("local run missing"))?;
    // Retain every runner's checkpoint even if peers overwrite the convenience ref.
    let retained = format!(
        "refs/kanban/wip-runs/{}/{}/{}",
        task.id, run.generation, journal.node.node_id
    );
    repo.set_ref(&retained, &tip)?;
    let remote = super::code::status(root, &project.id)?
        .remote
        .ok_or_else(|| invalid("SYNC_NO_REMOTE"))?;
    Ok(Captured {
        repo,
        remote,
        retained,
        tip: tip.as_str().into(),
    })
}

impl Captured {
    /// The network half: push both refs, then stamp the publication. Callers
    /// hold the task's [`push_lock`].
    fn push(self, project: &Project, task_id: &str) -> Result<String> {
        self.repo.push_checkpoint(&self.remote, &self.retained)?;
        self.repo
            .push_checkpoint(&self.remote, &reference(task_id))?;
        let directory = project.data_root.join(".kanban/wip");
        fs::create_dir_all(&directory)?;
        atomic_write_text(
            &directory.join(format!("{task_id}.published")),
            &super::timefmt::format(&super::timefmt::now()),
        )?;
        Ok(self.tip)
    }
}

/// Serializes one task's WIP pushes, so a slow periodic push that started
/// first cannot land after (and overwrite) a newer continuation checkpoint.
fn push_lock(project: &Project, task_id: &str) -> Result<BoardGuard> {
    let directory = project.data_root.join(".kanban/wip");
    fs::create_dir_all(&directory)?;
    lock_file_reentrant(&directory.join(format!("{task_id}.push.lock")))
}

/// Free the canonical Review branch without removing the stopped source checkout.
pub fn retire(project: &Project, task: &mut Task) -> Result<()> {
    let _guard = Storage::new(&project.data_root).lock()?;
    if task.branch.as_deref() != Some(format!("kanban/{}", task.id).as_str()) {
        return Ok(());
    }
    if task
        .session
        .as_deref()
        .is_some_and(|id| SessionManager::new(&project.data_root).is_session_active(id))
    {
        return Err(invalid("Cannot retire a running source checkout"));
    }
    let repo = vcs::detect(&project.work_path).ok_or_else(|| invalid("RUNNER_REPO_MISSING"))?;
    let old = task.branch.as_deref().expect("canonical branch");
    let tip = repo
        .read_ref(old)?
        .ok_or_else(|| invalid("source branch missing"))?;
    let retained = format!(
        "kanban/{}-retained-{}",
        task.id,
        super::timefmt::now()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap_or_default()
    );
    repo.rename_run_branch(old, &retained, &tip)?;
    task.branch = Some(retained);
    Ok(())
}

/// Serve's existing sync timer and the final bye call this; no independent job.
pub fn publish(root: &Path, interval: u64, force: bool) -> Result<()> {
    if !super::sync::enabled(root) {
        return Ok(());
    }
    let mut failure = None;
    for project in ProjectStore::at(root).list()? {
        let storage = Storage::new(&project.data_root);
        let sessions = SessionManager::new(&project.data_root);
        for task in storage.list_unarchived_tasks()? {
            if task.worktree.is_none()
                || !task
                    .session
                    .as_deref()
                    .is_some_and(|id| sessions.is_session_active(id))
            {
                continue;
            }
            let stamp = project
                .data_root
                .join(".kanban/wip")
                .join(format!("{}.published", task.id));
            if !force
                && fs::metadata(stamp)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_secs() < interval)
            {
                continue;
            }
            // Only the local snapshot holds the board lock. The two pushes
            // to the code remote take seconds per running task, and holding
            // the lock through them stalled every start, queue, and save on
            // the board once a minute. Lock order matches `checkpoint`:
            // board, then push.
            let result = (|| {
                let (captured, _push) = {
                    let _guard = storage.lock()?;
                    let current = storage
                        .load_task(&task.id)?
                        .ok_or_else(|| invalid("task disappeared"))?;
                    if !current
                        .session
                        .as_deref()
                        .is_some_and(|id| sessions.is_session_active(id))
                    {
                        return Ok(());
                    }
                    let push = push_lock(&project, &task.id)?;
                    (capture(&project, &current)?, push)
                };
                captured.push(&project, &task.id).map(drop)
            })();
            if let Err(e) = result {
                failure = Some(e);
            }
        }
    }
    failure.map_or(Ok(()), Err)
}
