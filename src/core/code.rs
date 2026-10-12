//! Machine-local clones and code exchange. Never checks out over human edits.
use super::{
    error::{KanbanError, Result},
    project::{Project, ProjectStore},
    storage::{atomic_write_text, lock_file_reentrant},
    sync::{Journal, NodeRole},
    vcs::{self, GitRepo},
};
use serde::Serialize;
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
    if Journal::open(root)?.node.role == NodeRole::Mobile
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
    if Journal::open(root)?.node.role == NodeRole::Mobile && remote(&project)?.is_none() {
        return Err(invalid("SYNC_NO_REMOTE"));
    }
    if repository(&project).is_some() {
        return Ok(project);
    }
    prepare(root, id)
}

/// Remote-only preparation, also usable by the mobile preflight UI.
pub fn prepare(root: &Path, id: &str) -> Result<Project> {
    let _store = ProjectStore::at(root).lock()?;
    let project = project(root, id)?;
    let _guard = lock_file_reentrant(&project.kanban_dir().join(".lock"))?;
    let source = remote(&project)?;
    if Journal::open(root)?.node.role == NodeRole::Mobile && source.is_none() {
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
