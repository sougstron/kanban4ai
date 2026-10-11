//! Transport-facing journal reads and bootstrap snapshots. The HTTP layer
//! lives in `cli/serve/sync.rs`; everything that touches the store is here.
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::*;
use crate::core::project::ProjectStore;
use crate::core::storage::{BoardGuard, Storage};

/// A full replica without machine-local data, for an empty peer (section 10.4).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub node_id: String,
    pub store_id: String,
    pub vector: VersionVector,
    pub hlc: Hlc,
    /// Store-relative path -> base64 content.
    pub files: BTreeMap<String, String>,
}

/// One page of operations a peer lacks. `next` is the paging cursor: per
/// node, the highest sequence included so far.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Page {
    pub operations: Vec<Operation>,
    pub next: VersionVector,
    pub more: bool,
}

const BOARD_TREES: [&str; 3] = ["instructions", "assets", "context"];
const STATUSES: [&str; 5] = ["todo", "in_progress", "review", "done", "archive"];

impl Journal {
    pub fn vector(&self) -> Result<VersionVector> {
        Ok(self.state()?.vector)
    }

    /// Project alias -> replicated UID.
    pub fn project_uids(&self) -> Result<BTreeMap<String, String>> {
        Ok(self.state()?.project_uids)
    }

    /// Operations newer than `after`, oldest HLC first. A node's HLC grows
    /// with its sequence, so each node's slice of a page is a sequence prefix.
    /// Leases and tombstones lead the page (section 10.2).
    pub fn operations_after(&self, after: &VersionVector, limit: usize) -> Result<Page> {
        let state = self.state()?;
        let mut ops = Vec::new();
        for op in state.operations.values() {
            if sequence(op)? > after.get(&op.hlc.2).copied().unwrap_or(0) {
                ops.push(op);
            }
        }
        ops.sort_by(|a, b| a.hlc.cmp(&b.hlc));
        let more = ops.len() > limit.max(1);
        ops.truncate(limit.max(1));
        let mut next = after.clone();
        for op in &ops {
            let seq = sequence(op)?;
            let entry = next.entry(op.hlc.2.clone()).or_default();
            *entry = (*entry).max(seq);
        }
        // Stable: HLC order is kept inside both groups.
        ops.sort_by_key(|op| !matches!(op.entity.as_str(), "lease" | "tombstone"));
        Ok(Page {
            operations: ops.into_iter().cloned().collect(),
            next,
            more,
        })
    }

    /// Whether this node issues the next ID grants for project `uid`: the
    /// issuer of existing grants, otherwise the paired laptop (section 6).
    pub fn issues_id_leases(&self, uid: &str) -> Result<bool> {
        let state = self.state()?;
        match lease::grants(&state, uid)?.first() {
            Some((op, _)) => Ok(op.hlc.2 == self.node.node_id),
            None => lease::fallback_issuer(self.root.parent().expect("sync parent"), &self.node),
        }
    }

