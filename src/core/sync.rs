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
mod drift;
mod format;
pub mod hub;
mod ids;
mod migrate_v1;
mod migrate_v4;
mod projection;
mod prune;
mod run;
mod segments;
pub mod transport;
pub use apply::Conflict;
pub use format::{
    MIN_SYNC_PROTOCOL, OP_FORMAT, STORE_FORMAT, SYNC_PROTOCOL, accept_store_protocol, enable_sync,
    enabled, max_op_version, negotiate, node_role, protocol_bounds,
};
pub(crate) use ids::{historical_max, renamed as renamed_task};
pub use ids::{task_display, task_uid};
pub(crate) use projection::apply_meta;
pub use projection::{Claim, Lifecycle};
pub use run::{LandingDescriptor, RunDescriptor, RunState, remote_runs};
pub(crate) use run::{remote_runs_for_tasks, store_for_board, task_landings, task_runs};
pub use segments::change_token;
pub use transport::Page;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    /// Operation format. Unknown newer versions are refused; unknown fields stay in `extra`.
    #[serde(default = "format::default_op_version")]
    pub v: u32,
    pub op: String,
    pub hlc: Hlc,
    pub store: String,
    pub project: String,
    pub entity: String,
    pub id: String,
    pub set: Fields,
    pub prev: BTreeMap<String, Hlc>,
    pub actor: String,
    /// Forward-compatible fields this binary does not interpret.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Operation {
    pub fn blob_hash(&self) -> Option<&str> {
        if self.entity == "file" {
            self.set.get("blob")?.as_str()
        } else {
            None
        }
    }
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
    /// Local `(project, "task", display id)` -> replicated task UID.
    #[serde(default)]
    pub task_uids: BTreeMap<String, String>,
    /// Retired display ids of renumbered tasks -> task UID, for old references.
    #[serde(default)]
    pub task_aliases: BTreeMap<String, String>,
    pub compacted: VersionVector,
    /// Commutative creation / claim / tombstone clocks. Delivery order does not matter.
    #[serde(default)]
    pub lifecycle: BTreeMap<String, projection::Lifecycle>,
    /// Loaded from a few task shards only (`projection::load_tasks`). Never
    /// checkpointed or cached as the whole store.
    #[serde(skip)]
    pub(crate) partial: bool,
}

#[derive(Serialize, Deserialize)]
struct Intent {
    path: PathBuf,
    #[serde(default)]
    external: bool,
    #[serde(default)]
    observed: Option<drift::Entry>,
    segment: String,
    start_offset: u64,
    through_seq: u64,
    #[serde(default)]
    uids: projection::UidsDelta,
    #[serde(default)]
    run_projection: bool,
    /// A board file was written as part of this transaction.
    #[serde(default)]
    file: bool,
    /// Files a drift scan folded in this one transaction, with what it saw.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    batch: Vec<(PathBuf, Option<drift::Entry>)>,
}

