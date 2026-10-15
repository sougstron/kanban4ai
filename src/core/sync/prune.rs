//! Causal pruning of operations every participant has acknowledged.
//! A participant that has not acked anything contributes a zero vector, so a
//! store never drops history a peer might still need. With no participants
//! (only our own operations, no peers) the whole prefix is stable.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use super::projection::PruneMeta;
use super::*;

const BLOB_GRACE: std::time::Duration = std::time::Duration::from_secs(3600);
const ARCHIVE_KEEP: u64 = 7 * 24 * 3600 * 1000;

impl Journal {
    /// `acks` maps a participant id to the vector that participant has acknowledged.
    /// Returns the retained frontier `H` (sequences at or below `H` may be dropped).
    pub fn prune(&self, acks: &BTreeMap<String, VersionVector>) -> Result<VersionVector> {
        let _guard = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let (mut state, mut meta) = projection::load_state(&self.root)?;
        let h = frontier(self, &state, &mut meta, acks)?;
        state.compacted.clone_from(&h);
        let retained = self.retained_ops(&state)?;
        archive_conflicts(&self.root, &state, &h, &meta.frontier)?;
        for node in segments::node_names(&self.root)? {
            let through = h.get(&node).copied().unwrap_or(0);
            if through > 0 {
                segments::drop_sealed(&self.root, &node, through)?;
            }
        }
        gc_blobs(&self.root, &state, &retained)?;
        projection::write_checkpoint(&self.root, &state, &meta)?;
        let _ = segments::rebuild_index(&self.root);
        Ok(h)
    }

    pub(super) fn retained_ops(&self, state: &JournalState) -> Result<Vec<Operation>> {
        let mut ops = Vec::new();
        for node in segments::node_names(&self.root)? {
            let floor = state.compacted.get(&node).copied().unwrap_or(0);
            ops.extend(segments::read_retained(&self.root, &node, floor)?);
        }
        Ok(ops)
    }
}

fn frontier(
    journal: &Journal,
    state: &JournalState,
    meta: &mut PruneMeta,
    acks: &BTreeMap<String, VersionVector>,
) -> Result<VersionVector> {
    let me = &journal.node.node_id;
    let mut participants = BTreeSet::new();
    participants.extend(acks.keys().cloned());
    for node in state.vector.keys() {
        if node != me {
            participants.insert(node.clone());
        }
    }
    let stable = if participants.is_empty() {
        state.vector.clone()
    } else {
        let vectors: Vec<VersionVector> = participants
            .iter()
            .map(|id| acks.get(id).cloned().unwrap_or_default())
            .chain(std::iter::once(state.vector.clone()))
            .collect();
        let mut names = BTreeSet::new();
        for vector in &vectors {
            names.extend(vector.keys().cloned());
        }
        let mut candidate = VersionVector::new();
        for name in names {
            let value = vectors
                .iter()
                .map(|v| v.get(&name).copied().unwrap_or(0))
                .min()
                .unwrap_or(0);
            if value > 0 {
                candidate.insert(name, value);
            }
        }
        meta.candidate = candidate.clone();
        let mut required = BTreeMap::new();
        for id in &participants {
            required.insert(
                id.clone(),
                acks.get(id).and_then(|v| v.get(id)).copied().unwrap_or(0),
            );
        }
        meta.required = required.clone();
        let ready = participants.iter().all(|id| {
            state.vector.get(id).copied().unwrap_or(0) >= required.get(id).copied().unwrap_or(0)
        });
        if !ready {
            return Ok(state.compacted.clone());
        }
        pointwise_max(&meta.stable, &candidate)
    };
    meta.stable = stable.clone();
    let (h, frontier_hlc) = close(journal, &state.compacted, &stable, &meta.frontier)?;
    meta.frontier = frontier_hlc;
    Ok(h)
}

fn pointwise_max(a: &VersionVector, b: &VersionVector) -> VersionVector {
    let mut out = a.clone();
    for (k, v) in b {
        let slot = out.entry(k.clone()).or_insert(0);
        *slot = (*slot).max(*v);
    }
    out
}

