//! Machine-local identity, persistent HLC and storage-level operation capture.
//! The pending transaction is a redo record: recovery finishes both the board
//! write and journal append before allowing another writer into this store.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::{KanbanError, Result};
use super::storage::{atomic_write_text_raw, lock_file_reentrant};

mod apply;
pub(crate) mod control;
mod lease;
pub mod relay;
mod run;
mod transport;
pub use apply::Conflict;
pub use lease::IdLease;
pub(crate) use lease::allocate as allocate_task_id;
pub use run::{LandingDescriptor, RunDescriptor, RunState, remote_runs};
pub(crate) use run::{remote_runs_for_tasks, store_for_board, task_landings, task_runs};
pub use transport::{Page, Snapshot, install_snapshot};

type Fields = BTreeMap<String, Value>;
pub type VersionVector = BTreeMap<String, u64>;

thread_local! {
    static REQUEST_ACTOR: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// HTTP requests must not inherit the serve process's agent environment.
pub(crate) fn with_actor<T>(actor: &'static str, action: impl FnOnce() -> T) -> T {
    struct Reset(Option<&'static str>);
    impl Drop for Reset {
        fn drop(&mut self) {
            REQUEST_ACTOR.set(self.0);
        }
    }
    let _reset = Reset(REQUEST_ACTOR.replace(Some(actor)));
    action()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    #[default]
    Laptop,
    Server,
    Mobile,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeDial {
    #[default]
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub node_id: String,
    pub name: String,
    pub role: NodeRole,
    pub store_id: String,
    #[serde(default)]
    pub dial: NodeDial,
}

impl NodeIdentity {
    pub fn outbound_only(&self) -> bool {
        self.role == NodeRole::Mobile || self.dial == NodeDial::Outbound
    }
}

/// Tuple serialization is the wire format, including the node tie-breaker.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hlc(pub u64, pub u64, pub String);

impl Hlc {
    pub fn tick(&self, now_ms: u64, node: &str) -> Result<Self> {
        let wall = now_ms.max(self.0);
        let counter = if wall == self.0 {
            self.1
                .checked_add(1)
                .ok_or_else(|| invalid("HLC counter exhausted"))?
        } else {
            0
        };
        Ok(Self(wall, counter, node.to_owned()))
    }

    pub fn observe(&mut self, remote: &Self, now_ms: u64, max_skew_ms: u64) -> Result<()> {
        if remote.0.saturating_sub(now_ms) > max_skew_ms {
            return Err(invalid("SYNC_CLOCK_SKEW"));
        }
        if remote > self {
            *self = remote.clone();
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub op: String,
    pub hlc: Hlc,
    pub store: String,
    pub project: String,
    pub entity: String,
    pub id: String,
    pub set: Fields,
    pub prev: BTreeMap<String, Hlc>,
    pub actor: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FieldState {
    pub values: Fields,
    pub clocks: BTreeMap<String, Hlc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JournalState {
    pub vector: VersionVector,
    pub fields: BTreeMap<String, FieldState>,
    /// Local display-id -> creation op_id. Never written into thread YAML.
    pub message_uids: BTreeMap<String, String>,
    pub project_uids: BTreeMap<String, String>,
    pub compacted: VersionVector,
    /// Retained causal versions make conflict detection independent of delivery order.
    #[serde(default)]
    pub operations: BTreeMap<String, Operation>,
}

#[derive(Serialize, Deserialize)]
struct Pending {
    path: PathBuf,
    content: Option<String>,
    operations: Vec<Operation>,
    state: JournalState,
}

pub struct Journal {
    root: PathBuf,
    pub node: NodeIdentity,
}

fn invalid(message: &str) -> KanbanError {
    KanbanError::Invalid(message.to_owned())
}

fn json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| invalid(&e.to_string()))
}

fn load<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| invalid(&e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

fn uuid() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut id = String::with_capacity(36);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in bytes.into_iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            id.push('-');
        }
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(id)
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

impl Journal {
    /// Creates identity once under a store-local lock. Global `node` settings
    /// supply name/role; node.yaml owns the immutable node/store UUIDs.
    pub fn open(store: &Path) -> Result<Self> {
        let root = store.join("sync");
        fs::create_dir_all(&root)?;
        transport::recover_bootstrap(store)?;
        apply::recover_remote(&root)?;
        let _guard = lock_file_reentrant(&root.join(".lock"))?;
        let path = root.join("node.yaml");
        let mut node: NodeIdentity = if path.exists() {
            serde_yaml_ng::from_str(&fs::read_to_string(&path)?)?
        } else {
            NodeIdentity {
                node_id: uuid()?,
                name: "laptop".into(),
                role: NodeRole::Laptop,
                store_id: uuid()?,
                dial: NodeDial::Inbound,
            }
        };
        if let Ok(text) = fs::read_to_string(store.join("config.yaml")) {
            let config: Value = serde_yaml_ng::from_str(&text)?;
            if let Some(settings) = config.get("node") {
                if let Some(name) = settings.get("name").and_then(Value::as_str) {
                    if name.is_empty()
                        || !name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    {
                        return Err(invalid("node.name must be a nonempty lowercase slug"));
                    }
                    node.name = name.to_owned();
                }
                if let Some(role) = settings.get("role") {
                    node.role = serde_json::from_value(role.clone())
                        .map_err(|e| invalid(&e.to_string()))?;
                }
                if let Some(dial) = settings.get("dial") {
                    node.dial = serde_json::from_value(dial.clone())
                        .map_err(|e| invalid(&e.to_string()))?;
                }
                if let Some(id) = settings.get("node_id").and_then(Value::as_str)
                    && id != node.node_id
                {
                    return Err(invalid("node.node_id is immutable"));
                }
            }
        }
        if node.role == NodeRole::Mobile {
            node.dial = NodeDial::Outbound;
        }
        if !path.exists() || fs::read_to_string(&path)? != serde_yaml_ng::to_string(&node)? {
            atomic_write_text_raw(&path, &serde_yaml_ng::to_string(&node)?)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        // Keep the global machine-local section explicit without changing any
        // legacy task/thread/project representation.
        let config_path = store.join("config.yaml");
        let mut config: serde_yaml_ng::Mapping = match fs::read_to_string(&config_path) {
            Ok(text) => serde_yaml_ng::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => return Err(e.into()),
        };
        let settings = serde_yaml_ng::to_value(
            serde_json::json!({"node_id": node.node_id, "name": node.name, "role": node.role, "dial": node.dial}),
        )?;
        if config.get("node") != Some(&settings) {
            config.insert("node".into(), settings);
            atomic_write_text_raw(&config_path, &serde_yaml_ng::to_string(&config)?)?;
        }
        let journal = Self { root, node };
        journal.recover()?;
        Ok(journal)
    }

    pub fn state(&self) -> Result<JournalState> {
        apply::recover_remote(&self.root)?;
        let _guard = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        load(&self.root.join("meta/state.json"))
    }

    /// Active field conflicts; historical records remain in `meta/conflicts.jsonl`.
    pub fn conflicts(&self) -> Result<Vec<Conflict>> {
        let state = self.state()?;
        let text = match fs::read_to_string(self.root.join("meta/conflicts.jsonl")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<Conflict>(line).map_err(|e| invalid(&e.to_string())))
            .filter(|conflict| match conflict {
                Ok(c) => {
                    state
                        .fields
                        .get(&key(&c.project, &c.entity, &c.id))
                        .and_then(|field| field.clocks.get(&c.field))
                        == Some(&c.winner)
                }
                Err(_) => true,
            })
            .collect()
    }

    pub fn observe(&self, remote: &Hlc, max_skew_ms: u64) -> Result<()> {
        apply::recover_remote(&self.root)?;
        let _guard = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let mut clock: Hlc = load(&self.root.join("hlc"))?;
        clock.observe(remote, now_ms(), max_skew_ms)?;
        atomic_write_text_raw(&self.root.join("hlc"), &json(&clock)?)
    }

    fn recover(&self) -> Result<()> {
        let path = self.root.join("pending.json");
        if !path.exists() {
            return Ok(());
        }
        let pending: Pending = serde_json::from_str(&fs::read_to_string(&path)?)
            .map_err(|e| invalid(&e.to_string()))?;
        self.finish(&pending, true)?;
        fs::remove_file(path)?;
        Ok(())
    }

    fn finish(&self, pending: &Pending, recovering: bool) -> Result<()> {
        match &pending.content {
            Some(text) => {
                atomic_write_text_raw(&pending.path, text)?;
                if target(&pending.path).is_some_and(|t| t.entity == "task")
                    && let Some(tasks) = pending.path.parent().and_then(Path::parent)
                {
                    for entry in fs::read_dir(tasks)? {
                        let other = entry?
                            .path()
                            .join(pending.path.file_name().expect("task filename"));
                        if other != pending.path && other.is_file() {
                            fs::remove_file(other)?;
                        }
                    }
                }
            }
            None => match fs::remove_file(&pending.path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            },
        }
        fs::create_dir_all(self.root.join("journal"))?;
        fs::create_dir_all(self.root.join("meta"))?;
        let path = self
            .root
            .join("journal")
            .join(format!("{}.jsonl", self.node.node_id));
        let mut seen = BTreeSet::new();
        // Only recovery scans the journal; normal writes are append-only.
        // A torn final line is truncated before replaying the redo record.
        if recovering && path.exists() {
            let file = fs::File::open(&path)?;
            let mut reader = BufReader::new(file);
            let mut offset = 0;
            loop {
                let mut line = String::new();
                let count = reader.read_line(&mut line)?;
                if count == 0 {
                    break;
                }
                if !line.ends_with('\n') {
                    OpenOptions::new()
                        .write(true)
                        .open(&path)?
                        .set_len(offset)?;
                    break;
                }
                let op: Operation =
                    serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
                if pending.operations.iter().any(|pending| pending.op == op.op) {
                    seen.insert(op.op);
                }
                offset += count as u64;
            }
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        for op in &pending.operations {
            if !seen.contains(&op.op) {
                writeln!(file, "{}", json(op)?)?;
            }
        }
        file.sync_all()?;
        atomic_write_text_raw(&self.root.join("meta/state.json"), &json(&pending.state)?)?;
        for op in pending
            .operations
            .iter()
            .filter(|op| op.entity == "run" && !op.set.contains_key("command"))
        {
            let fields = &pending.state.fields[&key(&op.project, "run", &op.id)];
            let (path, text) = run::projection(&self.root, &op.project, &op.id, fields)?;
            fs::create_dir_all(path.parent().expect("run parent"))?;
            atomic_write_text_raw(&path, &text)?;
        }
        if let Some(op) = pending.operations.last() {
            atomic_write_text_raw(&self.root.join("hlc"), &json(&op.hlc)?)?;
        }
        Ok(())
    }

    /// Compact only a prefix acknowledged by every known peer. The snapshot
    /// is durable before replacing the journal, and includes field clocks and
    /// tombstones. Callers must supply all paired peers, not just online peers.
    pub fn compact(&self, peers: &[VersionVector]) -> Result<u64> {
        apply::recover_remote(&self.root)?;
        let _guard = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let mut state: JournalState = load(&self.root.join("meta/state.json"))?;
        let node = &self.node.node_id;
        let own = *state.vector.get(node).unwrap_or(&0);
        let through = peers
            .iter()
            .map(|v| *v.get(node).unwrap_or(&0))
            .fold(own, u64::min);
        if through <= *state.compacted.get(node).unwrap_or(&0) {
            return Ok(through);
        }
        // Snapshot must describe exactly the compacted prefix, not later ops.
        let snapshot_path = self.root.join("snapshot/state.json");
        let mut snapshot: JournalState = load(&snapshot_path)?;
        let journal_path = self.root.join("journal").join(format!("{node}.jsonl"));
        let mut suffix = String::new();
        for line in BufReader::new(fs::File::open(&journal_path)?).lines() {
            let line = line?;
            let op: Operation = serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
            let seq = sequence(&op)?;
            if seq <= through {
                apply_meta(&mut snapshot, &op);
            } else {
                suffix.push_str(&line);
                suffix.push('\n');
            }
        }
        snapshot.message_uids = state.message_uids.clone();
        snapshot.project_uids = state.project_uids.clone();
        snapshot.compacted.insert(node.clone(), through);
        fs::create_dir_all(self.root.join("snapshot"))?;
        atomic_write_text_raw(&snapshot_path, &json(&snapshot)?)?;
        atomic_write_text_raw(&journal_path, &suffix)?;
        state.compacted.insert(node.clone(), through);
        atomic_write_text_raw(&self.root.join("meta/state.json"), &json(&state)?)?;
        Ok(through)
    }
}

fn sequence(op: &Operation) -> Result<u64> {
    op.op
        .rsplit_once(':')
        .and_then(|(_, seq)| seq.parse().ok())
        .ok_or_else(|| invalid("invalid operation id"))
}

fn key(project: &str, entity: &str, id: &str) -> String {
    // JSON tuple avoids collisions between arbitrary identifiers.
    serde_json::to_string(&(project, entity, id)).expect("string tuple")
}

fn apply_meta(state: &mut JournalState, op: &Operation) {
    state.operations.insert(op.op.clone(), op.clone());
    let fields = state
        .fields
        .entry(key(&op.project, &op.entity, &op.id))
        .or_default();
    for (field, value) in &op.set {
        if fields.clocks.get(field).is_none_or(|clock| clock < &op.hlc) {
            fields.values.insert(field.clone(), value.clone());
            fields.clocks.insert(field.clone(), op.hlc.clone());
        }
    }
    state
        .vector
        .insert(op.hlc.2.clone(), sequence(op).expect("generated operation"));
}

struct Target {
    store: PathBuf,
    project: String,
    entity: &'static str,
    id: String,
}

fn target(path: &Path) -> Option<Target> {
    // Only store-owned boards are captured. In-place legacy boards keep their
    // exact behavior and are captured after migration into the store.
    for ancestor in path.ancestors().skip(1) {
        if ancestor.file_name().is_some_and(|n| n == "projects") {
            let store = ancestor.parent()?.to_path_buf();
            let relative = path.strip_prefix(ancestor).ok()?;
            let mut parts = relative.components();
            let project = parts.next()?.as_os_str().to_str()?.to_owned();
            let rest: PathBuf = parts.collect();
            let text = rest.to_str()?;
            let entity = if text == "project.yaml" {
                "project"
            } else if text == ".kanban/config.yaml" {
                "config"
            } else if text.starts_with(".kanban/tasks/") && text.ends_with(".md") {
                "task"
            } else if text.starts_with(".kanban/threads/") && text.ends_with(".yaml") {
                "message"
            } else if text.starts_with(".kanban/sessions/") && text.ends_with(".yaml") {
                "session"
            } else {
                return None;
            };
            return Some(Target {
                store,
                project,
                entity,
                id: path.file_stem()?.to_str()?.to_owned(),
            });
        }
    }
    if path.file_name()? == "config.yaml" && path.parent()?.join("sync/node.yaml").exists() {
        return Some(Target {
            store: path.parent()?.to_path_buf(),
            project: String::new(),
            entity: "config",
            id: "global".into(),
        });
    }
    None
}

fn parse(text: &str, entity: &str) -> Result<Value> {
    if entity == "task" {
        let mut parts = text.splitn(3, "---");
        parts.next();
        let mut value: Value = serde_yaml_ng::from_str(
            parts
                .next()
                .ok_or_else(|| invalid("missing task frontmatter"))?,
        )?;
        value
            .as_object_mut()
            .ok_or_else(|| invalid("task is not a mapping"))?
            .insert(
                "description".into(),
                Value::String(parts.next().unwrap_or("").trim().into()),
            );
        Ok(value)
    } else {
        Ok(serde_yaml_ng::from_str(text)?)
    }
}

fn flatten(value: &Value, prefix: &str, fields: &mut Fields) {
    if let Some(map) = value.as_object()
        && !map.is_empty()
    {
        for (name, value) in map {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            flatten(value, &path, fields);
        }
    } else if !prefix.is_empty() {
        fields.insert(prefix.to_owned(), value.clone());
    }
}

fn local_fields(entity: &str, global: bool) -> &'static [&'static str] {
    match entity {
        "task" => &[
            "session",
            "worktree",
            "branch",
            "base_commit",
            "integration",
            "landing_pending",
            "run_duplicate",
            "cleanup_retry_at",
            "run_phase",
            "crash_restarts",
            "restart_at",
            "auto_resumes",
        ],
        "project" => &["work_path", "migrated_from", "last_opened_at"],
        "config" if global => &[
            "node",
            "sync",
            "serve",
            "net",
            "daemon",
            "tui",
            "recent_models",
        ],
        _ => &[],
    }
}

fn fields(value: &Value, entity: &str, global: bool) -> Fields {
    let local = local_fields(entity, global);
    let mut result = Fields::new();
    if let Some(map) = value.as_object() {
        for (field, value) in map {
            if entity == "config" && global && field == "sync" {
                if let Some(encrypt) = value.get("relay_encrypt") {
                    result.insert("sync.relay_encrypt".into(), encrypt.clone());
                }
            }
            if local.contains(&field.as_str()) {
                continue;
            }
            if entity == "config" {
                flatten(value, field, &mut result);
            } else {
                result.insert(field.clone(), value.clone());
            }
        }
    }
    result
}

/// Called by the atomic writer, while its caller holds the board/store lock.
pub(crate) fn capture(path: &Path, content: Option<&str>) -> Result<bool> {
    let Some(target) = target(path) else {
        return Ok(false);
    };
    let journal = Journal::open(&target.store)?;
    let _guard = lock_file_reentrant(&journal.root.join(".lock"))?;
    journal.recover()?;
    let mut state: JournalState = load(&journal.root.join("meta/state.json"))?;
    let project = if target.project.is_empty() {
        String::new()
    } else {
        if !state.project_uids.contains_key(&target.project) {
            state.project_uids.insert(target.project.clone(), uuid()?);
        }
        state.project_uids[&target.project].clone()
    };
    let before_text = if target.entity == "task" && !path.exists() {
        let tasks = path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| invalid("invalid task path"))?;
        let mut text = None;
        if tasks.exists() {
            for entry in fs::read_dir(tasks)? {
                let other = entry?.path().join(path.file_name().expect("task filename"));
                if other.is_file() {
                    text = Some(fs::read_to_string(other)?);
                    break;
                }
            }
        }
        text
    } else {
        match fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        }
    };
    let before = before_text
        .as_deref()
        .map(|s| parse(s, target.entity))
        .transpose()?
        .unwrap_or(Value::Null);
    let after = content
        .map(|s| parse(s, target.entity))
        .transpose()?
        .unwrap_or(Value::Null);
    let mut clock: Hlc = load(&journal.root.join("hlc"))?;
    let mut operations = Vec::new();
    run::capture(
        &journal,
        &target,
        &project,
        (&before, &after),
        &mut state,
        &mut clock,
        &mut operations,
    )?;
    let mut changes = Vec::new();
    if target.entity == "message" {
        let old: BTreeMap<String, Value> = before
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| Some((v.get("id")?.as_str()?.to_owned(), v.clone())))
            .collect();
        let new: BTreeMap<String, Value> = after
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| Some((v.get("id")?.as_str()?.to_owned(), v.clone())))
            .collect();
        // Preserve creation order; lexical MSG ids put MSG-1000 before MSG-999
        // and would try to resolve a reply before assigning its parent's uid.
        let ids = after
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.get("id").and_then(Value::as_str))
            .chain(
                old.keys()
                    .filter(|id| !new.contains_key(*id))
                    .map(String::as_str),
            );
        for id in ids {
            let before = if state
                .message_uids
                .contains_key(&key(&project, &target.id, id))
            {
                old.get(id).cloned().unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            changes.push((
                id.to_owned(),
                before,
                new.get(id).cloned().unwrap_or(Value::Null),
            ));
        }
    } else if target.entity != "session" {
        changes.push((target.id.clone(), before, after));
    }
    for (display_id, before, after) in changes {
        let old = fields(&before, target.entity, target.project.is_empty());
        let new = fields(&after, target.entity, target.project.is_empty());
        let mut set = Fields::new();
        for field in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
            if old.get(field) != new.get(field) {
                set.insert(
                    field.clone(),
                    new.get(field).cloned().unwrap_or(Value::Null),
                );
            }
        }
        if target.entity == "project" && before.is_null() {
            set.insert("uid".into(), Value::String(project.clone()));
        }
        if set.is_empty() {
            continue;
        }
        let seq = state
            .vector
            .get(&journal.node.node_id)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid("operation sequence exhausted"))?;
        let op_id = format!("{}:{seq}", journal.node.node_id);
        let mut id = display_id.clone();
        if target.entity == "message" {
            let alias = key(&project, &target.id, &display_id);
            id = state
                .message_uids
                .entry(alias)
                .or_insert_with(|| op_id.clone())
                .clone();
            if let Some(parent) = set.get("parent_id").and_then(Value::as_str) {
                if let Some(uid) = state.message_uids.get(&key(&project, &target.id, parent)) {
                    set.insert("parent_id".into(), Value::String(uid.clone()));
                } else {
                    return Err(invalid("message parent has no sync uid"));
                }
            }
            set.insert("task_id".into(), Value::String(target.id.clone()));
        }
        let entity = if after.is_null() {
            set.clear();
            set.insert("deleted".into(), Value::Bool(true));
            set.insert("entity".into(), Value::String(target.entity.into()));
            "tombstone"
        } else {
            target.entity
        };
        clock = clock.tick(now_ms(), &journal.node.node_id)?;
        let previous = state.fields.get(&key(&project, entity, &id));
        let prev = set
            .keys()
            .filter_map(|field| Some((field.clone(), previous?.clocks.get(field)?.clone())))
            .collect();
        let op = Operation {
            op: op_id,
            hlc: clock.clone(),
            store: journal.node.store_id.clone(),
            project: project.clone(),
            entity: entity.into(),
            id,
            set,
            prev,
            actor: REQUEST_ACTOR
                .get()
                .unwrap_or_else(|| {
                    if std::env::var_os("KANBAN_SESSION").is_some() {
                        "agent"
                    } else {
                        "human"
                    }
                })
                .into(),
        };
        apply_meta(&mut state, &op);
        if target.entity == "message" && after.is_null() {
            state
                .message_uids
                .remove(&key(&project, &target.id, &display_id));
        }
        operations.push(op);
    }
    if operations.is_empty() {
        return Ok(false);
    }
    let absolute = path
        .parent()
        .ok_or_else(|| invalid("missing parent"))?
        .canonicalize()?
        .join(
            path.file_name()
                .ok_or_else(|| invalid("missing filename"))?,
        );
    let pending = Pending {
        path: absolute,
        content: content.map(str::to_owned),
        operations,
        state,
    };
    atomic_write_text_raw(&journal.root.join("pending.json"), &json(&pending)?)?;
    journal.finish(&pending, false)?;
    fs::remove_file(journal.root.join("pending.json"))?;
    Ok(true)
}