thread_local! {
    static RECOVERING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
        format::ensure(store)?;
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
        if !RECOVERING.get() {
            journal.recover()?;
        }
        Ok(journal)
    }

    /// The whole-store projection, shared with the warm cache: read it, do not
    /// keep it around.
    pub fn state(&self) -> Result<std::sync::Arc<JournalState>> {
        self.settled(|| Ok(projection::shared_state(&self.root)?.0))
    }

    /// Runs `read` after pending remote applies and local intents settled.
    fn settled<T>(&self, read: impl FnOnce() -> Result<T>) -> Result<T> {
        apply::recover_remote(&self.root)?;
        let _guard = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        read()
    }

    /// State scoped to a few tasks of one board (see `projection::load_tasks`).
    pub(crate) fn task_state(
        &self,
        alias: &str,
        tasks: &[&str],
    ) -> Result<std::sync::Arc<JournalState>> {
        apply::recover_remote(&self.root)?;
        let _guard = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        projection::load_tasks(&self.root, |uids| {
            projection::task_scope(uids, alias, tasks)
        })
    }

    /// Retained operations in HLC order. `node` limits the result to one writer.
    /// Sequences at or below the compacted frontier are omitted.
    pub fn operations(&self, node: Option<&str>) -> Result<Vec<Operation>> {
        apply::recover_remote(&self.root)?;
        let _guard = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let compacted = projection::compacted(&self.root)?;
        let nodes = match node {
            Some(node) => vec![node.to_owned()],
            None => segments::node_names(&self.root)?,
        };
        let mut ops = Vec::new();
        for name in nodes {
            let floor = compacted.get(&name).copied().unwrap_or(0);
            ops.extend(segments::read_retained(&self.root, &name, floor)?);
        }
        ops.sort_by(|a, b| a.hlc.cmp(&b.hlc).then_with(|| a.op.cmp(&b.op)));
        Ok(ops)
    }

    /// Active field conflicts. Archived rows stay visible while they still win the field.
    pub fn conflicts(&self) -> Result<Vec<Conflict>> {
        let state = self.state()?;
        Ok(prune::stored_conflicts(&self.root)?
            .into_iter()
            .filter(|c| {
                state
                    .fields
                    .get(&key(&c.project, &c.entity, &c.id))
                    .and_then(|field| field.clocks.get(&c.field))
                    == Some(&c.winner)
            })
            .collect())
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
        if RECOVERING.get() {
            return Ok(());
        }
        let path = self.root.join("intent.json");
        if !path.exists() {
            return Ok(());
        }
        let intent: Intent = serde_json::from_str(&fs::read_to_string(&path)?)
            .map_err(|e| invalid(&e.to_string()))?;
        projection::forget(&self.root);
        prune::forget_groups(&self.root);
        let dir = self.root.join("journal").join(&self.node.node_id);
        if let Some(active) = segments_active(&dir)? {
            segments::repair_tail(&active)?;
        }
        let head = segments::head_seq(&dir).unwrap_or(0);
        let committed = head >= intent.through_seq && intent.through_seq > 0;
        if committed {
            let (mut state, _meta) = projection::load_state(&self.root)?;
            let mut uids = projection::uids_of(&state);
            intent.uids.apply(&mut uids);
            state.project_uids = uids.project_uids;
            state.message_uids = uids.message_uids;
            state.task_uids = uids.task_uids;
            state.task_aliases = uids.task_aliases;
            if !intent.uids.is_empty() {
                projection::save_uids(&self.root, &state)?;
            }
            if intent.run_projection {
                project_runs(self, &state)?;
            }
            if intent.file {
                if intent.external {
                    drift::record_observed(self, &intent.path, intent.observed.as_ref())?;
                } else {
                    drift::record(self, &intent.path)?;
                }
            }
            drift::record_observed_many(self, &intent.batch)?;
            fs::remove_file(&path)?;
            return Ok(());
        }
        if !intent.segment.is_empty() {
            let _ = segments::truncate(
                &self.root,
                &self.node.node_id,
                &intent.segment,
                intent.start_offset,
            );
        }
        fs::remove_file(&path)?;
        if intent.file {
            RECOVERING.set(true);
            let recovered = drift::reconcile_path(self, &intent.path);
            RECOVERING.set(false);
            recovered?;
        }
        Ok(())
    }

    fn clock(&self) -> Result<Hlc> {
        let mut clock: Hlc = load(&self.root.join("hlc"))?;
        if let Some(tail) = segments::tail_hlc(&self.root, &self.node.node_id)?
            && tail > clock
        {
            clock = tail;
        }
        Ok(clock)
    }
}

fn project_runs(journal: &Journal, state: &JournalState) -> Result<()> {
    for (k, fields) in &state.fields {
        let Ok((project, entity, id)) = serde_json::from_str::<(String, String, String)>(k) else {
            continue;
        };
        if entity == "run" && !fields.values.contains_key("command") {
            let (path, text) = run::projection(&journal.root, &project, &id, fields)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            atomic_write_text_raw(&path, &text)?;
        }
    }
    Ok(())
}

fn segments_active(dir: &Path) -> Result<Option<PathBuf>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "jsonl")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.len() == 20)
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files.pop())
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

