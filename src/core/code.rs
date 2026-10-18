//! Machine-local clones and code exchange. Never checks out over human edits.
use super::{
    error::{KanbanError, Result},
    project::{Project, ProjectStore},
    storage::{Storage, atomic_write_text, lock_file_reentrant},
    sync::{Journal, NodeRole, RunDescriptor, RunState, node_role},
    vcs::{self, GitRepo},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn invalid(text: &str) -> KanbanError {
    KanbanError::Invalid(text.into())
}
fn record(project: &Project) -> Result<serde_yaml_ng::Value> {
    Ok(serde_yaml_ng::from_str(&fs::read_to_string(
        project.data_root.join("project.yaml"),
    )?)?)
}
fn save(project: &Project, value: &serde_yaml_ng::Value) -> Result<()> {
    atomic_write_text(
        &project.data_root.join("project.yaml"),
        &serde_yaml_ng::to_string(value)?,
    )
}
fn repository(project: &Project) -> Option<GitRepo> {
    if project.work_path.as_os_str().is_empty() || !project.work_path.is_dir() {
        return None;
    }
    vcs::detect(&project.work_path)
}
fn remote(project: &Project) -> Result<Option<String>> {
    if let Some(repo) = repository(project) {
        if let Some(remote) = repo.code_remote()? {
            return Ok(Some(remote));
        }
    }
    Ok(record(project)?["git_remote"].as_str().map(str::to_owned))
}
fn clone_path(root: &Path, id: &str) -> Result<PathBuf> {
    let config: serde_yaml_ng::Value = match fs::read_to_string(root.join("config.yaml")) {
        Ok(text) => serde_yaml_ng::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_yaml_ng::Value::Null,
        Err(e) => return Err(e.into()),
    };
    let base = config["sync"]["clone_root"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("clones"));
    Ok(if base.is_absolute() {
        base
    } else {
        root.join(base)
    }
    .join(id))
}
fn project(root: &Path, id: &str) -> Result<Project> {
    ProjectStore::at(root)
        .get(id)?
        .ok_or_else(|| invalid("Not found"))
}

pub fn managed_clone(data_root: &Path, work_path: &Path) -> Result<bool> {
    let Some(projects) = data_root
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "projects"))
    else {
        return Ok(false);
    };
    let Some(root) = projects.parent() else {
        return Ok(false);
    };
    let Some(id) = data_root.file_name().and_then(|n| n.to_str()) else {
        return Ok(false);
    };
    Ok(work_path == clone_path(root, id)?)
}
#[derive(Serialize)]
pub struct CloneStatus {
    pub project: String,
    pub state: &'static str,
    pub path: PathBuf,
    pub remote: Option<String>,
    pub integration: Option<String>,
}
pub fn status(root: &Path, id: &str) -> Result<CloneStatus> {
    let project = project(root, id)?;
    let repo = repository(&project);
    Ok(CloneStatus {
        project: id.into(),
        state: if repo.is_some() { "ready" } else { "missing" },
        path: if project.work_path.as_os_str().is_empty() {
            clone_path(root, id)?
        } else {
            project.work_path.clone()
        },
        remote: remote(&project)?,
        integration: repo
            .map(|r| r.integration_ref())
            .transpose()?
            .flatten()
            .map(|oid| oid.to_string()),
    })
}

/// Capture immediately before forwarding a launch. Store metadata and git refs
/// are serialized with clone imports for this project.
pub fn export(
    root: &Path,
    id: &str,
    have: &[String],
) -> Result<(tempfile::NamedTempFile, Option<String>)> {
    let _store = ProjectStore::at(root).lock()?;
    let project = project(root, id)?;
    let _guard = lock_file_reentrant(&project.kanban_dir().join(".lock"))?;
    let repo = repository(&project).ok_or_else(|| invalid("RUNNER_REPO_MISSING"))?;
    let remote = remote(&project)?;
    let mut value = record(&project)?;
    value["git_remote"] = remote
        .clone()
        .map(serde_yaml_ng::Value::String)
        .unwrap_or(serde_yaml_ng::Value::Null);
    save(&project, &value)?;
    let bundle = tempfile::NamedTempFile::new()?;
    repo.code_bundle(
        bundle.path(),
        have,
        !managed_clone(&project.data_root, &project.work_path)?,
    )?;
    Ok((bundle, remote))
}

