//! Restartable migration from the single-file v1 journal to segmented format 2.
//! `sync/FORMAT` is written only after the new files are durable. A crash before
//! that deletes the partial v2 files and starts again; a crash after it only
//! finishes moving the v1 files aside.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use super::format::{self, JournalLimits, STORE_FORMAT};
use super::projection::{self, PruneMeta};
use super::*;
use crate::core::storage::{BoardGuard, Storage};

#[derive(Clone, Default, Serialize, Deserialize)]
struct V1State {
    #[serde(default)]
    vector: VersionVector,
    #[serde(default)]
    fields: BTreeMap<String, FieldState>,
    #[serde(default)]
    message_uids: BTreeMap<String, String>,
    #[serde(default)]
    project_uids: BTreeMap<String, String>,
    #[serde(default)]
    task_uids: BTreeMap<String, String>,
    #[serde(default)]
    task_aliases: BTreeMap<String, String>,
    #[serde(default)]
    compacted: VersionVector,
    #[serde(default)]
    operations: BTreeMap<String, Operation>,
}

#[derive(Deserialize)]
struct V1Pending {
    path: PathBuf,
    content: Option<String>,
    #[serde(default)]
    external: bool,
    #[serde(default)]
    observed: Option<drift::Entry>,
    #[serde(default)]
    operations: Vec<Operation>,
    state: V1State,
}

#[derive(Default, Deserialize)]
struct V1Remote {
    #[serde(default)]
    boards: Vec<PathBuf>,
    #[serde(default)]
    writes: BTreeMap<PathBuf, Option<String>>,
    #[serde(default)]
    blobs: BTreeMap<PathBuf, String>,
}

pub(super) fn migrate(store: &Path) -> Result<()> {
    let root = store.join("sync");
    if format::read_format(store)?.is_some_and(|v| v >= STORE_FORMAT) {
        return finish_v1_move(store);
    }
    wipe_partial(&root)?;
    let _store = lock_file_reentrant(&store.join(".lock"))?;
    if format::read_format(store)?.is_some_and(|v| v >= STORE_FORMAT) {
        return finish_v1_move(store);
    }
    let mut boards = BTreeSet::new();
    let projects = store.join("projects");
    if projects.is_dir() {
        for entry in fs::read_dir(&projects)? {
            let path = entry?.path();
            if path.is_dir() {
                boards.insert(path);
            }
        }
    }
    let _boards: Vec<BoardGuard> = boards
        .iter()
        .map(|p| Storage::new(p).lock())
        .collect::<Result<_>>()?;
    let _sync = lock_file_reentrant(&root.join(".lock"))?;
    if format::read_format(store)?.is_some_and(|v| v >= STORE_FORMAT) {
        return finish_v1_move(store);
    }
    wipe_partial(&root)?;
    let conflicts = fs::read_to_string(root.join("meta/conflicts.jsonl")).unwrap_or_default();
    finish_v1_remote(&root)?;
    finish_v1_pending(&root)?;
    let v1 = read_v1(&root)?;
    let mut by_node: BTreeMap<String, BTreeMap<u64, Operation>> = BTreeMap::new();
    for op in v1.operations.values() {
        by_node
            .entry(op.hlc.2.clone())
            .or_default()
            .insert(sequence(op)?, op.clone());
    }
    let limits = JournalLimits::load(store)?;
    let staging = root.join(".staging");
    fs::create_dir_all(&staging)?;
    let mut state = JournalState::default();
    for (node, ops) in &by_node {
        let mut ordered: Vec<Operation> = ops.values().cloned().collect();
        ordered.sort_by_key(|op| sequence(op).unwrap_or(0));
        let mut expect = 1u64;
        let mut contiguous = Vec::new();
        let mut ahead = Vec::new();
        for op in &ordered {
            let seq = sequence(op)?;
            if seq == expect {
                contiguous.push(op.clone());
                expect += 1;
            } else if seq > expect {
                ahead.push(op.clone());
            }
        }
        write_segments(&staging.join(node), &contiguous, &limits)?;
        if !ahead.is_empty() {
            fs::create_dir_all(staging.join(node))?;
            let mut text = String::new();
            for op in &ahead {
                text.push_str(&json(op)?);
                text.push('\n');
            }
            atomic_write_text_raw(&staging.join(node).join("ahead.jsonl"), &text)?;
        }
        for op in &ordered {
            projection::apply_meta(&mut state, op);
        }
        let _ = node;
    }
    state.vector = v1.vector;
    state.compacted = v1.compacted;
    state.project_uids = v1.project_uids;
    state.message_uids = v1.message_uids;
    state.task_uids = v1.task_uids;
    state.task_aliases = v1.task_aliases;
    // Fields and lifecycle come from the replay above; uid maps are local.
    let mut through = BTreeMap::new();
    for (node, ops) in &by_node {
        let mut expect = 0u64;
        let mut seqs: Vec<u64> = ops.keys().copied().collect();
        seqs.sort_unstable();
        for seq in seqs {
            if seq == expect + 1 {
                expect = seq;
            } else if seq > expect + 1 {
                break;
            }
        }
        if expect > 0 {
            through.insert(node.clone(), expect);
        }
    }
    write_projection(&root, &state, &through)?;
    projection::save_uids(&root, &state)?;
    if !conflicts.is_empty() {
        atomic_write_text_raw(&root.join("meta/conflicts.jsonl"), &conflicts)?;
    }
    format::write_format(store, STORE_FORMAT)?;
    finish_v1_move_locked(store)?;
    let _ = segments::rebuild_index(&root);
    Ok(())
}