    /// Replicated store copy taken under the same locks as `apply_remote`.
    pub fn snapshot(&self) -> Result<Snapshot> {
        apply::recover_remote(&self.root)?;
        let store = self.root.parent().expect("sync parent");
        let _store = lock_file_reentrant(&store.join(".lock"))?;
        let mut boards = Vec::new();
        let projects = store.join("projects");
        if projects.exists() {
            for entry in fs::read_dir(&projects)? {
                let path = entry?.path();
                if path.join("project.yaml").is_file() {
                    boards.push(path);
                }
            }
        }
        boards.sort();
        let _boards: Vec<BoardGuard> = boards
            .iter()
            .map(|board| Storage::new(board).lock())
            .collect::<Result<_>>()?;
        let _sync = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let state_path = self.root.join("meta/state.json");
        let mut state: JournalState = load(&state_path)?;
        // Untouched legacy boards have no UID yet; both replicas must share it.
        let mut assigned = false;
        for board in &boards {
            let alias = board
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| invalid("invalid project directory"))?;
            if !state.project_uids.contains_key(alias) {
                state.project_uids.insert(alias.into(), uuid()?);
                assigned = true;
            }
        }
        if assigned {
            fs::create_dir_all(self.root.join("meta"))?;
            atomic_write_text_raw(&state_path, &json(&state)?)?;
        }
        let mut files = BTreeMap::new();
        let mut add = |path: &Path, bytes: Vec<u8>| -> Result<()> {
            let relative = path
                .strip_prefix(store)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| invalid("invalid snapshot path"))?;
            files.insert(relative.to_owned(), STANDARD.encode(bytes));
            Ok(())
        };
        for board in &boards {
            let project = board.join("project.yaml");
            let text = fs::read_to_string(&project)?;
            add(&project, replicated(&text, "project", false)?.into_bytes())?;
            let kanban = board.join(".kanban");
            let config = kanban.join("config.yaml");
            if config.is_file() {
                add(&config, fs::read(&config)?)?;
            }
            for status in read_dirs(&kanban.join("tasks"))? {
                for task in read_files(&status)? {
                    if task.extension().is_some_and(|e| e == "md") {
                        let text = fs::read_to_string(&task)?;
                        add(&task, replicated(&text, "task", false)?.into_bytes())?;
                    }
                }
            }
            for thread in read_files(&kanban.join("threads"))? {
                if thread.extension().is_some_and(|e| e == "yaml") {
                    add(&thread, fs::read(&thread)?)?;
                }
            }
            for tree in BOARD_TREES {
                for file in walk(&kanban.join(tree))? {
                    add(&file, fs::read(&file)?)?;
                }
            }
        }
        let config = store.join("config.yaml");
        if config.is_file() {
            let text = fs::read_to_string(&config)?;
            add(&config, replicated(&text, "config", true)?.into_bytes())?;
        }
        for file in walk(&self.root.join("meta"))? {
            if file.file_name().is_some_and(|n| n != "allocation.json") {
                add(&file, fs::read(&file)?)?;
            }
        }
        for file in read_files(&self.root.join("journal"))? {
            if file.extension().is_some_and(|e| e == "jsonl") {
                add(&file, fs::read(&file)?)?;
            }
        }
        let clock: Hlc = load(&self.root.join("hlc"))?;
        Ok(Snapshot {
            node_id: self.node.node_id.clone(),
            store_id: self.node.store_id.clone(),
            vector: state.vector,
            hlc: clock,
            files,
        })
    }
}

/// Adopt a peer's replica. Only an empty store (no registered projects) may
/// be replaced; the redo record makes an interrupted install finish on the
/// next `Journal::open`.
pub fn install_snapshot(store: &Path, snapshot: &Snapshot) -> Result<()> {
    if snapshot.store_id.is_empty() || snapshot.store_id.contains(['/', '\\', '\0']) {
        return Err(invalid("invalid snapshot store"));
    }
    for (path, data) in &snapshot.files {
        allowed_path(path)?;
        STANDARD
            .decode(data)
            .map_err(|_| invalid("invalid snapshot file encoding"))?;
    }
    let root = store.join("sync");
    fs::create_dir_all(&root)?;
    {
        let _store = lock_file_reentrant(&store.join(".lock"))?;
        if !ProjectStore::at(store).list()?.is_empty() {
            return Err(invalid(
                "SYNC_STORE_MISMATCH: bootstrap requires an empty store",
            ));
        }
        atomic_write_text_raw(&root.join("bootstrap.json"), &json(snapshot)?)?;
    }
    recover_bootstrap(store)
}

