//! Remote redo transactions and deterministic projection of replicated fields.
use super::*;
use crate::core::models::{Message, Thread};
use crate::core::storage::{BoardGuard, Storage};
use crate::core::thread::merge_threads;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conflict {
    pub project: String,
    pub entity: String,
    pub id: String,
    pub field: String,
    pub winner: Hlc,
    pub loser: Hlc,
    pub winner_value: Value,
    pub loser_value: Value,
}

#[derive(Default, Serialize, Deserialize)]
struct RemotePending {
    boards: Vec<PathBuf>,
    writes: BTreeMap<PathBuf, Option<String>>,
    #[serde(default)]
    blobs: BTreeMap<PathBuf, String>,
    #[serde(default)]
    operations: Vec<Operation>,
    #[serde(default)]
    uids: Option<projection::Uids>,
    #[serde(default)]
    conflicts: String,
    #[serde(default)]
    hlc: Option<Hlc>,
}

fn finish(root: &Path, pending: &RemotePending) -> Result<()> {
    let journal = Journal {
        root: root.to_path_buf(),
        node: serde_yaml_ng::from_str(&fs::read_to_string(root.join("node.yaml"))?)?,
    };
    if !pending.operations.is_empty() {
        let limits = format::JournalLimits::load(root.parent().unwrap_or(root))?;
        let mut by_node: BTreeMap<String, Vec<Operation>> = BTreeMap::new();
        for op in &pending.operations {
            by_node
                .entry(op.hlc.2.clone())
                .or_default()
                .push(op.clone());
        }
        for (node, ops) in &by_node {
            let head = segments::head_seq(&root.join("journal").join(node)).unwrap_or(0);
            let ahead = segments::ahead_seqs(root, node).unwrap_or_default();
            let fresh: Vec<_> = ops
                .iter()
                .filter(|op| {
                    sequence(op).ok().is_some_and(|seq| {
                        seq > head && seq > journal_compacted(root, node) && !ahead.contains(&seq)
                    })
                })
                .cloned()
                .collect();
            if !fresh.is_empty() {
                segments::append(root, node, &fresh, &limits)?;
            }
        }
    }
    if let Some(uids) = &pending.uids {
        fs::create_dir_all(root.join("meta"))?;
        projection::atomic_write_json(&root.join("meta/uids.json"), uids)?;
    }
    if !pending.conflicts.is_empty() {
        atomic_write_text_raw(&root.join("meta/conflicts.jsonl"), &pending.conflicts)?;
    }
    if let Some(clock) = &pending.hlc {
        atomic_write_text_raw(&root.join("hlc"), &json(clock)?)?;
    }
    for (path, hash) in &pending.blobs {
        transport::write_bytes(path, &journal.blob(hash)?)?;
        drift::record(&journal, path)?;
    }
    for (path, content) in &pending.writes {
        if let Some(text) = content {
            fs::create_dir_all(path.parent().expect("file parent"))?;
            atomic_write_text_raw(path, text)?;
        } else {
            match fs::remove_file(path) {
                Ok(()) => fs::File::open(path.parent().expect("file parent"))?.sync_all()?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        drift::record(&journal, path)?;
    }
    Ok(())
}

pub(super) fn recover_remote(root: &Path) -> Result<()> {
    let path = root.join("remote-pending.json");
    if !path.exists() {
        return Ok(());
    }
    let store = root.parent().expect("sync parent");
    let _store = lock_file_reentrant(&store.join(".lock"))?;
    // Re-read under the store lock: another recovery may already have finished.
    if !path.exists() {
        return Ok(());
    }
    let pending: RemotePending = load(&path)?;
    let _boards: Vec<BoardGuard> = pending
        .boards
        .iter()
        .map(|board| Storage::new(board).lock())
        .collect::<Result<_>>()?;
    let _sync = lock_file_reentrant(&root.join(".lock"))?;
    finish(root, &pending)?;
    fs::remove_file(path)?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

impl Journal {
    /// Apply trusted operations durably, then execute runner-owned effects outside locks.
    /// max_skew_ms comes from the caller's sync configuration.
    pub fn apply_remote(
        &self,
        operations: &[Operation],
        max_skew_ms: u64,
    ) -> Result<Vec<Conflict>> {
        recover_remote(&self.root)?;
        self.reconcile()?;
        let conflicts = self.apply_remote_transaction(operations, max_skew_ms)?;
        super::control::process_store(self.root.parent().expect("sync parent"))?;
        Ok(conflicts)
    }

    /// Restore the losing field as a normal human edit with a fresh local HLC.
    /// Reject stale controls rather than overwriting edits made since rendering.
    pub fn take_other_version(&self, conflict: &Conflict) -> Result<()> {
        let store = self.root.parent().expect("sync parent");
        let _store = lock_file_reentrant(&store.join(".lock"))?;
        let mut boards =
            BTreeSet::from([store.join("projects").join(component(&conflict.project)?)]);
        for entry in fs::read_dir(store.join("projects"))? {
            let path = entry?.path();
            if path.is_dir() {
                boards.insert(path);
            }
        }
        let _boards = boards
            .iter()
            .map(|path| Storage::new(path).lock())
            .collect::<Result<Vec<_>>>()?;
        let _sync = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let state = self.state()?;
        let field = state
            .fields
            .get(&key(&conflict.project, &conflict.entity, &conflict.id))
            .ok_or_else(|| invalid("conflict entity no longer exists"))?;
        if field.clocks.get(&conflict.field) != Some(&conflict.winner) {
            return Err(invalid(
                "conflict changed; refresh before choosing a version",
            ));
        }
        if !self.conflicts()?.contains(conflict) {
            return Err(invalid("conflict is no longer active"));
        }
        let seq = state
            .vector
            .get(&self.node.node_id)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid("operation sequence exhausted"))?;
        let clock: Hlc = self.clock()?;
        let operation = Operation {
            v: OP_FORMAT,
            extra: BTreeMap::new(),
            op: format!("{}:{seq}", self.node.node_id),
            hlc: clock.tick(now_ms(), &self.node.node_id)?,
            store: self.node.store_id.clone(),
            project: conflict.project.clone(),
            entity: conflict.entity.clone(),
            id: conflict.id.clone(),
            set: BTreeMap::from([(conflict.field.clone(), conflict.loser_value.clone())]),
            prev: BTreeMap::from([(conflict.field.clone(), conflict.winner.clone())]),
            actor: "human".into(),
        };
        self.apply_remote_transaction(&[operation], u64::MAX)?;
        Ok(())
    }

    fn apply_remote_transaction(
        &self,
        operations: &[Operation],
        max_skew_ms: u64,
    ) -> Result<Vec<Conflict>> {
        let store = self.root.parent().expect("sync parent");
        let _store = lock_file_reentrant(&store.join(".lock"))?;
        // Local writers lock board before sync. Lock all existing and prospective
        // boards in stable order before taking sync, never in the reverse order.
        let mut boards = BTreeSet::new();
        let projects = store.join("projects");
        if projects.exists() {
            for entry in fs::read_dir(&projects)? {
                let path = entry?.path();
                if path.is_dir() {
                    boards.insert(path);
                }
            }
        }
        for op in operations {
            validate(op, &self.node.store_id)?;
            if !op.project.is_empty() {
                boards.insert(projects.join(component(&op.project)?));
            }
            if op.entity == "project" {
                if let Some(id) = op.set.get("id").and_then(Value::as_str) {
                    boards.insert(projects.join(component(id)?));
                }
            }
        }
        let mut guards: Vec<BoardGuard> = boards
            .iter()
            .map(|p| Storage::new(p).lock())
            .collect::<Result<_>>()?;
        let sync = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let (mut state, prune_meta) = projection::load_state(&self.root)?;
        let previous_aliases = state.project_uids.clone();
        let mut clock: Hlc = self.clock()?;
        for op in operations {
            clock.observe(&op.hlc, now_ms(), max_skew_ms)?;
        }
        let mut accepted = Vec::new();
        for op in operations {
            let seq = sequence(op)?;
            let node = &op.hlc.2;
            let head = state.vector.get(node).copied().unwrap_or(0);
            if seq <= head || seq <= state.compacted.get(node).copied().unwrap_or(0) {
                continue;
            }
            if segments::ahead_seqs(&self.root, node)?.contains(&seq) {
                continue;
            }
            apply_meta(&mut state, op);
            accepted.push(op.clone());
        }
        if accepted.is_empty() {
            return prune::conflicts_of(&state, &self.retained_ops(&state)?);
        }
        let mut history = self.retained_ops(&state)?;
        history.extend(accepted.iter().cloned());
        let conflicts = prune::conflicts_of(&state, &history)?;
        let mut writes = BTreeMap::new();
        project_aliases(&mut state)?;
        // Existing boards remain locked while calculating collision destinations.
        // Release sync before locking newly allocated boards to preserve lock order.
        drop(sync);
        for alias in state.project_uids.keys() {
            let path = projects.join(component(alias)?);
            if boards.insert(path.clone()) {
                guards.push(Storage::new(&path).lock()?);
            }
        }
        let _sync = lock_file_reentrant(&self.root.join(".lock"))?;
        project_files(
            store,
            &mut state,
            &previous_aliases,
            &conflicts,
            &mut writes,
        )?;
        let mut blobs = BTreeMap::new();
        for (key, fields) in &state.fields {
            let (project, entity, id): (String, String, String) =
                serde_json::from_str(key).map_err(|e| invalid(&e.to_string()))?;
            if entity != "file" {
                continue;
            }
            drift::validate_file(&id, &fields.values)?;
            let alias = state
                .project_uids
                .iter()
                .find(|(_, uid)| **uid == project)
                .map(|(alias, _)| alias)
                .unwrap_or(&project);
            let path = store.join("projects").join(component(alias)?).join(&id);
            transport::no_symlinks(&path)?;
            if deleted(&state, &project, "file", &id) {
                writes.insert(path, None);
            } else if let Some(hash) = fields.values.get("blob").and_then(Value::as_str) {
                self.blob(hash)?;
                blobs.insert(path, hash.to_owned());
            }
        }
        writes.retain(|path, content| {
            let text = path.to_string_lossy();
            if text.contains("/sync/meta/state.json") || text.contains("/sync/journal/") {
                return false;
            }
            match content {
                Some(new) => !matches!(fs::read_to_string(path), Ok(old) if old == *new),
                None => path.exists(),
            }
        });
        let mut text = String::new();
        for conflict in &conflicts {
            text.push_str(&json(conflict)?);
            text.push('\n');
        }
        let pending = RemotePending {
            boards: boards.into_iter().collect(),
            writes,
            blobs,
            operations: accepted,
            uids: Some(projection::uids_of(&state)),
            conflicts: text,
            hlc: Some(clock),
        };
        let path = self.root.join("remote-pending.json");
        atomic_write_text_raw(&path, &json(&pending)?)?;
        finish(&self.root, &pending)?;
        fs::remove_file(path)?;
        state.vector = segments::heads(&self.root, &state.compacted)?;
        projection::note_ops(&self.root, pending.operations.len() as u64)?;
        if projection::checkpoint(&self.root, &state, &prune_meta, false)?
            && format::solo_store(store)
        {
            let _ = self.prune(&BTreeMap::new());
        }
        fs::File::open(&self.root)?.sync_all()?;
        Ok(conflicts)
    }
}

fn component(value: &str) -> Result<&str> {
    if value.is_empty() || value == "." || value == ".." || value.contains(['/', '\\', '\0']) {
        return Err(invalid("invalid sync path component"));
    }
    Ok(value)
}

fn journal_compacted(root: &Path, node: &str) -> u64 {
    projection::load_state(root)
        .ok()
        .and_then(|(state, _)| state.compacted.get(node).copied())
        .unwrap_or(0)
}

pub(super) fn validate(op: &Operation, store: &str) -> Result<()> {
    if op.v > OP_FORMAT {
        return Err(invalid("SYNC_OP_FORMAT_TOO_NEW"));
    }
    if op.store != store {
        return Err(invalid("SYNC_STORE_MISMATCH"));
    }
    component(&op.hlc.2)?;
    if op.entity == "file"
        || (matches!(op.entity.as_str(), "tombstone" | "restore")
            && op.set.get("entity").and_then(Value::as_str) == Some("file"))
    {
        drift::validate_file(&op.id, &op.set)?;
    } else {
        component(&op.id)?;
    }
    if sequence(op)? == 0 || op.op.rsplit_once(':').map(|p| p.0) != Some(&op.hlc.2) {
        return Err(invalid("operation id does not match HLC node"));
    }
    if !matches!(
        op.entity.as_str(),
        // `lease` is inert: retired task-ID grants still present in old journals.
        "task"
            | "message"
            | "thread"
            | "project"
            | "config"
            | "file"
            | "tombstone"
            | "restore"
            | "lease"
            | "run"
    ) {
        return Err(invalid("unsupported sync entity"));
    }
    if op.entity == "thread"
        && (op.set.len() != 1 || !op.set.get("deleted").is_some_and(Value::is_boolean))
    {
        return Err(invalid("invalid thread lifecycle operation"));
    }
    if op.entity == "run" {
        run::validate(op)?;
    }
    if (op.entity == "tombstone" || op.entity == "restore")
        && !matches!(
            op.set.get("entity").and_then(Value::as_str),
            Some("task" | "message" | "project" | "config" | "file")
        )
    {
        return Err(invalid("invalid tombstone/restore entity"));
    }
    if op.entity == "task" {
        if let Some(status) = op.set.get("status") {
            let status = status
                .as_str()
                .ok_or_else(|| invalid("invalid task status"))?;
            if !matches!(
                status,
                "todo" | "in_progress" | "review" | "done" | "archive"
            ) {
                return Err(invalid("invalid task status"));
            }
        }
    }
    let global = op.project.is_empty();
    for field in op.set.keys() {
        if op.entity == "config" && global && field == "sync.relay_encrypt" {
            if !op.set[field].is_boolean() && !op.set[field].is_null() {
                return Err(invalid("invalid sync.relay_encrypt"));
            }
            continue;
        }
        let top = field.split('.').next().unwrap_or(field);
        let probe = serde_json::json!({top: true});
        if !fields(&probe, &op.entity, global).contains_key(top) {
            return Err(invalid("operation contains machine-local field"));
        }
    }
    Ok(())
}

pub(super) fn deleted(state: &JournalState, project: &str, entity: &str, id: &str) -> bool {
    let Some(life) = state.lifecycle.get(&key(project, entity, id)) else {
        return false;
    };
    life.deleted
        .as_ref()
        .is_some_and(|d| life.restored.as_ref().is_none_or(|r| d > r))
}

fn project_aliases(state: &mut JournalState) -> Result<()> {
    let mut projects = Vec::new();
    for (k, fields) in &state.fields {
        let (project, entity, _): (String, String, String) =
            serde_json::from_str(k).map_err(|e| invalid(&e.to_string()))?;
        if entity != "project" {
            continue;
        }
        let id = fields
            .values
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&project)
            .to_owned();
        component(&id)?;
        let created = state
            .lifecycle
            .iter()
            .filter_map(|(k, life)| {
                let (uid, entity, _): (String, String, String) = serde_json::from_str(k).ok()?;
                (entity == "project" && uid == project).then_some(life.created.clone())
            })
            .flatten()
            .min()
            .unwrap_or_default();
        projects.push((created, project, id));
    }
    projects.sort();
    let mut used: BTreeSet<String> = projects.iter().map(|(_, _, id)| id.clone()).collect();
    let mut owners = BTreeSet::new();
    for (_, uid, id) in projects {
        let mut alias = id.clone();
        let mut suffix = 2;
        if !owners.insert(id.clone()) {
            loop {
                alias = format!("{id}-{suffix}");
                suffix += 1;
                if !used.contains(&alias) {
                    break;
                }
            }
        }
        used.insert(alias.clone());
        state.project_uids.retain(|_, value| value != &uid);
        state.project_uids.insert(alias, uid);
    }
    Ok(())
}

fn object_at(path: &Path, entity: &str) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text, entity),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e.into()),
    }
}
fn set_path(value: &mut Value, path: &str, new: &Value) {
    let (head, tail) = path
        .split_once('.')
        .map_or((path, None), |(h, t)| (h, Some(t)));
    if !value.is_object() {
        *value = serde_json::json!({});
    }
    let map = value.as_object_mut().expect("object");
    if let Some(tail) = tail {
        set_path(map.entry(head).or_insert(Value::Null), tail, new);
    } else if new.is_null() {
        map.remove(head);
    } else {
        map.insert(head.into(), new.clone());
    }
}

