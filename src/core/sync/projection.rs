//! Periodic projection of field state and commutative per-entity lifecycle,
//! sharded by task under `meta/projection/`. Deleting the directory rebuilds
//! both by replaying the segments.
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use super::format::{JournalLimits, STORE_FORMAT};
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub hlc: Hlc,
    pub id: String,
}

/// Creation, display-id claim and tombstone/restore clocks. Every field is a
/// min or a max, so delivery order does not matter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lifecycle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<Hlc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<Hlc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored: Option<Hlc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Through {
    seq: u64,
    #[serde(default)]
    segment: String,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct PruneMeta {
    #[serde(default)]
    pub stable: VersionVector,
    #[serde(default)]
    pub candidate: VersionVector,
    #[serde(default)]
    pub required: BTreeMap<String, u64>,
    #[serde(default)]
    pub frontier: BTreeMap<String, Hlc>,
}

/// Format 2 single-file projection, read once and then split into shards.
#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyProjection {
    #[serde(default)]
    through: BTreeMap<String, Through>,
    #[serde(default)]
    fields: BTreeMap<String, FieldState>,
    #[serde(default)]
    lifecycle: BTreeMap<String, Lifecycle>,
    #[serde(default)]
    compacted: VersionVector,
    #[serde(default)]
    prune: PruneMeta,
}

/// Commit point of a checkpoint. Every shard holds all operations on its
/// keys up to `through`; a shard may be newer after a crash, and replaying
/// later operations onto it is idempotent.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Head {
    #[serde(default = "super::format::snapshot_format_v1")]
    format: u32,
    #[serde(default)]
    through: BTreeMap<String, Through>,
    #[serde(default)]
    compacted: VersionVector,
    #[serde(default)]
    prune: PruneMeta,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Shard {
    #[serde(default)]
    fields: BTreeMap<String, FieldState>,
    #[serde(default)]
    lifecycle: BTreeMap<String, Lifecycle>,
}

/// Where a key is checkpointed: one file per task (its fields, run, thread
/// and messages), everything else in `misc.json`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ShardId {
    Misc,
    Task(String, String),
}

fn task_scoped(entity: &str) -> bool {
    matches!(entity, "task" | "run" | "thread")
}

/// Shard of a stored key. A message follows its `task_id`; a message whose
/// creation has not arrived yet stays in `misc` until it does.
fn shard_of(state: &JournalState, k: &str) -> ShardId {
    let Ok((project, entity, id)) = serde_json::from_str::<(String, String, String)>(k) else {
        return ShardId::Misc;
    };
    let entity = if matches!(entity.as_str(), "tombstone" | "restore") {
        match state
            .fields
            .get(k)
            .and_then(|f| f.values.get("entity"))
            .and_then(Value::as_str)
        {
            Some(real) => real.to_owned(),
            None => return ShardId::Misc,
        }
    } else {
        entity
    };
    if task_scoped(&entity) {
        return ShardId::Task(project, id);
    }
    if entity == "message"
        && let Some(task) = state
            .fields
            .get(&key(&project, "message", &id))
            .and_then(|f| f.values.get("task_id"))
            .and_then(Value::as_str)
    {
        return ShardId::Task(project, task.to_owned());
    }
    ShardId::Misc
}