fn close(
    journal: &Journal,
    compacted: &VersionVector,
    stable: &VersionVector,
    old_frontier: &BTreeMap<String, Hlc>,
) -> Result<(VersionVector, BTreeMap<String, Hlc>)> {
    let mut ops: BTreeMap<String, Vec<Operation>> = BTreeMap::new();
    let mut nodes = BTreeSet::new();
    nodes.extend(stable.keys().cloned());
    nodes.extend(compacted.keys().cloned());
    nodes.extend(segments::node_names(&journal.root)?);
    for node in &nodes {
        let floor = compacted.get(node).copied().unwrap_or(0);
        let mut list = segments::read_after(&journal.root, node, floor)?;
        list.retain(|op| {
            sequence(op)
                .ok()
                .is_some_and(|seq| seq <= stable.get(node).copied().unwrap_or(0))
        });
        ops.insert(node.clone(), list);
    }
    let mut h = stable.clone();
    loop {
        let mut changed = false;
        for (node, list) in &ops {
            let limit = h.get(node).copied().unwrap_or(0);
            for op in list {
                let seq = sequence(op)?;
                if seq == 0 || seq > limit {
                    continue;
                }
                if op
                    .prev
                    .values()
                    .any(|prev| !inside(&h, prev, &ops, compacted, old_frontier))
                {
                    h.insert(node.clone(), seq - 1);
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut frontier = old_frontier.clone();
    for (node, list) in &ops {
        let limit = h.get(node).copied().unwrap_or(0);
        if let Some(op) = list
            .iter()
            .rev()
            .find(|op| sequence(op).ok() == Some(limit))
        {
            frontier.insert(node.clone(), op.hlc.clone());
        }
    }
    Ok((h, frontier))
}

fn inside(
    h: &VersionVector,
    prev: &Hlc,
    ops: &BTreeMap<String, Vec<Operation>>,
    compacted: &VersionVector,
    old_frontier: &BTreeMap<String, Hlc>,
) -> bool {
    let bound = h.get(&prev.2).copied().unwrap_or(0);
    if bound == 0 {
        return false;
    }
    if let Some(list) = ops.get(&prev.2)
        && let Some(op) = list.iter().find(|op| sequence(op).ok() == Some(bound))
    {
        return prev <= &op.hlc;
    }
    if compacted.get(&prev.2).copied().unwrap_or(0) >= bound
        && let Some(clock) = old_frontier.get(&prev.2)
    {
        return prev <= clock;
    }
    false
}

fn archive_conflicts(
    sync: &std::path::Path,
    state: &JournalState,
    h: &VersionVector,
    frontier: &BTreeMap<String, Hlc>,
) -> Result<()> {
    let live_path = sync.join("meta/conflicts.jsonl");
    let archive_path = sync.join("meta/conflicts-archive.jsonl");
    let live = read_conflicts(&live_path)?;
    let mut archive = read_conflicts(&archive_path)?;
    let mut ops = Vec::new();
    for node in segments::node_names(sync)? {
        let floor = h.get(&node).copied().unwrap_or(0);
        ops.extend(segments::read_retained(sync, &node, floor)?);
    }
    let fresh = conflicts_of(state, &ops)?;
    let now = now_ms();
    let inside =
        |c: &Conflict| clock_inside(&c.winner, h, frontier) && clock_inside(&c.loser, h, frontier);
    let mut still_live: Vec<_> = fresh.into_iter().filter(|c| !inside(c)).collect();
    for conflict in live {
        if inside(&conflict) {
            archive.push(conflict);
        } else if !still_live.iter().any(|kept| {
            kept.field == conflict.field
                && kept.winner == conflict.winner
                && kept.loser == conflict.loser
                && kept.id == conflict.id
        }) {
            still_live.push(conflict);
        }
    }
    archive.retain(|c| {
        c.winner.0.saturating_add(ARCHIVE_KEEP) >= now
            || !(clock_inside(&c.winner, h, frontier) && clock_inside(&c.loser, h, frontier))
    });
    write_conflicts(&live_path, &still_live)?;
    write_conflicts(&archive_path, &archive)?;
    Ok(())
}

fn clock_inside(clock: &Hlc, h: &VersionVector, frontier: &BTreeMap<String, Hlc>) -> bool {
    if h.get(&clock.2).copied().unwrap_or(0) == 0 {
        return false;
    }
    frontier.get(&clock.2).is_some_and(|bound| clock <= bound)
}

fn read_conflicts(path: &std::path::Path) -> Result<Vec<Conflict>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| invalid(&e.to_string())))
        .collect()
}

fn write_conflicts(path: &std::path::Path, conflicts: &[Conflict]) -> Result<()> {
    if conflicts.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut text = String::new();
        for conflict in conflicts {
            text.push_str(&json(conflict)?);
            text.push('\n');
        }
        atomic_write_text_raw(path, &text)
    }
}

