//! Periodic projection of field state and commutative per-entity lifecycle.
//! Deleting `projection.json` rebuilds both by replaying the segments.
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectionFile {
    #[serde(default = "super::format::snapshot_format_v1")]
    format: u32,
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

impl Default for ProjectionFile {
    fn default() -> Self {
        Self {
            format: STORE_FORMAT,
            through: BTreeMap::new(),
            fields: BTreeMap::new(),
            lifecycle: BTreeMap::new(),
            compacted: VersionVector::new(),
            prune: PruneMeta::default(),
        }
    }
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
    let mut out = MapDelta::default();
    for (k, v) in after {
        if before.get(k) != Some(v) {
            out.set.insert(k.clone(), v.clone());
        }
    }
    for k in before.keys() {
        if !after.contains_key(k) {
            out.remove.push(k.clone());
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

pub(super) fn uids_delta(before: &Uids, after: &Uids) -> UidsDelta {
    UidsDelta {
        project: delta(&before.project_uids, &after.project_uids),
        message: delta(&before.message_uids, &after.message_uids),
        task: delta(&before.task_uids, &after.task_uids),
        alias: delta(&before.task_aliases, &after.task_aliases),
    }
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

fn projection_path(sync: &Path) -> std::path::PathBuf {
    sync.join("meta/projection.json")
}

fn uids_path(sync: &Path) -> std::path::PathBuf {
    sync.join("meta/uids.json")
}

fn since_path(sync: &Path) -> std::path::PathBuf {
    sync.join("meta/since.json")
}

fn read_projection(sync: &Path) -> Result<Option<ProjectionFile>> {
    match fs::read_to_string(projection_path(sync)) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).map_err(|e| invalid(&e.to_string()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn load_uids(sync: &Path) -> Result<Uids> {
    load(&uids_path(sync))
}

pub(super) fn save_uids(sync: &Path, state: &JournalState) -> Result<()> {
    fs::create_dir_all(sync.join("meta"))?;
    atomic_write_json(&uids_path(sync), &uids_of(state))
}

/// Stream a JSON value to a temp file in the destination directory, then rename.
pub(super) fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
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
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(super) fn load_state(sync: &Path) -> Result<(JournalState, PruneMeta)> {
    let stored = read_projection(sync)?.unwrap_or_default();
    let mut state = JournalState {
        fields: stored.fields,
        lifecycle: stored.lifecycle,
        compacted: stored.compacted.clone(),
        ..JournalState::default()
    };
    apply_uids(&mut state, load_uids(sync)?);
    let mut nodes = segments::node_names(sync)?;
    for node in stored.through.keys() {
        if !nodes.iter().any(|n| n == node) {
            nodes.push(node.clone());
        }
    }
    nodes.sort();
    nodes.dedup();
    for node in &nodes {
        let through = stored.through.get(node).map(|t| t.seq).unwrap_or(0);
        for op in segments::read_after(sync, node, through)? {
            apply_meta(&mut state, &op);
        }
        for op in segments::read_ahead(sync, node)? {
            apply_meta(&mut state, &op);
        }
    }
    state.vector = segments::heads(sync, &stored.compacted)?;
    state.compacted = stored.compacted;
    Ok((state, stored.prune))
}

#[derive(Serialize, Deserialize)]
struct Since {
    ops: u64,
    at_ms: u64,
}

fn read_since(sync: &Path) -> Result<Since> {
    match fs::read_to_string(since_path(sync)) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| invalid(&e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Since {
            ops: 0,
            at_ms: now_ms(),
        }),
        Err(e) => Err(e.into()),
    }
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
    if !due && read_projection(sync)?.is_some() {
        return Ok(false);
    }
    // A first small write still records `since` so the window starts now.
    if !due && read_projection(sync)?.is_none() && since.ops < limits.snapshot_ops && !force {
        if !since_path(sync).exists() {
            fs::create_dir_all(sync.join("meta"))?;
            atomic_write_text_raw(
                &since_path(sync),
                &json(&Since {
                    ops: since.ops,
                    at_ms: now,
                })?,
            )?;
        }
        return Ok(false);
    }
    write_checkpoint(sync, state, prune)?;
    atomic_write_text_raw(&since_path(sync), &json(&Since { ops: 0, at_ms: now })?)?;
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
    fs::create_dir_all(sync.join("meta"))?;
    atomic_write_text_raw(&since_path(sync), &json(&since)?)
}

pub(super) fn write_checkpoint(sync: &Path, state: &JournalState, prune: &PruneMeta) -> Result<()> {
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
    let file = ProjectionFile {
        format: STORE_FORMAT,
        through,
        fields: state.fields.clone(),
        lifecycle: state.lifecycle.clone(),
        compacted: state.compacted.clone(),
        prune: prune.clone(),
    };
    fs::create_dir_all(sync.join("meta"))?;
    atomic_write_json(&projection_path(sync), &file)
}

pub(super) fn load_prune(sync: &Path) -> Result<PruneMeta> {
    Ok(read_projection(sync)?.map(|p| p.prune).unwrap_or_default())
}