type ThreadProjection = BTreeMap<(String, String), Vec<(Hlc, String, Value)>>;

fn project_files(
    store: &Path,
    state: &mut JournalState,
    previous_aliases: &BTreeMap<String, String>,
    conflicts: &[Conflict],
    writes: &mut BTreeMap<PathBuf, Option<String>>,
) -> Result<()> {
    let renames = ids::reassign(state);
    let mut threads = ThreadProjection::new();
    for (k, fields) in &state.fields {
        let (project, entity, id): (String, String, String) =
            serde_json::from_str(k).map_err(|e| invalid(&e.to_string()))?;
        let alias = state
            .project_uids
            .iter()
            .find(|(_, uid)| **uid == project)
            .map(|(id, _)| id.as_str())
            .unwrap_or(&project);
        let root = store.join("projects").join(alias);
        let old_alias = previous_aliases
            .iter()
            .find(|(_, uid)| **uid == project)
            .map(|(id, _)| id.as_str())
            .unwrap_or(&project);
        let old_root = store.join("projects").join(old_alias);
        if entity == "thread" {
            if fields.values.get("deleted") == Some(&Value::Bool(true)) {
                let display = renames.display(&project, &id);
                writes.insert(
                    root.join(".kanban/threads")
                        .join(format!("{}.yaml", component(&display)?)),
                    None,
                );
            } else {
                threads.entry((project, id)).or_default();
            }
            continue;
        }
        if entity == "message" {
            if deleted(state, &project, "message", &id) {
                continue;
            }
            let Some(task) = fields.values.get("task_id").and_then(Value::as_str) else {
                continue;
            };
            if state
                .fields
                .get(&key(&project, "thread", task))
                .and_then(|f| f.values.get("deleted"))
                == Some(&Value::Bool(true))
            {
                continue;
            }
            component(task)?;
            let created = state
                .lifecycle
                .get(&key(&project, "message", &id))
                .and_then(|life| life.created.clone())
                .unwrap_or_default();
            threads.entry((project, task.into())).or_default().push((
                created,
                id,
                serde_json::to_value(&fields.values).map_err(|e| invalid(&e.to_string()))?,
            ));
            continue;
        }
        let path = match entity.as_str() {
            "run" if fields.values.contains_key("command") => continue,
            "run" => {
                let (path, text) = run::projection(&store.join("sync"), &project, &id, fields)?;
                writes.insert(path, Some(text));
                continue;
            }
            "task" => {
                let tasks = old_root.join(".kanban/tasks");
                let mut existing = None;
                let display = renames.display(&project, &id);
                if let Some(old) = renames.previous(&project, &id)
                    && tasks.exists()
                {
                    for entry in fs::read_dir(&tasks)? {
                        let candidate = entry?.path().join(format!("{old}.md"));
                        if candidate.is_file() {
                            existing = Some(candidate.clone());
                            writes.entry(candidate).or_insert(None);
                        }
                    }
                }
                if deleted(state, &project, "task", &id) {
                    continue;
                }
                // Deltas can arrive before creation. Do not publish an invalid task.
                if !fields.values.contains_key("title") || !fields.values.contains_key("status") {
                    continue;
                }
                let status = fields.values["status"]
                    .as_str()
                    .ok_or_else(|| invalid("invalid status"))?;
                let path = root
                    .join(".kanban/tasks")
                    .join(component(status)?)
                    .join(format!("{}.md", component(&display)?));
                let mut value = if let Some(old) = existing {
                    object_at(&old, "task")?
                } else {
                    serde_json::json!({})
                };
                let mut values = fields.values.clone();
                ids::references_to_displays(renames.project(&project), &mut values);
                for (field, new) in &values {
                    set_path(&mut value, field, new);
                }
                set_path(&mut value, "id", &Value::String(display));
                let description = value
                    .as_object_mut()
                    .expect("task map")
                    .remove("description")
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                writes.insert(
                    path,
                    Some(format!(
                        "---\n{}---\n{description}\n",
                        serde_yaml_ng::to_string(&value)?
                    )),
                );
                continue;
            }
            "project" => {
                if deleted(state, &project, "project", &id) {
                    writes.entry(root.join("project.yaml")).or_insert(None);
                    continue;
                }
                if !fields.values.contains_key("name") || !fields.values.contains_key("created_at")
                {
                    continue;
                }
                root.join("project.yaml")
            }
            "config" => {
                let path = if project.is_empty() {
                    store.join("config.yaml")
                } else {
                    root.join(".kanban/config.yaml")
                };
                if deleted(state, &project, "config", &id) {
                    writes.insert(path, None);
                    continue;
                }
                path
            }
            _ => continue,
        };
        let old_path = if entity == "config" && project.is_empty() {
            path.clone()
        } else {
            old_root.join(
                path.strip_prefix(&root)
                    .map_err(|e| invalid(&e.to_string()))?,
            )
        };
        let mut value = object_at(&old_path, &entity)?;
        if old_path != path {
            writes.entry(old_path).or_insert(None);
        }
        for (field, new) in &fields.values {
            set_path(&mut value, field, new);
        }
        if entity == "project" {
            set_path(&mut value, "id", &Value::String(alias.into()));
            value
                .as_object_mut()
                .expect("project map")
                .entry("work_path")
                .or_insert(Value::String(String::new()));
        }
        writes.insert(path, Some(serde_yaml_ng::to_string(&value)?));
    }
    // Local sessions follow their renumbered task; worktree and branch
    // names stay recorded on the task itself.
    for (project, _, old, new) in renames.renamed() {
        let alias = state
            .project_uids
            .iter()
            .find(|(_, uid)| **uid == project)
            .map(|(id, _)| id.as_str())
            .unwrap_or(&project);
        let sessions = store.join("projects").join(alias).join(".kanban/sessions");
        if !sessions.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&sessions)? {
            let path = entry?.path();
            // Already rebound by another renaming in this projection.
            if path.extension().is_none_or(|e| e != "yaml") || writes.contains_key(&path) {
                continue;
            }
            let mut session: serde_yaml_ng::Value =
                serde_yaml_ng::from_str(&fs::read_to_string(&path)?)?;
            if session
                .get("task_id")
                .and_then(serde_yaml_ng::Value::as_str)
                == Some(old.as_str())
            {
                session["task_id"] = serde_yaml_ng::Value::String(new.clone());
                writes.insert(path, Some(serde_yaml_ng::to_string(&session)?));
            }
        }
    }
    for c in conflicts {
        let task = if c.entity == "task" {
            Some(c.id.as_str())
        } else if c.entity == "message" {
            state
                .fields
                .get(&key(&c.project, "message", &c.id))
                .and_then(|f| f.values.get("task_id"))
                .and_then(Value::as_str)
        } else {
            None
        };
        let Some(task) = task else {
            continue;
        };
        let uid = format!("sync-conflict:{}", json(c)?);
        let time = timestamp(&c.winner);
        let note = serde_json::json!({"id": uid, "role": "system", "kind": "system", "status": "resolved", "author": "kanban", "origin": "kanban",
            "body": format!("Конфликт поля «{}»: оставлено значение с {} ({}), на {} было: {}", c.field, c.winner.2, time, c.loser.2, c.loser_value),
            "created_at": time, "updated_at": time});
        threads
            .entry((c.project.clone(), task.into()))
            .or_default()
            .push((c.winner.clone(), uid, note));
    }
    // A renumbered task carries its thread file along, even without messages.
    for (project, uid, old, _) in renames.renamed() {
        let alias = state
            .project_uids
            .iter()
            .find(|(_, u)| **u == project)
            .map(|(id, _)| id.as_str())
            .unwrap_or(&project);
        if store
            .join("projects")
            .join(alias)
            .join(".kanban/threads")
            .join(format!("{old}.yaml"))
            .is_file()
        {
            threads.entry((project, uid)).or_default();
        }
    }
    // Include empty threads for deleted messages so the last deletion is projected.
    for (k, fields) in &state.fields {
        let Ok((project, entity, _)) = serde_json::from_str::<(String, String, String)>(k) else {
            continue;
        };
        if entity == "message"
            && let Some(task) = fields.values.get("task_id").and_then(Value::as_str)
        {
            threads.entry((project, task.into())).or_default();
        }
    }
    // Message display aliases are keyed by thread display id, which a
    // renumbering moves: rebuild every touched thread's keys from scratch.
    let old_uids = std::mem::take(&mut state.message_uids);
    let touched: BTreeSet<(String, String)> = threads
        .keys()
        .flat_map(|(project, task)| {
            [
                Some(renames.display(project, task)),
                renames.previous(project, task),
            ]
            .into_iter()
            .flatten()
            .map(|display| (project.clone(), display))
        })
        .collect();
    state.message_uids = old_uids
        .iter()
        .filter(|(k, _)| {
            serde_json::from_str::<(String, String, String)>(k)
                .is_ok_and(|(p, t, _)| !touched.contains(&(p, t)))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for ((project, uid), mut entries) in threads {
        // Messages may arrive before their task; project them once it exists.
        // A legacy UID is also its display id.
        if !renames.known(&project, &uid) && ids::number(&uid).is_none() {
            continue;
        }
        let task = renames.display(&project, &uid);
        let previous = renames
            .previous(&project, &uid)
            .or_else(|| (!renames.known(&project, &uid)).then(|| uid.clone()));
        let alias = state
            .project_uids
            .iter()
            .find(|(_, uid)| **uid == project)
            .map(|(id, _)| id.as_str())
            .unwrap_or(&project);
        let path = store
            .join("projects")
            .join(alias)
            .join(".kanban/threads")
            .join(format!("{}.yaml", component(&task)?));
        let old_alias = previous_aliases
            .iter()
            .find(|(_, uid)| **uid == project)
            .map(|(id, _)| id.as_str())
            .unwrap_or(&project);
        let old_path = previous.as_ref().map(|old| {
            store
                .join("projects")
                .join(old_alias)
                .join(".kanban/threads")
                .join(format!("{old}.yaml"))
        });
        entries.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let mut used = BTreeSet::new();
        let mut aliases = BTreeMap::new();
        for (hlc, uid, value) in &entries {
            let display = value.get("id").and_then(Value::as_str).unwrap_or(uid);
            let mut display = if uid.starts_with("sync-conflict:") {
                format!("NOTE-{}-{}-{}", hlc.0, hlc.1, aliases.len())
            } else {
                display.into()
            };
            if !used.insert(display.clone()) {
                display = format!("{display}@{}", hlc.2);
                if !used.insert(display.clone()) {
                    display = format!("{display}@{uid}");
                    used.insert(display.clone());
                }
            }
            aliases.insert(uid.clone(), display);
        }
        let mut desired = Thread::new(&task);
        let mut clocks = BTreeMap::new();
        for (hlc, uid, mut value) in entries {
            let display = aliases[&uid].clone();
            value["id"] = Value::String(display.clone());
            if let Some(parent) = value
                .get("parent_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                value["parent_id"] = aliases
                    .get(&parent)
                    .map(|id| Value::String(id.clone()))
                    .unwrap_or(Value::Null);
            }
            value
                .as_object_mut()
                .expect("message map")
                .remove("task_id");
            value
                .as_object_mut()
                .expect("message map")
                .entry("created_at")
                .or_insert(Value::String(timestamp(&hlc)));
            value
                .as_object_mut()
                .expect("message map")
                .entry("updated_at")
                .or_insert(Value::String(timestamp(&hlc)));
            desired.messages.push(
                serde_json::from_value::<Message>(value).map_err(|e| invalid(&e.to_string()))?,
            );
            clocks.insert(display.clone(), (hlc, uid.clone()));
        }
        let mut current: Thread = match old_path.as_ref().map(fs::read_to_string) {
            Some(Ok(text)) => serde_yaml_ng::from_str(&text)?,
            None => Thread::new(&task),
            Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Thread::new(&task),
            Some(Err(e)) => return Err(e.into()),
        };
        let old_task = previous.as_deref().unwrap_or(&task);
        let replicated = |m: &&Message| {
            old_uids.contains_key(&key(&project, old_task, &m.id)) || m.id.starts_with("NOTE-")
        };
        let changed = !current
            .messages
            .iter()
            .filter(replicated)
            .eq(desired.messages.iter());
        current.messages.retain(|m| {
            !old_uids.contains_key(&key(&project, old_task, &m.id)) && !m.id.starts_with("NOTE-")
        });
        for (uid, display) in &aliases {
            state
                .message_uids
                .insert(key(&project, &task, display), uid.clone());
        }
        if let Some(old_path) = old_path
            && old_path != path
        {
            writes.entry(old_path).or_insert(None);
        }
        // merge_threads supplies the existing by-id union; replicated entries already
        // have deterministic UID-derived display ids and LWW field values.
        let rev = current.rev;
        let mut merged = merge_threads(current, &desired);
        merged.task_id = task.clone();
        merged.messages.sort_by(|a, b| {
            clocks
                .get(&a.id)
                .cmp(&clocks.get(&b.id))
                .then_with(|| a.id.cmp(&b.id))
        });
        merged.rev = if changed {
            rev.checked_add(1)
                .ok_or_else(|| invalid("thread revision exhausted"))?
        } else {
            rev
        };
        let text = serde_yaml_ng::to_string(&merged)?;
        writes.insert(path, Some(text));
    }
    Ok(())
}

fn timestamp(hlc: &Hlc) -> String {
    chrono::DateTime::from_timestamp_millis(hlc.0.min(i64::MAX as u64) as i64)
        .unwrap_or_default()
        .naive_utc()
        .format("%Y-%m-%dT%H:%M:%S%.6f")
        .to_string()
}