pub(super) fn conflicts_of(state: &JournalState, ops: &[Operation]) -> Result<Vec<Conflict>> {
    let mut result = Vec::new();
    let mut groups: BTreeMap<(&str, &str, &str, &str), Vec<&Operation>> = BTreeMap::new();
    for op in ops {
        for field in op
            .set
            .keys()
            .filter(|field| !matches!(field.as_str(), "draft" | "updated_at"))
        {
            groups
                .entry((&op.project, &op.entity, &op.id, field))
                .or_default()
                .push(op);
        }
    }
    for ((project, entity, id, field), versions) in groups {
        let clocks: BTreeMap<&Hlc, &Operation> = versions.iter().map(|op| (&op.hlc, *op)).collect();
        for (i, a) in versions.iter().enumerate() {
            for b in &versions[i + 1..] {
                let av = &a.set[field];
                let bv = &b.set[field];
                if a.hlc.2 == b.hlc.2
                    || av == bv
                    || ancestor(&clocks, field, &a.hlc, b)
                    || ancestor(&clocks, field, &b.hlc, a)
                {
                    continue;
                }
                let (winner, loser, winner_value, loser_value) = if a.hlc > b.hlc {
                    (*a, *b, av, bv)
                } else {
                    (*b, *a, bv, av)
                };
                result.push(Conflict {
                    project: project.into(),
                    entity: entity.into(),
                    id: id.into(),
                    field: field.into(),
                    winner: winner.hlc.clone(),
                    loser: loser.hlc.clone(),
                    winner_value: winner_value.clone(),
                    loser_value: loser_value.clone(),
                });
            }
        }
    }
    for deletion in ops.iter().filter(|op| op.entity == "tombstone") {
        let entity = deletion
            .set
            .get("entity")
            .and_then(Value::as_str)
            .unwrap_or("");
        push_tombstone(
            deletion.project.as_str(),
            entity,
            &deletion.id,
            &deletion.hlc,
            ops,
            &mut result,
        );
    }
    for (k, life) in &state.lifecycle {
        let Some(deleted) = &life.deleted else {
            continue;
        };
        if life.restored.as_ref().is_some_and(|r| r >= deleted) {
            continue;
        }
        let Ok((project, entity, id)) = serde_json::from_str::<(String, String, String)>(k) else {
            continue;
        };
        push_tombstone(&project, &entity, &id, deleted, ops, &mut result);
    }
    result.sort_by(|a, b| {
        (&a.project, &a.entity, &a.id, &a.field, &a.winner, &a.loser)
            .cmp(&(&b.project, &b.entity, &b.id, &b.field, &b.winner, &b.loser))
    });
    result.dedup();
    Ok(result)
}

fn push_tombstone(
    project: &str,
    entity: &str,
    id: &str,
    deleted: &Hlc,
    ops: &[Operation],
    result: &mut Vec<Conflict>,
) {
    for edit in ops.iter().filter(|op| {
        op.project == project && op.entity == entity && op.id == id && op.hlc > *deleted
    }) {
        for (field, value) in &edit.set {
            result.push(Conflict {
                project: edit.project.clone(),
                entity: entity.into(),
                id: edit.id.clone(),
                field: field.clone(),
                winner: deleted.clone(),
                loser: edit.hlc.clone(),
                winner_value: Value::Null,
                loser_value: value.clone(),
            });
        }
    }
}

fn ancestor(
    versions: &BTreeMap<&Hlc, &Operation>,
    field: &str,
    older: &Hlc,
    newer: &Operation,
) -> bool {
    let mut cursor = newer.prev.get(field);
    let mut seen = BTreeSet::new();
    while let Some(clock) = cursor {
        if clock == older {
            return true;
        }
        if !seen.insert(clock) {
            break;
        }
        cursor = versions.get(clock).and_then(|op| op.prev.get(field));
    }
    false
}

fn gc_blobs(sync: &std::path::Path, state: &JournalState, ops: &[Operation]) -> Result<()> {
    let dir = sync.join("blobs");
    if !dir.is_dir() {
        return Ok(());
    }
    let mut keep = BTreeSet::new();
    if let Ok(text) = fs::read_to_string(sync.join("drift.jsonl")) {
        for line in text.lines() {
            if let Some(hash) = line
                .split('"')
                .find(|p| p.len() == 64 && p.bytes().all(|c| c.is_ascii_hexdigit()))
            {
                keep.insert(hash.to_owned());
            }
        }
    }
    for (k, fields) in &state.fields {
        let Ok((_, entity, _)) = serde_json::from_str::<(String, String, String)>(k) else {
            continue;
        };
        if entity == "file"
            && let Some(hash) = fields.values.get("blob").and_then(Value::as_str)
        {
            keep.insert(hash.to_owned());
        }
    }
    for op in ops {
        if let Some(hash) = op.blob_hash() {
            keep.insert(hash.to_owned());
        }
    }
    if let Ok(text) = fs::read_to_string(sync.join("remote-pending.json")) {
        for token in text.split('"') {
            if token.len() == 64
                && token
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            {
                keep.insert(token.to_owned());
            }
        }
    }
    let now = SystemTime::now();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if keep.contains(name) {
            continue;
        }
        let modified = fs::metadata(&path)?.modified().unwrap_or(UNIX_EPOCH);
        if now.duration_since(modified).unwrap_or_default() < BLOB_GRACE {
            continue;
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Live conflicts plus the archive, for readers that outlive a prune.
pub(super) fn stored_conflicts(sync: &std::path::Path) -> Result<Vec<Conflict>> {
    let mut all = read_conflicts(&sync.join("meta/conflicts.jsonl"))?;
    all.extend(read_conflicts(&sync.join("meta/conflicts-archive.jsonl"))?);
    Ok(all)
}