pub fn publish_live(root: &Path, id: &str, remote: &str) -> Result<()> {
    let _store = ProjectStore::at(root).lock()?;
    let project = project(root, id)?;
    let _guard = lock_file_reentrant(&project.kanban_dir().join(".lock"))?;
    let repo = repository(&project).ok_or_else(|| invalid("RUNNER_REPO_MISSING"))?;
    let mut value = record(&project)?;
    value["git_remote"] = remote.into();
    save(&project, &value)?;
    repo.capture_code_snapshot(!managed_clone(&project.data_root, &project.work_path)?)?;
    repo.publish_code(remote)
}

pub fn prepare_from_remote(root: &Path, id: &str, remote: &str) -> Result<CloneStatus> {
    let _store = ProjectStore::at(root).lock()?;
    {
        let project = project(root, id)?;
        let _guard = lock_file_reentrant(&project.kanban_dir().join(".lock"))?;
        let mut value = record(&project)?;
        value["git_remote"] = remote.into();
        save(&project, &value)?;
    }
    prepare(root, id)?;
    status(root, id)
}

/// First receipt is a lazy clone; subsequent receipts import refs only.
/// A configured remote seeds history, while the bundle supplies unpublished WIP.
pub fn receive(
    root: &Path,
    id: &str,
    bundle: &Path,
    source_remote: Option<&str>,
) -> Result<CloneStatus> {
    let _store = ProjectStore::at(root).lock()?;
    let project = project(root, id)?;
    let _guard = lock_file_reentrant(&project.kanban_dir().join(".lock"))?;
    if node_role(root)? == Some(NodeRole::Mobile)
        && source_remote.is_none()
        && remote(&project)?.is_none()
    {
        return Err(invalid("SYNC_NO_REMOTE"));
    }
    let mut value = record(&project)?;
    if let Some(remote) = source_remote {
        value["git_remote"] = remote.into();
    }
    match repository(&project) {
        Some(repo) => repo.import_code_bundle(bundle)?,
        None => {
            let path = clone_path(root, id)?;
            if path.exists() {
                return Err(invalid("Clone destination already exists"));
            }
            // Clone in a temporary sibling: failed transfers never leave a partial destination.
            fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| invalid("Clone has no parent"))?,
            )?;
            let temp = tempfile::tempdir_in(path.parent().unwrap())?;
            let staging = temp.path().join("repo");
            let source = source_remote.or(value["git_remote"].as_str());
            let repo = GitRepo::clone_code(
                source.unwrap_or(
                    bundle
                        .to_str()
                        .ok_or_else(|| invalid("Invalid bundle path"))?,
                ),
                &staging,
            )?;
            if source.is_none() {
                repo.remove_code_origin()?;
            }
            repo.import_code_bundle(bundle)?;
            fs::rename(&staging, &path)?;
            value["work_path"] = path.to_string_lossy().as_ref().into();
        }
    };
    save(&project, &value)?;
    status(root, id)
}

/// An already prepared clone can launch offline. Network refresh is performed
/// by the incoming transfer/preflight, not repeated after that transfer.
pub fn for_launch(root: &Path, id: &str) -> Result<Project> {
    let project = project(root, id)?;
    if node_role(root)? == Some(NodeRole::Mobile) && remote(&project)?.is_none() {
        return Err(invalid("SYNC_NO_REMOTE"));
    }
    if repository(&project).is_some() {
        return Ok(project);
    }
    prepare(root, id).map_err(|e| {
        if e.to_string().contains("SYNC_NO_REMOTE") || e.to_string().contains("RUNNER_REPO_MISSING")
        {
            e
        } else {
            invalid(&format!("RUNNER_REPO_MISSING: {e}"))
        }
    })
}