/// Task shard an operation provably belongs to, from the operation alone.
/// `None` means it may touch any loaded shard and is always replayed.
fn op_task(op: &Operation) -> Option<(&str, &str)> {
    let entity = real_entity(op);
    if task_scoped(entity) {
        return Some((op.project.as_str(), op.id.as_str()));
    }
    if op.entity == "message" {
        return Some((op.project.as_str(), op.set.get("task_id")?.as_str()?));
    }
    None
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Uids {
    #[serde(default)]
    pub project_uids: BTreeMap<String, String>,
    #[serde(default)]
    pub message_uids: BTreeMap<String, String>,
    #[serde(default)]
    pub task_uids: BTreeMap<String, String>,
    #[serde(default)]
    pub task_aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct MapDelta {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

impl MapDelta {
    pub(super) fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }

    pub(super) fn apply(&self, map: &mut BTreeMap<String, String>) {
        for (k, v) in &self.set {
            map.insert(k.clone(), v.clone());
        }
        for k in &self.remove {
            map.remove(k);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct UidsDelta {
    #[serde(default)]
    pub project: MapDelta,
    #[serde(default)]
    pub message: MapDelta,
    #[serde(default)]
    pub task: MapDelta,
    #[serde(default)]
    pub alias: MapDelta,
}

impl UidsDelta {
    pub(super) fn is_empty(&self) -> bool {
        self.project.is_empty()
            && self.message.is_empty()
            && self.task.is_empty()
            && self.alias.is_empty()
    }

    pub(super) fn apply(&self, uids: &mut Uids) {
        self.project.apply(&mut uids.project_uids);
        self.message.apply(&mut uids.message_uids);
        self.task.apply(&mut uids.task_uids);
        self.alias.apply(&mut uids.task_aliases);
    }
}

pub(super) fn delta(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> MapDelta {
    // One merge walk over both sorted maps; this runs on every captured write.
    let mut out = MapDelta::default();
    let (mut b, mut a) = (before.iter().peekable(), after.iter().peekable());
    loop {
        match (b.peek(), a.peek()) {
            (None, None) => break,
            (Some((bk, _)), Some((ak, _))) if bk < ak => {
                out.remove.push((*bk).clone());
                b.next();
            }
            (Some((bk, bv)), Some((ak, av))) if bk == ak => {
                if bv != av {
                    out.set.insert((*ak).clone(), (*av).clone());
                }
                b.next();
                a.next();
            }
            (_, Some((ak, av))) => {
                out.set.insert((*ak).clone(), (*av).clone());
                a.next();
            }
            (Some((bk, _)), None) => {
                out.remove.push((*bk).clone());
                b.next();
            }
        }
    }
    out
}

pub(super) fn uids_of(state: &JournalState) -> Uids {
    Uids {
        project_uids: state.project_uids.clone(),
        message_uids: state.message_uids.clone(),
        task_uids: state.task_uids.clone(),
        task_aliases: state.task_aliases.clone(),
    }
}

pub(super) fn uids_delta(before: &Uids, after: &JournalState) -> UidsDelta {
    UidsDelta {
        project: delta(&before.project_uids, &after.project_uids),
        message: delta(&before.message_uids, &after.message_uids),
        task: delta(&before.task_uids, &after.task_uids),
        alias: delta(&before.task_aliases, &after.task_aliases),
    }
}

/// Whether `state` still carries exactly the uid tables of `uids`.
pub(super) fn same_uids(uids: &Uids, state: &JournalState) -> bool {
    uids.project_uids == state.project_uids
        && uids.message_uids == state.message_uids
        && uids.task_uids == state.task_uids
        && uids.task_aliases == state.task_aliases
}

/// `Uids` borrowed from a state, so saving them does not clone the tables.
#[derive(Serialize)]
struct UidsRef<'a> {
    project_uids: &'a BTreeMap<String, String>,
    message_uids: &'a BTreeMap<String, String>,
    task_uids: &'a BTreeMap<String, String>,
    task_aliases: &'a BTreeMap<String, String>,
}

fn apply_uids(state: &mut JournalState, uids: Uids) {
    state.project_uids = uids.project_uids;
    state.message_uids = uids.message_uids;
    state.task_uids = uids.task_uids;
    state.task_aliases = uids.task_aliases;
}

pub(super) fn real_entity(op: &Operation) -> &str {
    if matches!(op.entity.as_str(), "tombstone" | "restore") {
        op.set
            .get("entity")
            .and_then(Value::as_str)
            .unwrap_or(op.entity.as_str())
    } else {
        op.entity.as_str()
    }
}

/// Field LWW plus lifecycle. The vector advances only on the next contiguous sequence.
pub(crate) fn apply_meta(state: &mut JournalState, op: &Operation) {
    let seq = sequence(op).expect("generated operation");
    let floor = state.vector.get(&op.hlc.2).copied().unwrap_or(0);
    if seq == floor + 1 {
        state.vector.insert(op.hlc.2.clone(), seq);
    }
    let fields = state
        .fields
        .entry(key(&op.project, &op.entity, &op.id))
        .or_default();
    for (field, value) in &op.set {
        if op.entity == "config" && op.project.is_empty() && field == "sync.relay_encrypt" { continue; }
        if fields.clocks.get(field).is_none_or(|clock| clock < &op.hlc) {
            fields.values.insert(field.clone(), value.clone());
            fields.clocks.insert(field.clone(), op.hlc.clone());
        }
    }
    let life = state
        .lifecycle
        .entry(key(&op.project, real_entity(op), &op.id))
        .or_default();
    match op.entity.as_str() {
        "tombstone" => {
            if life.deleted.as_ref().is_none_or(|d| &op.hlc > d) {
                life.deleted = Some(op.hlc.clone());
            }
        }
        "restore" => {
            if life.restored.as_ref().is_none_or(|d| &op.hlc > d) {
                life.restored = Some(op.hlc.clone());
            }
        }
        _ => {
            if life.created.as_ref().is_none_or(|c| &op.hlc < c) {
                life.created = Some(op.hlc.clone());
            }
        }
    }
    if op.entity == "task"
        && let Some(id) = op.set.get("id").and_then(Value::as_str)
        && super::ids::number(id).is_some()
        && life.claim.as_ref().is_none_or(|c| op.hlc < c.hlc)
    {
        life.claim = Some(Claim {
            hlc: op.hlc.clone(),
            id: id.to_owned(),
        });
    }
}

fn legacy_path(sync: &Path) -> PathBuf {
    sync.join("meta/projection.json")
}

fn shards_dir(sync: &Path) -> PathBuf {
    sync.join("meta/projection")
}

pub(super) fn head_path(sync: &Path) -> PathBuf {
    shards_dir(sync).join("head.json")
}

fn shard_path(sync: &Path, shard: &ShardId) -> PathBuf {
    match shard {
        ShardId::Misc => shards_dir(sync).join("misc.json"),
        ShardId::Task(project, id) => {
            let name = drift::hash(key(project, "task", id).as_bytes());
            shards_dir(sync)
                .join("tasks")
                .join(format!("{}.json", &name[..32]))
        }
    }
}

fn uids_path(sync: &Path) -> PathBuf {
    sync.join("meta/uids.json")
}

fn since_path(sync: &Path) -> PathBuf {
    sync.join("meta/since.json")
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Ok(None),
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).map_err(|e| invalid(&e.to_string()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn read_head(sync: &Path) -> Result<Option<Head>> {
    read_json(&head_path(sync))
}

fn merge_shard(state: &mut JournalState, shard: Shard) {
    for (k, incoming) in shard.fields {
        let Some(current) = state.fields.get_mut(&k) else {
            state.fields.insert(k, incoming);
            continue;
        };
        for (field, clock) in incoming.clocks {
            if current.clocks.get(&field).is_none_or(|c| c < &clock) {
                if let Some(value) = incoming.values.get(&field) {
                    current.values.insert(field.clone(), value.clone());
                }
                current.clocks.insert(field, clock);
            }
        }
    }
    for (k, incoming) in shard.lifecycle {
        let life = state.lifecycle.entry(k).or_default();
        let earlier = |a: &Option<Hlc>, b: Option<Hlc>| match (a, b) {
            (Some(a), Some(b)) => Some(a.clone().min(b)),
            (a, b) => a.clone().or(b),
        };
        let later = |a: &Option<Hlc>, b: Option<Hlc>| match (a, b) {
            (Some(a), Some(b)) => Some(a.clone().max(b)),
            (a, b) => a.clone().or(b),
        };
        life.created = earlier(&life.created, incoming.created);
        life.deleted = later(&life.deleted, incoming.deleted);
        life.restored = later(&life.restored, incoming.restored);
        if let Some(claim) = incoming.claim
            && life.claim.as_ref().is_none_or(|c| claim.hlc < c.hlc)
        {
            life.claim = Some(claim);
        }
    }
}

/// Checkpointed state, either every shard or `misc` plus the given tasks.
/// Without a sharded head, a format 2 `projection.json` is read whole.
fn read_checkpoint(sync: &Path, tasks: Option<&BTreeSet<ShardId>>) -> Result<(JournalState, Head)> {
    let mut state = JournalState::default();
    let Some(head) = read_head(sync)? else {
        let legacy: LegacyProjection = read_json(&legacy_path(sync))?.unwrap_or_default();
        state.fields = legacy.fields;
        state.lifecycle = legacy.lifecycle;
        let head = Head {
            format: STORE_FORMAT,
            through: legacy.through,
            compacted: legacy.compacted,
            prune: legacy.prune,
        };
        return Ok((state, head));
    };
    let mut files = vec![shard_path(sync, &ShardId::Misc)];
    match tasks {
        Some(tasks) => files.extend(tasks.iter().map(|shard| shard_path(sync, shard))),
        None => match fs::read_dir(shards_dir(sync).join("tasks")) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry?.path();
                    if path.extension().is_some_and(|e| e == "json") {
                        files.push(path);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        },
    }
    for path in files {
        if let Some(shard) = read_json::<Shard>(&path)? {
            merge_shard(&mut state, shard);
        }
    }
    Ok((state, head))
}

/// Operations after the checkpoint frontier, contiguous and ahead.
fn unprojected(sync: &Path, head: &Head) -> Result<Vec<Operation>> {
    let mut nodes = segments::node_names(sync)?;
    nodes.extend(head.through.keys().cloned());
    nodes.sort();
    nodes.dedup();
    let mut ops = Vec::new();
    for node in &nodes {
        let through = head.through.get(node).map(|t| t.seq).unwrap_or(0);
        ops.extend(segments::read_after(sync, node, through)?);
        ops.extend(segments::read_ahead(sync, node)?);
    }
    Ok(ops)
}

pub(super) fn load_uids(sync: &Path) -> Result<Uids> {
    load(&uids_path(sync))
}

pub(super) fn save_uids(sync: &Path, state: &JournalState) -> Result<()> {
    fs::create_dir_all(sync.join("meta"))?;
    atomic_write_json(
        &uids_path(sync),
        &UidsRef {
            project_uids: &state.project_uids,
            message_uids: &state.message_uids,
            task_uids: &state.task_uids,
            task_aliases: &state.task_aliases,
        },
    )
}

/// Stream a JSON value to a temp file in the destination directory, then rename.
pub(super) fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_json_synced(path, value)?;
    let parent = path.parent().ok_or_else(|| invalid("path has no parent"))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// `atomic_write_json` without the directory fsync, for a batch that syncs
/// the directory once.
fn write_json_synced<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp_")
        .tempfile_in(parent)?;
    {
        let mut writer = BufWriter::new(tmp.as_file_mut());
        serde_json::to_writer(&mut writer, value).map_err(|e| invalid(&e.to_string()))?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    tmp.persist(path).map_err(|e| KanbanError::Io(e.error))?;
    Ok(())
}

struct StateSlot {
    token: u64,
    state: JournalState,
    prune: PruneMeta,
}

static STATE_CACHE: LazyLock<Mutex<HashMap<PathBuf, StateSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn state_cache() -> std::sync::MutexGuard<'static, HashMap<PathBuf, StateSlot>> {
    STATE_CACHE.lock().unwrap_or_else(|err| err.into_inner())
}

pub(super) fn forget(sync: &Path) {
    state_cache().remove(sync);
}

pub(super) fn cached_prune(sync: &Path) -> Option<PruneMeta> {
    let parent = sync.parent()?;
    let token = segments::change_token(parent)?;
    let cache = state_cache();
    let slot = cache.get(sync)?;
    (slot.token == token).then(|| slot.prune.clone())
}

/// Remember a projection that already matches the files `change_token` watches.
/// Takes the state by value: a whole-store projection is large, and cloning it
/// on every write made a full drift scan quadratic.
pub(super) fn remember(sync: &Path, state: JournalState, prune: &PruneMeta) {
    if state.partial {
        return;
    }
    let Some(parent) = sync.parent() else {
        return;
    };
    let Some(token) = segments::change_token(parent) else {
        return;
    };
    let mut cache = state_cache();
    if cache.len() > 32 {
        cache.clear();
    }
    cache.insert(
        sync.to_path_buf(),
        StateSlot {
            token,
            state,
            prune: prune.clone(),
        },
    );
}

pub(super) fn load_state(sync: &Path) -> Result<(JournalState, PruneMeta)> {
    if let Some(parent) = sync.parent()
        && let Some(token) = segments::change_token(parent)
    {
        let cache = state_cache();
        if let Some(slot) = cache.get(sync)
            && slot.token == token
        {
            return Ok((slot.state.clone(), slot.prune.clone()));
        }
    }
    let loaded = load_state_disk(sync)?;
    remember(sync, loaded.0.clone(), &loaded.1);
    Ok(loaded)
}

fn load_state_disk(sync: &Path) -> Result<(JournalState, PruneMeta)> {
    let (mut state, head) = read_checkpoint(sync, None)?;
    apply_uids(&mut state, load_uids(sync)?);
    for op in unprojected(sync, &head)? {
        apply_meta(&mut state, &op);
    }
    state.vector = segments::heads(sync, &head.compacted)?;
    state.compacted = head.compacted;
    Ok((state, head.prune))
}

/// Board alias and display ids to the project UID and task UIDs `load_tasks` reads.
pub(super) fn task_scope(uids: &Uids, alias: &str, displays: &[&str]) -> (String, Vec<String>) {
    let Some(project) = uids.project_uids.get(alias) else {
        return (String::new(), Vec::new());
    };
    let tasks = displays
        .iter()
        .map(|display| {
            uids.task_uids
                .get(&key(project, "task", display))
                .cloned()
                .unwrap_or_else(|| (*display).to_owned())
        })
        .collect();
    (project.clone(), tasks)
}

/// State for writes and reads scoped to a few tasks of one project: `misc`
/// plus those task shards, and only the later operations that can touch them.
/// `scope` maps the UID tables to the project UID and task UIDs. A warm
/// whole-store state is reused when the process already holds one.
pub(super) fn load_tasks(
    sync: &Path,
    scope: impl FnOnce(&Uids) -> (String, Vec<String>),
) -> Result<JournalState> {
    if let Some(parent) = sync.parent()
        && let Some(token) = segments::change_token(parent)
        && let Some(slot) = state_cache().get(sync)
        && slot.token == token
    {
        return Ok(slot.state.clone());
    }
    load_tasks_cold(sync, scope)
}

/// `load_tasks` for a writer that hands the state back through `remember`
/// (`commit_local` does): a warm whole-store state is moved out of the cache
/// instead of cloned. A writer that fails or bails out without remembering
/// leaves the cache empty, so the next load reads disk again.
pub(super) fn take_tasks(
    sync: &Path,
    scope: impl FnOnce(&Uids) -> (String, Vec<String>),
) -> Result<JournalState> {
    if let Some(parent) = sync.parent()
        && let Some(token) = segments::change_token(parent)
    {
        let mut cache = state_cache();
        if cache.get(sync).is_some_and(|slot| slot.token == token) {
            return Ok(cache.remove(sync).expect("cached slot").state);
        }
    }
    load_tasks_cold(sync, scope)
}

fn load_tasks_cold(
    sync: &Path,
    scope: impl FnOnce(&Uids) -> (String, Vec<String>),
) -> Result<JournalState> {
    if read_head(sync)?.is_none() {
        // A format 2 projection is one file; read it whole until it is sharded.
        return Ok(load_state_disk(sync)?.0);
    }
    let uids = load_uids(sync)?;
    let (project, tasks) = scope(&uids);
    let wanted: BTreeSet<ShardId> = tasks
        .iter()
        .map(|id| ShardId::Task(project.clone(), id.clone()))
        .collect();
    let (mut state, head) = read_checkpoint(sync, Some(&wanted))?;
    apply_uids(&mut state, uids);
    for op in unprojected(sync, &head)? {
        let relevant =
            op_task(&op).is_none_or(|(p, id)| p == project && tasks.iter().any(|t| t == id));
        if relevant {
            apply_meta(&mut state, &op);
        }
    }
    state.vector = segments::heads(sync, &head.compacted)?;
    state.compacted = head.compacted;
    state.partial = true;
    Ok(state)
}

#[derive(Serialize, Deserialize)]
struct Since {
    ops: u64,
    at_ms: u64,
}

fn read_since(sync: &Path) -> Result<Since> {
    match fs::read_to_string(since_path(sync)) {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or(Since {
            ops: 0,
            at_ms: now_ms(),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Since {
            ops: 0,
            at_ms: now_ms(),
        }),
        Err(e) => Err(e.into()),
    }
}

fn write_since(sync: &Path, since: &Since) -> Result<()> {
    fs::create_dir_all(sync.join("meta"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(since_path(sync))?;
    writeln!(file, "{}", json(since)?)?;
    // The counter is a hint shared by processes. A crash may repeat or skip a
    // snapshot window; the projection itself is what recovery replays.
    Ok(())
}

pub(super) fn checkpoint(
    sync: &Path,
    state: &JournalState,
    prune: &PruneMeta,
    force: bool,
) -> Result<bool> {
    let limits = JournalLimits::load(sync.parent().unwrap_or(sync))?;
    let now = now_ms();
    let since = read_since(sync)?;
    let due = force
        || since.ops >= limits.snapshot_ops
        || now.saturating_sub(since.at_ms) >= limits.snapshot_secs.saturating_mul(1000);
    let exists = head_path(sync).is_file() || legacy_path(sync).is_file();
    if !due && exists {
        return Ok(false);
    }
    // A first small write still records `since` so the window starts now.
    if !due && !exists && since.ops < limits.snapshot_ops && !force {
        if !since_path(sync).exists() {
            write_since(
                sync,
                &Since {
                    ops: since.ops,
                    at_ms: now,
                },
            )?;
        }
        return Ok(false);
    }
    if state.partial {
        // The scoped state lacks other tasks; the journal already holds them.
        let (full, _) = load_state_disk(sync)?;
        write_checkpoint(sync, &full, prune)?;
    } else {
        write_checkpoint(sync, state, prune)?;
    }
    write_since(sync, &Since { ops: 0, at_ms: now })?;
    Ok(true)
}

pub(super) fn note_ops(sync: &Path, count: u64) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    let now = now_ms();
    let mut since = read_since(sync)?;
    if !since_path(sync).exists() {
        since.at_ms = now;
    }
    since.ops = since.ops.saturating_add(count);
    write_since(sync, &since)
}

/// Rewrite the shards touched since the last head, then the head. Without a
/// sharded head (fresh store, or a format 2 `projection.json`) every shard is
/// written and the single file is removed afterwards.
pub(super) fn write_checkpoint(sync: &Path, state: &JournalState, prune: &PruneMeta) -> Result<()> {
    debug_assert!(!state.partial, "checkpoint of a task-scoped state");
    let old = read_head(sync)?;
    let dirty: Option<BTreeSet<ShardId>> = match &old {
        Some(head) => {
            let mut dirty = BTreeSet::new();
            for op in unprojected(sync, head)? {
                dirty.insert(shard_of(state, &key(&op.project, &op.entity, &op.id)));
                dirty.insert(shard_of(state, &key(&op.project, real_entity(&op), &op.id)));
            }
            Some(dirty)
        }
        None => None,
    };
    let mut shards: BTreeMap<ShardId, Shard> = BTreeMap::new();
    if let Some(dirty) = &dirty {
        for shard in dirty {
            shards.insert(shard.clone(), Shard::default());
        }
    }
    let wanted = |shard: &ShardId| dirty.as_ref().is_none_or(|d| d.contains(shard));
    for (k, fields) in &state.fields {
        let shard = shard_of(state, k);
        if wanted(&shard) {
            shards
                .entry(shard)
                .or_default()
                .fields
                .insert(k.clone(), fields.clone());
        }
    }
    for (k, life) in &state.lifecycle {
        let shard = shard_of(state, k);
        if wanted(&shard) {
            shards
                .entry(shard)
                .or_default()
                .lifecycle
                .insert(k.clone(), life.clone());
        }
    }
    shards.entry(ShardId::Misc).or_default();
    fs::create_dir_all(shards_dir(sync).join("tasks"))?;
    // A first checkpoint writes every task. Concurrent fsyncs share journal
    // commits, so a few writers turn thousands of barriers into a few seconds.
    let shards: Vec<_> = shards.into_iter().collect();
    let chunk = shards.len().div_ceil(8).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = shards
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || -> Result<()> {
                    for (shard, content) in part {
                        write_json_synced(&shard_path(sync, shard), content)?;
                    }
                    Ok(())
                })
            })
            .collect();
        workers
            .into_iter()
            .try_for_each(|worker| worker.join().expect("shard writer"))
    })?;
    File::open(shards_dir(sync).join("tasks"))?.sync_all()?;
    let heads = segments::heads(sync, &state.compacted)?;
    let mut through = BTreeMap::new();
    for (node, seq) in &heads {
        if *seq == 0 {
            continue;
        }
        through.insert(
            node.clone(),
            Through {
                seq: *seq,
                segment: String::new(),
                offset: 0,
            },
        );
    }
    let head = Head {
        format: STORE_FORMAT,
        through,
        compacted: state.compacted.clone(),
        prune: prune.clone(),
    };
    atomic_write_json(&head_path(sync), &head)?;
    match fs::remove_file(legacy_path(sync)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub(super) fn load_prune(sync: &Path) -> Result<PruneMeta> {
    if let Some(prune) = cached_prune(sync) {
        return Ok(prune);
    }
    if let Some(head) = read_head(sync)? {
        return Ok(head.prune);
    }
    Ok(read_json::<LegacyProjection>(&legacy_path(sync))?
        .map(|p| p.prune)
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::storage::{Storage, atomic_write_text};

    fn task(id: &str, title: &str) -> String {
        format!("---\nid: {id}\ntitle: {title}\nstatus: todo\n---\nbody\n")
    }

    #[test]
    fn delta_walk_reports_sets_changes_and_removals() {
        let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let before = map(&[("a", "1"), ("b", "2"), ("d", "4"), ("f", "6")]);
        let after = map(&[("b", "2"), ("c", "3"), ("d", "40"), ("g", "7")]);
        let out = delta(&before, &after);
        assert_eq!(out.set, map(&[("c", "3"), ("d", "40"), ("g", "7")]));
        assert_eq!(out.remove, vec!["a".to_owned(), "f".to_owned()]);
        let mut applied = before.clone();
        out.apply(&mut applied);
        assert_eq!(applied, after);
        assert!(delta(&after, &after).is_empty());
        assert_eq!(delta(&BTreeMap::new(), &after).set, after);
        assert_eq!(delta(&before, &BTreeMap::new()).remove.len(), before.len());
    }

    fn thread(id: &str, bodies: &[&str]) -> String {
        let mut text = format!("task_id: {id}\nmessages:\n");
        for (index, body) in bodies.iter().enumerate() {
            text.push_str(&format!(
                "- id: MSG-{:03}\n  role: human\n  kind: context\n  body: {body}\n  author: user\n  created_at: '2026-01-01T00:00:0{index}'\n",
                index + 1
            ));
        }
        text
    }

    /// Forces a full projection checkpoint, as the snapshot spool does.
    fn snapshot(journal: &Journal) {
        journal.reconcile().unwrap();
        let _sync = lock_file_reentrant(&journal.root.join(".lock")).unwrap();
        journal.recover().unwrap();
        let (state, prune_meta) = load_state(&journal.root).unwrap();
        checkpoint(&journal.root, &state, &prune_meta, true).unwrap();
    }

    fn store() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().unwrap();
        crate::core::sync::enable_sync(dir.path()).unwrap();
        let board = Storage::new(dir.path().join("projects/demo"));
        board.init_board().unwrap();
        atomic_write_text(
            &dir.path().join("projects/demo/project.yaml"),
            "id: demo\nname: Demo\ncreated_at: '2026-01-01T00:00:00'\n",
        )
        .unwrap();
        (dir, board)
    }

    fn write(board: &Storage, id: &str, title: &str, bodies: &[&str]) {
        atomic_write_text(
            &board.tasks_dir.join(format!("todo/{id}.md")),
            &task(id, title),
        )
        .unwrap();
        atomic_write_text(
            &board.threads_dir.join(format!("{id}.yaml")),
            &thread(id, bodies),
        )
        .unwrap();
    }

    fn json_of<T: Serialize>(value: &T) -> Value {
        serde_json::to_value(value).unwrap()
    }

    /// Every key of `task` in the full state, as the scoped load must see it.
    fn task_keys(state: &JournalState, project: &str, task: &str) -> Vec<String> {
        let shard = ShardId::Task(project.to_owned(), task.to_owned());
        state
            .fields
            .keys()
            .filter(|k| shard_of(state, k) == shard)
            .cloned()
            .collect()
    }

    #[test]
    fn task_scoped_load_matches_the_full_state_for_that_task() {
        let (dir, board) = store();
        let journal = Journal::open(dir.path()).unwrap();
        write(&board, "TASK-001", "one", &["first"]);
        write(&board, "TASK-002", "two", &["second"]);
        snapshot(&journal);
        assert!(head_path(&journal.root).is_file());
        assert!(!legacy_path(&journal.root).exists());
        // Operations after the head are replayed onto the shards.
        write(&board, "TASK-001", "one again", &["first", "reply"]);
        write(&board, "TASK-002", "two again", &["second", "more", "most"]);

        let full = journal.state().unwrap();
        let project = full.project_uids["demo"].clone();
        for display in ["TASK-001", "TASK-002"] {
            // A cold CLI process: no whole-store state in memory.
            forget(&journal.root);
            let scoped = journal.task_state("demo", &[display]).unwrap();
            assert!(scoped.partial);
            let uid = ids::task_uid(&full, &project, display);
            let keys = task_keys(&full, &project, &uid);
            assert!(keys.len() >= 4, "task, thread and messages: {keys:?}");
            for k in &keys {
                assert_eq!(json_of(&scoped.fields[k]), json_of(&full.fields[k]), "{k}");
                assert_eq!(
                    json_of(&scoped.lifecycle.get(k)),
                    json_of(&full.lifecycle.get(k)),
                    "{k}"
                );
            }
            let other = if display == "TASK-001" {
                "TASK-002"
            } else {
                "TASK-001"
            };
            let other = ids::task_uid(&full, &project, other);
            assert!(!scoped.fields.contains_key(&key(&project, "task", &other)));
            assert_eq!(scoped.vector, full.vector);
        }
    }

    #[test]
    fn format_two_projection_is_read_whole_then_sharded() {
        let (dir, board) = store();
        let journal = Journal::open(dir.path()).unwrap();
        write(&board, "TASK-001", "one", &["first"]);
        snapshot(&journal);
        let full = journal.state().unwrap();
        let head = read_head(&journal.root).unwrap().unwrap();
        let legacy = serde_json::json!({
            "format": 2,
            "through": head.through,
            "fields": full.fields,
            "lifecycle": full.lifecycle,
            "compacted": head.compacted,
            "prune": head.prune,
        });
        fs::remove_dir_all(shards_dir(&journal.root)).unwrap();
        fs::write(legacy_path(&journal.root), legacy.to_string()).unwrap();
        forget(&journal.root);

        let scoped = journal.task_state("demo", &["TASK-001"]).unwrap();
        assert!(!scoped.partial, "a format 2 file is the whole state");
        assert_eq!(json_of(&scoped.fields), json_of(&full.fields));

        snapshot(&journal);
        assert!(head_path(&journal.root).is_file());
        assert!(!legacy_path(&journal.root).exists());
        forget(&journal.root);
        assert_eq!(
            json_of(&journal.state().unwrap().fields),
            json_of(&full.fields)
        );
    }

    #[test]
    fn checkpoints_from_task_scoped_writes_keep_every_task() {
        let (dir, board) = store();
        fs::write(
            dir.path().join("config.yaml"),
            "sync:\n  enabled: true\n  journal:\n    snapshot_ops: 1\n",
        )
        .unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        let ids = ["TASK-001", "TASK-002", "TASK-003"];
        for round in 0..3 {
            for id in ids {
                // Each capture starts cold, writes from a scoped state and,
                // with snapshot_ops 1, checkpoints and prunes the solo store.
                forget(&journal.root);
                let bodies: Vec<String> = (0..=round).map(|n| format!("{id} note {n}")).collect();
                let bodies: Vec<&str> = bodies.iter().map(String::as_str).collect();
                write(&board, id, &format!("{id} round {round}"), &bodies);
            }
        }
        forget(&journal.root);
        let state = journal.state().unwrap();
        let project = state.project_uids["demo"].clone();
        assert!(head_path(&journal.root).is_file());
        assert!(
            !state.compacted.is_empty(),
            "prune dropped the checkpointed prefix"
        );
        for id in ids {
            let uid = ids::task_uid(&state, &project, id);
            assert_eq!(
                state.fields[&key(&project, "task", &uid)].values["title"],
                format!("{id} round 2")
            );
            let mut bodies: Vec<String> = state
                .fields
                .values()
                .filter(|f| f.values.get("task_id").and_then(Value::as_str) == Some(uid.as_str()))
                .filter_map(|f| f.values.get("body")?.as_str().map(str::to_owned))
                .collect();
            bodies.sort();
            assert_eq!(
                bodies,
                (0..3).map(|n| format!("{id} note {n}")).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_stale_duplicate_in_another_shard_never_wins() {
        let mut state = JournalState::default();
        let k = key("p", "message", "m");
        let newer = Hlc(2, 0, "n".into());
        let older = Hlc(1, 0, "n".into());
        let field = |value: &str, clock: &Hlc| FieldState {
            values: BTreeMap::from([("body".into(), Value::String(value.into()))]),
            clocks: BTreeMap::from([("body".into(), clock.clone())]),
        };
        for (value, clock) in [("new", &newer), ("old", &older)] {
            merge_shard(
                &mut state,
                Shard {
                    fields: BTreeMap::from([(k.clone(), field(value, clock))]),
                    lifecycle: BTreeMap::from([(
                        k.clone(),
                        Lifecycle {
                            created: Some(clock.clone()),
                            deleted: Some(clock.clone()),
                            ..Lifecycle::default()
                        },
                    )]),
                },
            );
        }
        assert_eq!(state.fields[&k].values["body"], "new");
        assert_eq!(state.lifecycle[&k].created, Some(older));
        assert_eq!(state.lifecycle[&k].deleted, Some(newer));
    }

    /// Store-scale timing, format 2 single file vs shards. Run with
    /// `cargo test --release --lib projection_scale -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn projection_scale() {
        let tasks: usize = std::env::var("BENCH_TASKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1800);
        let messages: usize = std::env::var("BENCH_MESSAGES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(25);
        let dir = tempfile::tempdir_in(std::env::var("TMPDIR").unwrap_or("/tmp".into())).unwrap();
        let sync = dir.path().join("sync");
        fs::create_dir_all(sync.join("meta")).unwrap();
        let mut state = JournalState::default();
        let mut seq = 0u64;
        let mut op = |state: &mut JournalState, entity: &str, id: String, set: Fields| {
            seq += 1;
            let operation = Operation {
                v: OP_FORMAT,
                op: format!("n:{seq}"),
                hlc: Hlc(seq, 0, "n".into()),
                store: "s".into(),
                project: "p".into(),
                entity: entity.into(),
                id,
                set,
                prev: BTreeMap::new(),
                extra: BTreeMap::new(),
                actor: "agent".into(),
            };
            apply_meta(state, &operation);
        };
        let text = |n: usize| Value::String("lorem ipsum dolor sit amet ".repeat(n));
        for t in 0..tasks {
            let uid = format!("task-{t}");
            op(
                &mut state,
                "task",
                uid.clone(),
                BTreeMap::from([
                    ("id".into(), Value::String(format!("TASK-{t}"))),
                    ("title".into(), Value::String(format!("task {t}"))),
                    ("status".into(), Value::String("todo".into())),
                    ("description".into(), text(20)),
                ]),
            );
            for m in 0..messages {
                op(
                    &mut state,
                    "message",
                    format!("{uid}-m{m}"),
                    BTreeMap::from([
                        ("task_id".into(), Value::String(uid.clone())),
                        ("body".into(), text(30)),
                        ("role".into(), Value::String("agent".into())),
                        ("kind".into(), Value::String("context".into())),
                        ("author".into(), Value::String("agent".into())),
                        ("status".into(), Value::String("open".into())),
                    ]),
                );
            }
        }
        state.vector.clear();
        let prune = PruneMeta::default();
        let time = |label: &str, f: &mut dyn FnMut()| {
            let mut runs: Vec<f64> = (0..5)
                .map(|_| {
                    let start = std::time::Instant::now();
                    f();
                    start.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            runs.sort_by(f64::total_cmp);
            println!("{label}: {:.1} ms", runs[2]);
        };
        let legacy = serde_json::json!({
            "format": 2, "fields": state.fields, "lifecycle": state.lifecycle,
        });
        fs::write(legacy_path(&sync), legacy.to_string()).unwrap();
        println!(
            "{tasks} tasks x {messages} messages, projection.json {:.1} MB",
            fs::metadata(legacy_path(&sync)).unwrap().len() as f64 / 1e6
        );
        time("format 2: whole-store load", &mut || {
            load_state_disk(&sync).unwrap();
        });
        time("format 2: checkpoint (whole file)", &mut || {
            atomic_write_json(&legacy_path(&sync), &legacy).unwrap();
        });
        let start = std::time::Instant::now();
        write_checkpoint(&sync, &state, &prune).unwrap();
        println!(
            "first sharding checkpoint: {:.1} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
        let shard = shard_path(&sync, &ShardId::Task("p".into(), "task-7".into()));
        println!(
            "shard files {}, one task shard {:.1} KB, misc {} B",
            fs::read_dir(shards_dir(&sync).join("tasks"))
                .unwrap()
                .count(),
            fs::metadata(&shard).unwrap().len() as f64 / 1e3,
            fs::metadata(shard_path(&sync, &ShardId::Misc))
                .unwrap()
                .len(),
        );
        time("format 3: whole-store load", &mut || {
            load_state_disk(&sync).unwrap();
        });
        time("format 3: one-task load", &mut || {
            let scoped = load_tasks(&sync, |_| ("p".into(), vec!["task-7".into()])).unwrap();
            assert!(scoped.partial && scoped.fields.len() == messages + 1);
        });
        time("format 3: checkpoint with no new ops", &mut || {
            write_checkpoint(&sync, &state, &prune).unwrap();
        });
    }
}