fn commit_local(
    journal: &Journal,
    state: JournalState,
    before: &projection::Uids,
    ops: &[Operation],
    file: Option<FileWrite>,
    batch: &[(PathBuf, Option<drift::Entry>)],
) -> Result<()> {
    if ops.is_empty() {
        return Ok(());
    }
    let cached = prune::take_groups(&journal.root);
    let limits = format::JournalLimits::load(journal.root.parent().unwrap_or(&journal.root))?;
    let prepared = segments::prepare(&journal.root, &journal.node.node_id, &limits)?;
    let through = sequence(ops.last().expect("ops"))?;
    let intent = Intent {
        path: file.as_ref().map(|f| f.path.clone()).unwrap_or_default(),
        external: file.as_ref().is_some_and(|f| f.external),
        observed: file.as_ref().and_then(|f| f.observed.clone()),
        segment: prepared.segment,
        start_offset: prepared.start_offset,
        through_seq: through,
        uids: projection::uids_delta(before, &state),
        run_projection: ops
            .iter()
            .any(|op| op.entity == "run" && !op.set.contains_key("command")),
        file: file.is_some(),
        batch: batch.to_vec(),
    };
    atomic_write_text_raw(&journal.root.join("intent.json"), &json(&intent)?)?;
    if let Some(file) = &file {
        if !file.external {
            match &file.content {
                Some(text) => {
                    atomic_write_text_raw(&file.path, text)?;
                    if target(&file.path).is_some_and(|t| t.entity == "task")
                        && let Some(tasks) = file.path.parent().and_then(Path::parent)
                    {
                        for entry in fs::read_dir(tasks)? {
                            let other = entry?
                                .path()
                                .join(file.path.file_name().expect("task filename"));
                            if other != file.path && other.is_file() {
                                fs::remove_file(other)?;
                            }
                        }
                    }
                }
                None => match fs::remove_file(&file.path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                },
            }
        }
    }
    let span = segments::append(&journal.root, &journal.node.node_id, ops, &limits)?;
    if span.end_offset < span.start_offset {
        return Err(invalid("journal append shrank"));
    }
    if !intent.uids.is_empty() {
        projection::save_uids(&journal.root, &state)?;
    }
    if intent.run_projection {
        project_runs(journal, &state)?;
    }
    if let Some(file) = &file {
        if file.external {
            drift::record_observed(journal, &file.path, file.observed.as_ref())?;
        } else {
            drift::record(journal, &file.path)?;
        }
    }
    drift::record_observed_many(journal, batch)?;
    match fs::remove_file(journal.root.join("intent.json")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    projection::note_ops(&journal.root, ops.len() as u64)?;
    let meta = projection::load_prune(&journal.root)?;
    let pruned = if projection::checkpoint(&journal.root, &state, &meta, false)?
        && format::solo_store(journal.root.parent().unwrap_or(&journal.root))
    {
        let _ = journal.prune(&BTreeMap::new());
        true
    } else {
        false
    };
    if !pruned && !state.partial {
        if let Some(mut cached) = cached {
            let conflicts = prune::ingest(
                &state,
                &mut cached.groups,
                &mut cached.ops,
                &cached.conflicts,
                ops,
            )?;
            prune::store_groups(&journal.root, cached.groups, cached.ops, conflicts);
        }
        projection::remember(&journal.root, state, &meta);
    }
    Ok(())
}

struct FileWrite {
    path: PathBuf,
    content: Option<String>,
    external: bool,
    observed: Option<drift::Entry>,
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
            } else if (text.starts_with(".kanban/instructions/")
                || text.starts_with(".kanban/assets/images/"))
                && transport::allowed_path(&format!("projects/{project}/{text}")).is_ok()
            {
                "file"
            } else {
                return None;
            };
            return Some(Target {
                store,
                project,
                entity,
                id: if entity == "file" {
                    text.to_owned()
                } else {
                    path.file_stem()?.to_str()?.to_owned()
                },
            });
        }
    }
    if path.file_name()? == "config.yaml" && path.parent().is_some_and(enabled) {
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

/// Top-level `<store>/config.yaml` keys that stay on this machine.
pub(crate) fn local_config_sections() -> &'static [&'static str] {
    local_fields("config", true)
}

