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
use serde::{Deserialize, Serialize};
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
    remote: Option<String>,
    retained: String,
    tip: String,
    tree: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Published {
    remote: String,
    #[serde(default)]
    retained: String,
    #[serde(default)]
    refs: Vec<String>,
    tree: String,
    at: String,
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
    let tree = repo.tree_oid(tip.as_str())?.as_str().to_owned();
    let remote = super::code::publication_remote(root, project)?;
    Ok(Captured {
        repo,
        remote,
        retained,
        tip: tip.as_str().into(),
        tree,
    })
}

impl Captured {
    /// The network half: push both refs, then stamp the publication. Callers
    /// hold the task's [`push_lock`].
    fn push(self, project: &Project, task_id: &str) -> Result<String> {
        let Some(remote) = self.remote else {
            // Keep both refs locally. A future heartbeat/reconcile will send
            // the latest objects after code_remote is configured.
            return Ok(self.tip);
        };
        let directory = project.data_root.join(".kanban/wip");
        fs::create_dir_all(&directory)?;
        let stamp = directory.join(format!("{task_id}.published"));
        let previous = fs::read_to_string(&stamp)
            .ok()
            .and_then(|text| serde_yaml_ng::from_str::<Published>(&text).ok());
        if previous.as_ref().is_some_and(|published| {
            published.remote == remote
                && published.tree == self.tree
                && (published.retained == self.retained
                    || published.refs.iter().any(|name| name == &self.retained))
        }) {
            // Refresh cadence without a network call; commit-tree may have
            // produced a different commit ID for this identical tree.
            atomic_write_text(
                &stamp,
                &serde_yaml_ng::to_string(&Published {
                    remote,
                    retained: self.retained,
                    refs: Vec::new(),
                    tree: self.tree,
                    at: super::timefmt::format(&super::timefmt::now()),
                })?,
            )?;
            return Ok(self.tip);
        }
        self.repo.push_checkpoint(&remote, &self.retained)?;
        self.repo.push_checkpoint(&remote, &reference(task_id))?;
        atomic_write_text(
            &stamp,
            &serde_yaml_ng::to_string(&Published {
                remote,
                retained: self.retained,
                refs: Vec::new(),
                tree: self.tree,
                at: super::timefmt::format(&super::timefmt::now()),
            })?,
        )?;
        Ok(self.tip)
    }
}

/// Send a checkpoint captured while no publication remote existed. This path
/// never creates a new commit and its marker makes later idle ticks network
/// no-ops.
fn publish_pending(project: &Project, task_id: &str) -> Result<()> {
    let Some(root) = super::sync::store_for_board(&project.data_root) else {
        return Ok(());
    };
    let Some(remote) = super::code::publication_remote(root, project)? else {
        return Ok(());
    };
    let Some(repo) = vcs::detect(&project.work_path) else {
        return Ok(());
    };
    let canonical = reference(task_id);
    let Some(tip) = repo.read_ref(&canonical)? else {
        return Ok(());
    };
    let prefix = format!("refs/kanban/wip-runs/{task_id}/");
    let refs: Vec<_> = repo
        .refs_with_prefixes(std::slice::from_ref(&prefix))?
        .into_iter()
        .filter(|name| name.starts_with(&prefix))
        .collect();
    let tree = repo.tree_oid(tip.as_str())?.as_str().to_owned();
    let directory = project.data_root.join(".kanban/wip");
    fs::create_dir_all(&directory)?;
    let stamp = directory.join(format!("{task_id}.published"));
    let previous = fs::read_to_string(&stamp)
        .ok()
        .and_then(|text| serde_yaml_ng::from_str::<Published>(&text).ok());
    if previous.as_ref().is_some_and(|published| {
        published.remote == remote
            && published.tree == tree
            && refs.iter().all(|name| {
                published.retained == *name || published.refs.iter().any(|old| old == name)
            })
    }) {
        return Ok(());
    }
    for reference in &refs {
        repo.push_checkpoint(&remote, reference)?;
    }
    repo.push_checkpoint(&remote, &canonical)?;
    atomic_write_text(
        &stamp,
        &serde_yaml_ng::to_string(&Published {
            remote,
            retained: String::new(),
            refs,
            tree,
            at: super::timefmt::format(&super::timefmt::now()),
        })?,
    )
}

/// Remove moving WIP refs when a task becomes terminal. Completed run refs
/// are retained by default for recovery; `sync.cleanup_runs: true` opts into
/// deleting those as well.
pub fn cleanup(project: &Project, task_id: &str) -> Result<()> {
    let Some(root) = super::sync::store_for_board(&project.data_root) else {
        return Ok(());
    };
    let Some(repo) = vcs::detect(&project.work_path) else {
        return Ok(());
    };
    let settings = super::config::SyncCodeSettings::from_global(
        &ProjectStore::at(root).load_global_config()?,
    )?;
    let canonical = reference(task_id);
    let retained = format!("refs/kanban/wip-runs/{task_id}/");
    let runs = format!("refs/kanban/runs/{task_id}/");
    let mut prefixes = vec![canonical.clone(), retained.clone()];
    if settings.cleanup_runs {
        prefixes.push(runs.clone());
    }
    // `for-each-ref` arguments are prefixes, so the explicit filter prevents
    // cleanup of TASK-10 when TASK-1 becomes terminal.
    let refs: Vec<_> = repo
        .refs_with_prefixes(&prefixes)?
        .into_iter()
        .filter(|name| {
            name == &canonical
                || name.starts_with(&retained)
                || (settings.cleanup_runs && name.starts_with(&runs))
        })
        .collect();
    if let Some(remote) = super::code::publication_remote(root, project)? {
        repo.delete_refs(&remote, &refs)?;
    }
    repo.delete_local_refs(&refs)?;
    let stamp = project
        .data_root
        .join(".kanban/wip")
        .join(format!("{task_id}.published"));
    if let Err(error) = fs::remove_file(stamp)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    Ok(())
}

pub fn cleanup_board(board: &Path, task_id: &str) -> Result<()> {
    let Some(root) = super::sync::store_for_board(board) else {
        return Ok(());
    };
    let Some(id) = board.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let Some(project) = ProjectStore::at(root).get(id)? else {
        return Ok(());
    };
    cleanup(&project, task_id)
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
            let active = task.worktree.is_some()
                && task
                    .session
                    .as_deref()
                    .is_some_and(|id| sessions.is_session_active(id));
            if !active {
                if let Err(error) = publish_pending(&project, &task.id) {
                    failure = Some(error);
                }
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