fn read_v1(root: &Path) -> Result<V1State> {
    let mut state: V1State = match fs::read_to_string(root.join("meta/state.json")) {
        Ok(text) if !format::is_stub(&text) => {
            serde_json::from_str(&text).map_err(|e| invalid(&e.to_string()))?
        }
        _ => V1State::default(),
    };
    for path in format::flat_journal_files(root)? {
        let node = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let op: Operation = match serde_json::from_str(&line) {
                Ok(op) => op,
                Err(_) => continue,
            };
            let _ = node;
            state.operations.entry(op.op.clone()).or_insert(op);
        }
    }
    Ok(state)
}

fn write_segments(dir: &Path, ops: &[Operation], limits: &JournalLimits) -> Result<()> {
    if ops.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let mut index = 0;
    while index < ops.len() {
        let first = sequence(&ops[index])?;
        let path = dir.join(segments::segment_name(first));
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut size = file.metadata()?.len();
        let mut text = String::new();
        while index < ops.len() {
            let line = format!("{}\n", json(&ops[index])?);
            if size > 0 && size + line.len() as u64 > limits.segment_bytes && !text.is_empty() {
                break;
            }
            if size > 0 && text.is_empty() && size >= limits.segment_bytes {
                break;
            }
            text.push_str(&line);
            size += line.len() as u64;
            index += 1;
            if size >= limits.segment_bytes {
                break;
            }
        }
        if !text.is_empty() {
            file.write_all(text.as_bytes())?;
            file.sync_data()?;
        }
    }
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn write_projection(
    root: &Path,
    state: &JournalState,
    through: &BTreeMap<String, u64>,
) -> Result<()> {
    #[derive(Serialize)]
    struct Through {
        seq: u64,
        segment: String,
        offset: u64,
    }
    #[derive(Serialize)]
    struct File<'a> {
        format: u32,
        through: BTreeMap<String, Through>,
        fields: &'a BTreeMap<String, FieldState>,
        lifecycle: &'a BTreeMap<String, projection::Lifecycle>,
        compacted: &'a VersionVector,
        prune: PruneMeta,
    }
    let through = through
        .iter()
        .map(|(node, seq)| {
            (
                node.clone(),
                Through {
                    seq: *seq,
                    segment: String::new(),
                    offset: 0,
                },
            )
        })
        .collect();
    fs::create_dir_all(root.join("meta"))?;
    projection::atomic_write_json(
        &root.join("meta/projection.json"),
        &File {
            format: STORE_FORMAT,
            through,
            fields: &state.fields,
            lifecycle: &state.lifecycle,
            compacted: &state.compacted,
            prune: PruneMeta::default(),
        },
    )
}