/// Remote-only preparation, also usable by the mobile preflight UI.
pub fn prepare(root: &Path, id: &str) -> Result<Project> {
    let _store = ProjectStore::at(root).lock()?;
    let project = project(root, id)?;
    let _guard = lock_file_reentrant(&project.kanban_dir().join(".lock"))?;
    let source = remote(&project)?;
    if node_role(root)? == Some(NodeRole::Mobile) && source.is_none() {
        return Err(invalid("SYNC_NO_REMOTE"));
    }
    if let Some(repo) = repository(&project) {
        // A human working folder must not be silently fetched on every local launch.
        if project.work_path == clone_path(root, id)? {
            if let Some(source) = source {
                repo.fetch_code_remote(&source)?;
            }
        }
        return Ok(project);
    }
    // Local non-git projects retain the ordinary non-isolated launch contract.
    if !project.work_path.as_os_str().is_empty() && project.work_path.is_dir() {
        return Ok(project);
    }
    let source = source.ok_or_else(|| invalid("RUNNER_REPO_MISSING"))?;
    let path = clone_path(root, id)?;
    if path.exists() {
        return Err(invalid("Clone destination already exists"));
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let temp = tempfile::tempdir_in(path.parent().unwrap())?;
    let staging = temp.path().join("repo");
    let repo = GitRepo::clone_code(&source, &staging)?;
    repo.fetch_code_remote(&source)?;
    fs::rename(staging, &path)?;
    let mut value = record(&project)?;
    value["work_path"] = path.to_string_lossy().as_ref().into();
    save(&project, &value)?;
    self::project(root, id)
}

#[derive(Serialize, Deserialize)]
struct SealedRun {
    started_at: String,
    tip: String,
}

pub(crate) fn sealed_tip(board: &Path, task: &str, start: Option<&str>) -> Result<Option<String>> {
    let path = board.join(".kanban/code").join(format!("{task}.json"));
    match fs::read_to_string(path) {
        Ok(text) => {
            let sealed: SealedRun =
                serde_json::from_str(&text).map_err(|e| invalid(&e.to_string()))?;
            Ok((Some(sealed.started_at.as_str()) == start).then_some(sealed.tip))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn run_ref(task: &str, run: &RunDescriptor) -> String {
    format!("refs/kanban/runs/{task}/{}/{}", run.generation, run.node_id)
}

/// Commit and retain the result before on-land cleanup can remove its branch.
pub(crate) fn seal_run(board: &Path, work: &Path, task: &super::models::Task) -> Result<()> {
    let Some(root) = super::sync::store_for_board(board) else {
        return Ok(());
    };
    if !super::sync::enabled(root) {
        return Ok(());
    }
    let (Some(branch), Some(wt), Some(session)) = (&task.branch, &task.worktree, &task.session)
    else {
        return Ok(());
    };
    let session = super::session::SessionManager::new(board).load_session(session);
    let Some(session) = session else {
        return Ok(());
    };
    let started_at = super::timefmt::format(&session.started_at);
    if sealed_tip(board, &task.id, Some(&started_at))?.is_some() {
        return Ok(());
    }
    let repo = vcs::detect(work).ok_or_else(|| invalid("RUNNER_REPO_MISSING"))?;
    repo.commit_all(
        &board.join(".kanban/worktrees").join(wt),
        "kanban: seal completed run",
    )?;
    let tip = repo
        .read_ref(branch)?
        .ok_or_else(|| invalid("completed branch missing"))?;
    let journal = Journal::open(root)?;
    let alias = board
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid board"))?;
    let runs = journal.runs(alias, &task.id)?;
    let run = runs
        .get(&journal.node.node_id)
        .ok_or_else(|| invalid("local run missing"))?;
    repo.set_ref(&run_ref(&task.id, run), &tip)?;
    let directory = board.join(".kanban/code");
    fs::create_dir_all(&directory)?;
    atomic_write_text(
        &directory.join(format!("{}.json", task.id)),
        &serde_json::to_string(&SealedRun {
            started_at,
            tip: tip.as_str().into(),
        })
        .map_err(|e| invalid(&e.to_string()))?,
    )
}

fn current_runs(root: &Path, alias: &str, task: &str) -> Result<Vec<RunDescriptor>> {
    Ok(select_runs(Journal::open(root)?.runs(alias, task)?))
}

fn select_runs(runs: std::collections::BTreeMap<String, RunDescriptor>) -> Vec<RunDescriptor> {
    let generation = runs.values().map(|r| r.generation).max().unwrap_or(0);
    let mut runs: Vec<_> = runs
        .into_values()
        .filter(|r| r.generation == generation)
        .collect();
    runs.sort_by(|a, b| {
        a.start_hlc
            .cmp(&b.start_hlc)
            .then_with(|| a.started_at.cmp(&b.started_at))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    runs
}

pub(crate) fn duplicate_run(board: &Path, task: &str) -> Result<bool> {
    let Some(root) = super::sync::store_for_board(board) else {
        return Ok(false);
    };
    if !super::sync::enabled(root) {
        return Ok(false);
    }
    let alias = board
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid board"))?;
    Ok(current_runs(root, alias, task)?.len() > 1)
}

fn review_branch(task: &str, run: &RunDescriptor, index: usize, runs: &[RunDescriptor]) -> String {
    if index == 0 {
        return format!("kanban/{task}");
    }
    let suffix = if runs.iter().filter(|r| r.name == run.name).count() > 1 {
        format!("{}-{}", run.name, run.node_id)
    } else {
        run.name.clone()
    };
    format!("kanban/{task}@{suffix}")
}

pub(crate) fn validate_review_branch(board: &Path, task: &str, branch: &str) -> Result<String> {
    let root = super::sync::store_for_board(board).ok_or_else(|| invalid("not a synced board"))?;
    let alias = board
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid board"))?;
    let runs = current_runs(root, alias, task)?;
    runs.iter()
        .enumerate()
        .find(|(i, run)| {
            run.state == RunState::Finished
                && run.tip.is_some()
                && review_branch(task, run, *i, &runs) == branch
        })
        .and_then(|(_, run)| run.tip.clone())
        .ok_or_else(|| invalid("branch is not a completed result of this task"))
}

/// Remote exchange is retried by serve even with no HTTP peer reachable.
/// Fetch only per-run refs: never replace this node's integration spine.
pub fn reconcile(root: &Path) -> Result<()> {
    if !super::sync::enabled(root) {
        return Ok(());
    }
    let _store = ProjectStore::at(root).lock()?;
    let journal = Journal::open(root)?;
    let state = journal.state()?;
    let mut failure = None;
    for project in ProjectStore::at(root).list()? {
        let storage = Storage::new(&project.data_root);
        for task in storage.list_unarchived_tasks()? {
            let runs = select_runs(super::sync::task_runs(&state, &project.id, &task.id)?);
            if runs.is_empty() {
                continue;
            }
            let guard = storage.lock()?;
            let Some(mut task) = storage.load_task(&task.id)? else {
                continue;
            };
            let before = (
                task.branch.clone(),
                task.landing_pending.clone(),
                task.run_duplicate,
            );
            task.run_duplicate = runs.len() > 1;
            if !matches!(
                task.status,
                super::models::TaskStatus::Review | super::models::TaskStatus::InProgress
            ) {
                continue;
            }
            if task.run_duplicate && !before.2 {
                let _ = super::thread::ThreadManager::new(&project.data_root)?.post_board_note(
                    &task.id, "RUN_DUPLICATE: оба результата сохранены; выберите ветку в Review через integrate --branch.");
            }
            let finished = runs
                .iter()
                .enumerate()
                .filter(|(_, r)| r.state == RunState::Finished && r.tip.is_some());
            let Some((first_index, first_finished)) = finished.clone().next() else {
                if task.run_duplicate != before.2 {
                    storage.save_task(&task)?;
                }
                continue;
            };
            task.landing_pending = Some(journal.node.name.clone());
            if !task.run_duplicate
                && let Some(repo) = repository(&project)
                && repo.read_ref(&run_ref(&task.id, first_finished))?.is_some()
                && repo.integration_ref()?.is_some()
                && repo.is_landed(first_finished.tip.as_deref().expect("finished tip"))?
            {
                task.landing_pending = None;
            }
            // Pushes and fetches run unlocked: a slow or rejecting remote held
            // the board lock for seconds every cycle, stalling every TUI save
            // and task open behind it. The task is re-read once they finish.
            drop(guard);
            let transfer = (|| -> Result<()> {
                let source = remote(&project)?.ok_or_else(|| invalid("SYNC_NO_REMOTE"))?;
                let repo = match repository(&project) {
                    Some(repo) => repo,
                    None => {
                        prepare(root, &project.id)?;
                        let refreshed = ProjectStore::at(root)
                            .get(&project.id)?
                            .ok_or_else(|| invalid("project missing"))?;
                        repository(&refreshed).ok_or_else(|| invalid("RUNNER_REPO_MISSING"))?
                    }
                };
                for (_, run) in finished.clone() {
                    if run.node_id == journal.node.node_id {
                        repo.push_run(&source, &run_ref(&task.id, run))?;
                    }
                }
                if let Some((index, local)) = finished
                    .clone()
                    .find(|(_, run)| run.node_id == journal.node.node_id)
                {
                    let canonical = format!("kanban/{}", task.id);
                    let branch = review_branch(&task.id, local, index, &runs);
                    if index != 0 && task.branch.as_deref() == Some(&canonical) {
                        let tip = repo
                            .read_ref(&run_ref(&task.id, local))?
                            .ok_or_else(|| invalid("local result missing"))?;
                        repo.rename_run_branch(&canonical, &branch, &tip)?;
                        task.branch = Some(branch);
                    }
                }
                for (index, run) in finished.clone() {
                    let reference = run_ref(&task.id, run);
                    if run.node_id != journal.node.node_id && repo.read_ref(&reference)?.is_none() {
                        repo.fetch_run(&source, &reference)?;
                    }
                    let tip = repo
                        .read_ref(&reference)?
                        .ok_or_else(|| invalid("received result missing"))?;
                    if Some(tip.as_str()) != run.tip.as_deref() {
                        return Err(invalid("received result commit mismatch"));
                    }
                    let branch = review_branch(&task.id, run, index, &runs);
                    if !repo.install_run_branch(&branch, &tip)? {
                        return Err(invalid("Review branch is still owned by a live checkout"));
                    }
                    repo.publish_run_branch(&source, &branch)?;
                }
                if !task.run_duplicate {
                    let branch = review_branch(&task.id, first_finished, first_index, &runs);
                    if repo.integration_ref()?.is_some() && repo.is_landed(&branch)? {
                        task.landing_pending = None;
                    }
                } else if task.integration == super::models::IntegrationState::Landed
                    && selected_tip(&project.data_root, &task.id)?
                        .is_some_and(|tip| runs.iter().any(|r| r.tip.as_ref() == Some(&tip)))
                {
                    task.landing_pending = None;
                }
                Ok(())
            })();
            let renamed = task.branch != before.0;
            let guard = storage.lock()?;
            let Some(mut current) = storage.load_task(&task.id)? else {
                continue;
            };
            let before = (
                current.branch.clone(),
                current.landing_pending.clone(),
                current.run_duplicate,
            );
            // Keep edits made while unlocked; take only the fields owned here.
            if renamed {
                current.branch = task.branch.clone();
            }
            current.landing_pending = task.landing_pending.take();
            current.run_duplicate = task.run_duplicate;
            let mut task = current;
            let local_pending = task.landing_pending.is_some();
            let selected = if !local_pending {
                selected_tip(&project.data_root, &task.id)?
            } else {
                None
            };
            let tip = selected
                .as_deref()
                .unwrap_or_else(|| first_finished.tip.as_deref().expect("finished tip"));
            journal.record_landing(
                &project.data_root,
                &task.id,
                runs[0].generation,
                tip,
                local_pending,
            )?;
            task.landing_pending = pending_landings(&project.data_root, &task.id)?;
            if task.branch != before.0
                || task.landing_pending != before.1
                || task.run_duplicate != before.2
            {
                storage.save_task(&task)?;
            }
            drop(guard);
            if let Err(error) = transfer {
                failure = Some(error);
                continue;
            }
            if local_pending
                && !task.run_duplicate
                && task.status == super::models::TaskStatus::Review
                && task.worktree.is_none()
            {
                let refreshed = ProjectStore::at(root)
                    .get(&project.id)?
                    .ok_or_else(|| invalid("project missing"))?;
                let ops = super::operations::Operations::for_project(&refreshed);
                if ops.config.get_orchestration()?.isolation.land
                    != super::config::IsolationLand::Manual
                {
                    ops.integrate_received(
                        &task.id,
                        &review_branch(&task.id, first_finished, first_index, &runs),
                    )?;
                }
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn selection_path(board: &Path, task: &str) -> PathBuf {
    board.join(".kanban/code").join(format!("{task}.selected"))
}
fn selected_tip(board: &Path, task: &str) -> Result<Option<String>> {
    match fs::read_to_string(selection_path(board, task)) {
        Ok(tip) => Ok(Some(tip)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub(crate) fn record_selection(board: &Path, task: &str, tip: &str) -> Result<()> {
    fs::create_dir_all(board.join(".kanban/code"))?;
    atomic_write_text(&selection_path(board, task), tip)?;
    let root = super::sync::store_for_board(board).ok_or_else(|| invalid("not a synced board"))?;
    let alias = board
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid board"))?;
    let runs = current_runs(root, alias, task)?;
    let generation = runs
        .first()
        .ok_or_else(|| invalid("run missing"))?
        .generation;
    Journal::open(root)?.record_landing(board, task, generation, tip, false)
}

pub(crate) fn pending_landings(board: &Path, task: &str) -> Result<Option<String>> {
    let root = super::sync::store_for_board(board).ok_or_else(|| invalid("not a synced board"))?;
    let alias = board
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid board"))?;
    let state = Journal::open(root)?.state()?;
    let runs = select_runs(super::sync::task_runs(&state, alias, task)?);
    let Some(run) = runs.first() else {
        return Ok(None);
    };
    let names: Vec<_> = super::sync::task_landings(&state, alias, task)?
        .into_values()
        .filter(|ack| ack.pending && ack.generation == run.generation)
        .map(|ack| ack.name)
        .collect();
    Ok((!names.is_empty()).then(|| names.join(", ")))
}