/// Finish an interrupted bootstrap. Replaces the local journal, metadata and
/// replicated files, keeps machine-local config sections, then adopts the
/// store UUID last.
pub(super) fn recover_bootstrap(store: &Path) -> Result<()> {
    let root = store.join("sync");
    let path = root.join("bootstrap.json");
    if !path.exists() {
        return Ok(());
    }
    let _store = lock_file_reentrant(&store.join(".lock"))?;
    if !path.exists() {
        return Ok(());
    }
    let snapshot: Snapshot = load(&path)?;
    let mut boards = BTreeSet::new();
    for relative in snapshot.files.keys() {
        allowed_path(relative)?;
        if let Some(rest) = relative.strip_prefix("projects/") {
            let alias = rest.split('/').next().expect("split yields one item");
            boards.insert(store.join("projects").join(alias));
        }
    }
    let _boards: Vec<BoardGuard> = boards
        .iter()
        .map(|board| Storage::new(board).lock())
        .collect::<Result<_>>()?;
    let _sync = lock_file_reentrant(&root.join(".lock"))?;
    for stale in ["journal", "meta", "snapshot"] {
        match fs::remove_dir_all(root.join(stale)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    for name in ["pending.json", "remote-pending.json"] {
        match fs::remove_file(root.join(name)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    for (relative, data) in &snapshot.files {
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| invalid("invalid snapshot file encoding"))?;
        let target = store.join(relative);
        if relative == "config.yaml" {
            let text = String::from_utf8(bytes).map_err(|_| invalid("invalid config"))?;
            atomic_write_text_raw(&target, &merge_global(&target, &text)?)?;
            continue;
        }
        fs::create_dir_all(target.parent().expect("file parent"))?;
        write_bytes(&target, &bytes)?;
    }
    for board in &boards {
        for status in STATUSES {
            fs::create_dir_all(board.join(".kanban/tasks").join(status))?;
        }
        fs::create_dir_all(board.join(".kanban/threads"))?;
    }
    let mut clock: Hlc = load(&root.join("hlc"))?;
    if snapshot.hlc > clock {
        clock = snapshot.hlc.clone();
    }
    atomic_write_text_raw(&root.join("hlc"), &json(&clock)?)?;
    let node_path = root.join("node.yaml");
    let mut node: NodeIdentity = serde_yaml_ng::from_str(&fs::read_to_string(&node_path)?)?;
    node.store_id = snapshot.store_id.clone();
    atomic_write_text_raw(&node_path, &serde_yaml_ng::to_string(&node)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&node_path, fs::Permissions::from_mode(0o600))?;
    }
    fs::remove_file(&path)?;
    fs::File::open(&root)?.sync_all()?;
    Ok(())
}

/// Snapshot paths are store-relative and limited to replicated locations.
fn allowed_path(path: &str) -> Result<()> {
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts {
        component(part)?;
    }
    let ok = match parts.as_slice() {
        ["config.yaml"] => true,
        ["sync", "journal", _] => true,
        ["sync", "meta", rest @ ..] => !rest.is_empty() && rest != ["allocation.json"],
        ["projects", _, "project.yaml"] => true,
        ["projects", _, ".kanban", "config.yaml"] => true,
        ["projects", _, ".kanban", "tasks", status, _] => STATUSES.contains(status),
        ["projects", _, ".kanban", "threads", _] => true,
        ["projects", _, ".kanban", tree, rest @ ..] => {
            BOARD_TREES.contains(tree) && !rest.is_empty()
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(invalid("snapshot contains a machine-local path"))
    }
}

fn component(part: &str) -> Result<()> {
    if part.is_empty() || part == "." || part == ".." || part.contains(['\\', '\0']) {
        return Err(invalid("invalid snapshot path"));
    }
    Ok(())
}

/// Strip machine-local fields while preserving key order and file format.
fn replicated(text: &str, entity: &str, global: bool) -> Result<String> {
    let mut value = parse(text, entity)?;
    let map = value
        .as_object_mut()
        .ok_or_else(|| invalid("snapshot file is not a mapping"))?;
    for field in local_fields(entity, global) {
        map.remove(*field);
    }
    match entity {
        "task" => {
            let description = map
                .remove("description")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            Ok(format!(
                "---\n{}---\n{description}\n",
                serde_yaml_ng::to_string(&value)?
            ))
        }
        "project" => {
            // The receiving node has its own working folder (section 3).
            map.insert("work_path".into(), Value::String(String::new()));
            Ok(serde_yaml_ng::to_string(&value)?)
        }
        _ => Ok(serde_yaml_ng::to_string(&value)?),
    }
}

/// Replicated global keys come from the snapshot; local sections stay.
fn merge_global(local_path: &Path, remote: &str) -> Result<String> {
    let mut merged: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(remote)?;
    let local: serde_yaml_ng::Mapping = match fs::read_to_string(local_path) {
        Ok(text) => serde_yaml_ng::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e.into()),
    };
    for field in local_fields("config", true) {
        merged.remove(*field);
        if let Some(value) = local.get(*field) {
            merged.insert((*field).into(), value.clone());
        }
    }
    Ok(serde_yaml_ng::to_string(&merged)?)
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("missing parent"))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp_")
        .tempfile_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| KanbanError::Io(e.error))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                // Skip in-flight temp files and anything not plain UTF-8.
                if name.to_str().is_some_and(|n| !n.starts_with(".tmp_")) {
                    result.push(entry.path());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    result.sort();
    Ok(result)
}

fn read_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in entries(dir)? {
        if fs::symlink_metadata(&path)?.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

fn read_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for path in entries(dir)? {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if fs::symlink_metadata(&path)?.is_dir() && STATUSES.contains(&name) {
            dirs.push(path);
        }
    }
    Ok(dirs)
}

fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in entries(dir)? {
        let kind = fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            files.extend(walk(&path)?);
        } else if kind.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}