fn wipe_partial(root: &Path) -> Result<()> {
    if root.join(".staging").exists() {
        fs::remove_dir_all(root.join(".staging"))?;
    }
    let journal = root.join("journal");
    if journal.is_dir() {
        for entry in fs::read_dir(&journal)? {
            let path = entry?.path();
            if path.is_dir() {
                fs::remove_dir_all(path)?;
            }
        }
    }
    for name in ["projection.json", "uids.json", "index.json", "since.json"] {
        match fs::remove_file(root.join("meta").join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub(super) fn finish_v1_move(store: &Path) -> Result<()> {
    let root = store.join("sync");
    let _store = lock_file_reentrant(&store.join(".lock"))?;
    let _sync = lock_file_reentrant(&root.join(".lock"))?;
    finish_v1_move_locked(store)
}

fn finish_v1_move_locked(store: &Path) -> Result<()> {
    let root = store.join("sync");
    let legacy = root.join("legacy-v1");
    fs::create_dir_all(legacy.join("journal"))?;
    for path in format::flat_journal_files(&root)? {
        let name = path.file_name().expect("journal name");
        move_aside(&path, &legacy.join("journal").join(name))?;
    }
    let staging = root.join(".staging");
    if staging.is_dir() {
        fs::create_dir_all(root.join("journal"))?;
        for entry in fs::read_dir(&staging)? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().expect("node").to_owned();
            let dest = root.join("journal").join(&name);
            if dest.exists() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::rename(&path, &dest)?;
            }
        }
        let _ = fs::remove_dir_all(&staging);
    }
    if root.join("snapshot").exists() {
        move_aside(&root.join("snapshot"), &legacy.join("snapshot"))?;
    }
    let state_path = root.join("meta/state.json");
    if state_path.is_file() {
        let text = fs::read_to_string(&state_path)?;
        if !format::is_stub(&text) {
            move_aside(&state_path, &legacy.join("state.json"))?;
        }
    }
    format::write_stub(&root)?;
    fs::File::open(&root)?.sync_all()?;
    Ok(())
}

fn move_aside(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if to.exists() {
        if from.is_dir() {
            fs::remove_dir_all(from)?;
        } else {
            fs::remove_file(from)?;
        }
    } else {
        fs::rename(from, to)?;
    }
    Ok(())
}

fn journal_of(root: &Path) -> Result<Journal> {
    let node = serde_yaml_ng::from_str(&fs::read_to_string(root.join("node.yaml"))?)?;
    Ok(Journal {
        root: root.to_path_buf(),
        node,
    })
}

fn finish_v1_remote(root: &Path) -> Result<()> {
    let path = root.join("remote-pending.json");
    if !path.is_file() {
        return Ok(());
    }
    let pending: V1Remote =
        serde_json::from_str(&fs::read_to_string(&path)?).map_err(|e| invalid(&e.to_string()))?;
    let _boards = pending
        .boards
        .iter()
        .map(|board| crate::core::storage::Storage::new(board).lock())
        .collect::<Result<Vec<_>>>()?;
    let journal = journal_of(root)?;
    for (blob_path, hash) in &pending.blobs {
        transport::write_bytes(blob_path, &journal.blob(hash)?)?;
        drift::record(&journal, blob_path)?;
    }
    for (file, content) in &pending.writes {
        if let Some(text) = content {
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent)?;
            }
            atomic_write_text_raw(file, text)?;
        } else {
            match fs::remove_file(file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        drift::record(&journal, file)?;
    }
    fs::remove_file(path)?;
    Ok(())
}

fn finish_v1_pending(root: &Path) -> Result<()> {
    let path = root.join("pending.json");
    if !path.is_file() {
        return Ok(());
    }
    let pending: V1Pending =
        serde_json::from_str(&fs::read_to_string(&path)?).map_err(|e| invalid(&e.to_string()))?;
    let journal = journal_of(root)?;
    if !pending.external {
        match &pending.content {
            Some(text) => atomic_write_text_raw(&pending.path, text)?,
            None => match fs::remove_file(&pending.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
        }
    }
    fs::create_dir_all(root.join("journal"))?;
    let log = root
        .join("journal")
        .join(format!("{}.jsonl", journal.node.node_id));
    drift::repair_tail(&log)?;
    let mut seen = BTreeSet::new();
    if log.exists() {
        for line in BufReader::new(fs::File::open(&log)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(op) = serde_json::from_str::<Operation>(&line)
                && pending.operations.iter().any(|p| p.op == op.op)
            {
                seen.insert(op.op);
            }
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&log)?;
    for op in &pending.operations {
        if !seen.contains(&op.op) {
            writeln!(file, "{}", json(op)?)?;
        }
    }
    file.sync_all()?;
    fs::create_dir_all(root.join("meta"))?;
    atomic_write_text_raw(&root.join("meta/state.json"), &json(&pending.state)?)?;
    for op in pending
        .operations
        .iter()
        .filter(|op| op.entity == "run" && !op.set.contains_key("command"))
    {
        let Some(fields) = pending.state.fields.get(&key(&op.project, "run", &op.id)) else {
            continue;
        };
        let (path, text) = run::projection(root, &op.project, &op.id, fields)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_text_raw(&path, &text)?;
    }
    if pending.external {
        drift::record_observed(&journal, &pending.path, pending.observed.as_ref())?;
    } else {
        drift::record(&journal, &pending.path)?;
    }
    fs::remove_file(path)?;
    Ok(())
}