/// `project.yaml` keys that stay on this machine (not replicated).
pub(crate) fn local_project_fields() -> &'static [&'static str] {
    local_fields("project", false)
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
            "queued_at",
            "conflict_held",
            "resolver_runs",
        ],
        "project" => &["work_path", "migrated_from", "last_opened_at"],
        "config" if global => &[
            "node",
            "sync",
            "serve",
            "net",
            "daemon",
            "tui",
            "auto_mode",
            "recent_models",
            "backup",
        ],
        _ => &[],
    }
}

fn fields(value: &Value, entity: &str, global: bool) -> Fields {
    let local = local_fields(entity, global);
    let mut result = Fields::new();
    if let Some(map) = value.as_object() {
        for (field, value) in map {
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

/// Open the journal and fold on-disk files into it. Callers must not hold a
/// board lock: reconcile takes store, then boards, then sync.
pub fn activate(store: &Path) -> Result<usize> {
    if !enabled(store) {
        return Ok(0);
    }
    Journal::open(store)?.reconcile()
}

/// CLI start: finish a crashed write and arm a store that was never scanned.
/// An armed store skips the full scan; capture folds bypass writes of each
/// path it overwrites, and serve/TUI keep the periodic full scan.
pub fn prepare(store: &Path) -> Result<()> {
    if enabled(store) {
        let journal = Journal::open(store)?;
        if !format::armed(store) {
            journal.reconcile()?;
        }
    } else if store.join("sync/intent.json").is_file() {
        // A crash left a redo that still has to finish the board write.
        Journal::open(store)?;
    }
    Ok(())
}

/// Called by the atomic writer, while its caller holds the board/store lock.
pub(crate) fn capture(path: &Path, content: Option<&str>) -> Result<bool> {
    let Some(found) = target(path) else {
        return Ok(false);
    };
    if found.entity == "file" {
        // Binary attachments and arbitrary instruction text are reconciled as blobs.
        return Ok(false);
    }
    // A disabled store must not create sync/, take the journal lock, or fsync.
    // Dropping the arm marker forces the next activation to reconcile first:
    // capture reads "before" from disk, so a delta made while sync was off
    // would otherwise be skipped forever.
    if !enabled(&found.store) {
        format::disarm(&found.store)?;
        return Ok(false);
    }
    if !format::armed(&found.store) {
        return Ok(false);
    }
    capture_inner(path, content, None, None)
}

/// Display ids of the tasks a captured file can touch: the task itself, the
/// thread's task, or a session's task.
fn scope_display(target: &Target, path: &Path, content: Option<&str>) -> Result<Vec<String>> {
    Ok(match target.entity {
        "task" | "message" => vec![target.id.clone()],
        "session" => {
            let task_of = |text: &str| {
                serde_yaml_ng::from_str::<Value>(text)
                    .ok()
                    .and_then(|v| v.get("task_id")?.as_str().map(str::to_owned))
            };
            let before = match fs::read_to_string(path) {
                Ok(text) => task_of(&text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            content
                .and_then(task_of)
                .into_iter()
                .chain(before)
                .collect()
        }
        _ => Vec::new(),
    })
}

/// Operations for one captured file, planned on `state`. Callers own the
/// state, clock and operation list, so a drift scan can plan every file on
/// one state and commit them in a single transaction.
#[allow(clippy::too_many_arguments)]
fn plan_capture(
    journal: &Journal,
    target: &Target,
    path: &Path,
    content: Option<&str>,
    before_override: Option<Option<&str>>,
    state: &mut JournalState,
    clock: &mut Hlc,
    operations: &mut Vec<Operation>,
) -> Result<()> {
    let project = if target.project.is_empty() {
        String::new()
    } else {
        if !state.project_uids.contains_key(&target.project) {
            state.project_uids.insert(target.project.clone(), uuid()?);
        }
        state.project_uids[&target.project].clone()
    };
    let before_text = if let Some(before) = before_override {
        before.map(str::to_owned)
    } else if target.entity == "task" && !path.exists() {
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
    let task_uid = if target.entity == "task" {
        Some(ids::capture_uid(
            state,
            &project,
            &target.id,
            before.is_null(),
        )?)
    } else {
        None
    };
    run::capture(
        journal,
        target,
        &project,
        (&before, &after),
        state,
        clock,
        operations,
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
        let mut id = task_uid.clone().unwrap_or_else(|| display_id.clone());
        if target.entity == "task" {
            ids::references_to_uids(state, &project, &mut set);
        }
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
            set.insert(
                "task_id".into(),
                Value::String(ids::task_uid(state, &project, &target.id)),
            );
        }
        let entity = if after.is_null() {
            set.clear();
            set.insert("deleted".into(), Value::Bool(true));
            set.insert("entity".into(), Value::String(target.entity.into()));
            "tombstone"
        } else {
            target.entity
        };
        *clock = clock.tick(now_ms(), &journal.node.node_id)?;
        let previous = state.fields.get(&key(&project, entity, &id));
        let prev = set
            .keys()
            .filter_map(|field| Some((field.clone(), previous?.clocks.get(field)?.clone())))
            .collect();
        let op = Operation {
            v: OP_FORMAT,
            op: op_id,
            hlc: clock.clone(),
            store: journal.node.store_id.clone(),
            project: project.clone(),
            entity: entity.into(),
            id,
            set,
            prev,
            extra: BTreeMap::new(),
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
        apply_meta(state, &op);
        if target.entity == "message" && after.is_null() {
            state
                .message_uids
                .remove(&key(&project, &target.id, &display_id));
        }
        operations.push(op);
        if !after.is_null()
            && apply::deleted(
                state,
                &project,
                target.entity,
                &operations.last().expect("operation").id,
            )
        {
            *clock = clock.tick(now_ms(), &journal.node.node_id)?;
            let restore = Operation {
                v: OP_FORMAT,
                op: format!(
                    "{}:{}",
                    journal.node.node_id,
                    state.vector[&journal.node.node_id] + 1
                ),
                hlc: clock.clone(),
                store: journal.node.store_id.clone(),
                project: project.clone(),
                entity: "restore".into(),
                id: operations.last().expect("operation").id.clone(),
                set: BTreeMap::from([("entity".into(), Value::String(target.entity.into()))]),
                prev: BTreeMap::new(),
                extra: BTreeMap::new(),
                actor: "human".into(),
            };
            apply_meta(state, &restore);
            operations.push(restore);
        }
    }
    if target.entity == "message" {
        let id = ids::task_uid(state, &project, &target.id);
        let deleted = Value::Bool(content.is_none());
        let previous = state.fields.get(&key(&project, "thread", &id));
        if previous.and_then(|f| f.values.get("deleted")) != Some(&deleted) {
            let prev = previous
                .and_then(|f| f.clocks.get("deleted"))
                .map(|clock| BTreeMap::from([("deleted".into(), clock.clone())]))
                .unwrap_or_default();
            *clock = clock.tick(now_ms(), &journal.node.node_id)?;
            let op = Operation {
                v: OP_FORMAT,
                op: format!(
                    "{}:{}",
                    journal.node.node_id,
                    state
                        .vector
                        .get(&journal.node.node_id)
                        .copied()
                        .unwrap_or(0)
                        + 1
                ),
                hlc: clock.clone(),
                store: journal.node.store_id.clone(),
                project: project.clone(),
                entity: "thread".into(),
                id,
                set: BTreeMap::from([("deleted".into(), deleted)]),
                prev,
                extra: BTreeMap::new(),
                actor: "human".into(),
            };
            apply_meta(state, &op);
            operations.push(op);
        }
    }
    Ok(())
}

fn capture_inner(
    path: &Path,
    content: Option<&str>,
    before_override: Option<Option<&str>>,
    observed: Option<drift::Entry>,
) -> Result<bool> {
    let Some(target) = target(path) else {
        return Ok(false);
    };
    let journal = Journal::open(&target.store)?;
    let _guard = lock_file_reentrant(&journal.root.join(".lock"))?;
    journal.recover()?;
    if before_override.is_none() {
        drift::reconcile_for_write(&journal, path)?;
    }
    let scoped = scope_display(&target, path, content)?;
    let mut state = projection::take_tasks(&journal.root, |uids| {
        projection::task_scope(
            uids,
            &target.project,
            &scoped.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    })?;
    let before_uids = projection::uids_of(&state);
    let mut clock: Hlc = journal.clock()?;
    let mut operations = Vec::new();
    plan_capture(
        &journal,
        &target,
        path,
        content,
        before_override,
        &mut state,
        &mut clock,
        &mut operations,
    )?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if operations.is_empty() {
        if before_override.is_some() {
            drift::record_observed(&journal, &absolute, observed.as_ref())?;
        } else {
            if let Some(text) = content {
                atomic_write_text_raw(&absolute, text)?;
            } else if absolute.exists() {
                fs::remove_file(&absolute)?;
            }
            drift::record(&journal, &absolute)?;
        }
        // No operation means no field changed; a uid assigned along the way was
        // never saved, so only an untouched uid table may go back to the cache.
        if projection::same_uids(&before_uids, &state) {
            let meta = projection::load_prune(&journal.root)?;
            projection::remember(&journal.root, state, &meta);
        }
        return Ok(true);
    }
    commit_local(
        &journal,
        state,
        &before_uids,
        &operations,
        Some(FileWrite {
            path: absolute,
            content: content.map(str::to_owned),
            external: before_override.is_some(),
            observed,
        }),
        &[],
    )?;
    Ok(true)
}

/// A file a drift scan read: its text before (from the index blob) and now,
/// and the stat/hash entry the scan observed.
struct Observed {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
    entry: Option<drift::Entry>,
}

/// Fold every file of a drift scan into the journal as one transaction: one
/// whole-store state, one intent, one append, one uid save and one index
/// append, instead of a commit per file. The caller holds the store, board
/// and sync locks, so files are planned in the caller's order (project
/// metadata first).
fn capture_batch(journal: &Journal, files: Vec<Observed>) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let _guard = lock_file_reentrant(&journal.root.join(".lock"))?;
    journal.recover()?;
    let (mut state, _) = projection::load_state(&journal.root)?;
    let before_uids = projection::uids_of(&state);
    let mut clock: Hlc = journal.clock()?;
    let mut operations = Vec::new();
    let mut observed = Vec::with_capacity(files.len());
    for file in files {
        if let Some(target) = target(&file.path) {
            plan_capture(
                journal,
                &target,
                &file.path,
                file.after.as_deref(),
                Some(file.before.as_deref()),
                &mut state,
                &mut clock,
                &mut operations,
            )?;
        }
        observed.push((file.path, file.entry));
    }
    if operations.is_empty() {
        drift::record_observed_many(journal, &observed)?;
        if projection::same_uids(&before_uids, &state) {
            let meta = projection::load_prune(&journal.root)?;
            projection::remember(&journal.root, state, &meta);
        }
        return Ok(());
    }
    commit_local(journal, state, &before_uids, &operations, None, &observed)
}

#[cfg(test)]
mod local_first_tests {
    use super::*;
    use std::path::Path;

    fn task_path(store: &Path) -> std::path::PathBuf {
        store.join("projects/demo/.kanban/tasks/todo/TASK-001.md")
    }

    #[test]
    fn capture_when_disabled_does_not_create_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = task_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(!capture(&path, Some("---\nid: TASK-001\n---\n")).unwrap());
        assert!(!dir.path().join("sync").exists());
    }

    #[test]
    fn capture_when_enabled_but_unarmed_skips_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.yaml"), "sync:\n  enabled: true\n").unwrap();
        let path = task_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(!capture(&path, Some("---\nid: TASK-001\n---\n")).unwrap());
        assert!(!dir.path().join("sync").exists());
        assert!(!format::armed(dir.path()));
    }
}
