//! Causal pruning of operations every participant has acknowledged.
//! A participant that has not acked anything contributes a zero vector, so a
//! store never drops history a peer might still need. With no participants
//! (only our own operations, no peers) the whole prefix is stable.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use super::projection::PruneMeta;
use super::*;

const BLOB_GRACE: std::time::Duration = std::time::Duration::from_secs(3600);
const ARCHIVE_KEEP: u64 = 7 * 24 * 3600 * 1000;

impl Journal {
    /// `acks` maps a participant id to the vector that participant has acknowledged.
    /// Returns the retained frontier `H` (sequences at or below `H` may be dropped).
    pub fn prune(&self, acks: &BTreeMap<String, VersionVector>) -> Result<VersionVector> {
        forget_groups(&self.root);
        let _guard = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let (mut state, mut meta) = projection::load_state(&self.root)?;
        let h = frontier(self, &state, &mut meta, acks)?;
        if h == state.compacted {
            // Nothing became prunable: conflicts and segments are as the last
            // run left them. Re-deriving them read and parsed every retained
            // operation each maintenance tick. Only a blob nothing ever
            // referenced can have become collectable.
            gc_blobs(&self.root, &state, || self.retained_ops(&state))?;
            projection::remember(&self.root, state, &meta);
            return Ok(h);
        }
        state.compacted.clone_from(&h);
        archive_conflicts(&self.root, &state, &h, &meta.frontier)?;
        // The checkpoint finds its dirty shards in the operations after the
        // old head, so it is written before any of them are dropped.
        projection::write_checkpoint(&self.root, &state, &meta)?;
        for node in segments::node_names(&self.root)? {
            let through = h.get(&node).copied().unwrap_or(0);
            if through > 0 {
                segments::drop_sealed(&self.root, &node, through)?;
            }
        }
        gc_blobs(&self.root, &state, || self.retained_ops(&state))?;
        let _ = segments::rebuild_index(&self.root);
        projection::remember(&self.root, state, &meta);
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
    // Participants are active hub clients or the client's single server receipt.
    // Without any receipt, foreign origins hold their history: a store that
    // has not reached a server yet must not simplify what it never forwarded.
    let mut participants: BTreeSet<String> = acks.keys().cloned().collect();
    if participants.is_empty() {
        let me = &journal.node.node_id;
        participants.extend(state.vector.keys().filter(|node| *node != me).cloned());
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
    if stable == state.compacted {
        // `close` only lowers `stable`, and nothing above the floor is left
        // to check: the frontier stays where it is.
        return Ok(stable);
    }
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
    // Sequences parsed once, and each node's clock by sequence for `inside`:
    // a linear search per `prev` made the fixpoint quadratic in the retained
    // history (36 s for the server's 90k operations).
    let mut seqs: BTreeMap<&str, Vec<(u64, &Operation)>> = BTreeMap::new();
    let mut clocks: HashMap<&str, HashMap<u64, &Hlc>> = HashMap::new();
    for (node, list) in &ops {
        let mut numbered = Vec::with_capacity(list.len());
        let by_seq = clocks.entry(node.as_str()).or_default();
        for op in list {
            let seq = sequence(op)?;
            by_seq.entry(seq).or_insert(&op.hlc);
            numbered.push((seq, op));
        }
        seqs.insert(node.as_str(), numbered);
    }
    let mut h = stable.clone();
    loop {
        let mut changed = false;
        for (node, list) in &seqs {
            let limit = h.get(*node).copied().unwrap_or(0);
            for (seq, op) in list {
                if *seq == 0 || *seq > limit {
                    continue;
                }
                if op
                    .prev
                    .values()
                    .any(|prev| !inside(&h, prev, &clocks, compacted, old_frontier))
                {
                    h.insert((*node).to_owned(), seq - 1);
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
    for (node, list) in &seqs {
        let limit = h.get(*node).copied().unwrap_or(0);
        if let Some((_, op)) = list.iter().rev().find(|(seq, _)| *seq == limit) {
            frontier.insert((*node).to_owned(), op.hlc.clone());
        }
    }
    Ok((h, frontier))
}

fn inside(
    h: &VersionVector,
    prev: &Hlc,
    clocks: &HashMap<&str, HashMap<u64, &Hlc>>,
    compacted: &VersionVector,
    old_frontier: &BTreeMap<String, Hlc>,
) -> bool {
    let bound = h.get(&prev.2).copied().unwrap_or(0);
    if bound == 0 {
        return false;
    }
    if let Some(clock) = clocks
        .get(prev.2.as_str())
        .and_then(|by_seq| by_seq.get(&bound))
    {
        return prev <= *clock;
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

/// Versions of each replicated field. An operation is shared by every field it
/// sets: a full copy per field made the server's group cache gigabytes.
pub(super) type FieldGroups = BTreeMap<(String, String, String, String), Vec<Arc<Operation>>>;

pub(super) struct GroupsSlot {
    token: u64,
    pub groups: FieldGroups,
    pub ops: Vec<Operation>,
    pub conflicts: Vec<Conflict>,
}

static GROUPS: LazyLock<Mutex<HashMap<PathBuf, GroupsSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn groups_cache() -> std::sync::MutexGuard<'static, HashMap<PathBuf, GroupsSlot>> {
    GROUPS.lock().unwrap_or_else(|err| err.into_inner())
}

pub(super) fn forget_groups(sync: &std::path::Path) {
    groups_cache().remove(sync);
}

pub(super) fn take_groups(sync: &std::path::Path) -> Option<GroupsSlot> {
    let parent = sync.parent()?;
    let token = segments::change_token(parent)?;
    let mut cache = groups_cache();
    let fresh = cache.get(sync).is_some_and(|slot| slot.token == token);
    if !fresh {
        cache.remove(sync);
        return None;
    }
    cache.remove(sync)
}

pub(super) fn store_groups(
    sync: &std::path::Path,
    groups: FieldGroups,
    ops: Vec<Operation>,
    conflicts: Vec<Conflict>,
) {
    let Some(parent) = sync.parent() else {
        return;
    };
    let Some(token) = segments::change_token(parent) else {
        return;
    };
    let mut cache = groups_cache();
    if cache.len() > 32 {
        cache.clear();
    }
    cache.insert(
        sync.to_path_buf(),
        GroupsSlot {
            token,
            groups,
            ops,
            conflicts,
        },
    );
}

pub(super) fn group_ops(ops: &[Operation]) -> FieldGroups {
    let mut groups = FieldGroups::new();
    for op in ops {
        push_group(&mut groups, &Arc::new(op.clone()));
    }
    groups
}

fn push_group(groups: &mut FieldGroups, op: &Arc<Operation>) {
    for field in op
        .set
        .keys()
        .filter(|field| !matches!(field.as_str(), "draft" | "updated_at"))
    {
        groups
            .entry((
                op.project.clone(),
                op.entity.clone(),
                op.id.clone(),
                field.clone(),
            ))
            .or_default()
            .push(Arc::clone(op));
    }
}

fn push_pair(
    result: &mut Vec<Conflict>,
    entity: (&str, &str, &str, &str),
    lineage: &Lineage<'_>,
    a: &Operation,
    b: &Operation,
) {
    let (project, kind, id, field) = entity;
    let (Some(av), Some(bv)) = (a.set.get(field), b.set.get(field)) else {
        return;
    };
    if a.hlc.2 == b.hlc.2
        || av == bv
        || lineage.ancestor(&a.hlc, &b.hlc)
        || lineage.ancestor(&b.hlc, &a.hlc)
    {
        return;
    }
    let (winner, loser, winner_value, loser_value) = if a.hlc > b.hlc {
        (a, b, av, bv)
    } else {
        (b, a, bv, av)
    };
    if covered(field, &loser.hlc, winner) {
        return;
    }
    result.push(Conflict {
        project: project.into(),
        entity: kind.into(),
        id: id.into(),
        field: field.into(),
        winner: winner.hlc.clone(),
        loser: loser.hlc.clone(),
        winner_value: winner_value.clone(),
        loser_value: loser_value.clone(),
    });
}

fn live_deletion(state: &JournalState, op: &Operation) -> bool {
    let Some(life) = state
        .lifecycle
        .get(&key(&op.project, real_entity_name(op), &op.id))
    else {
        return false;
    };
    life.deleted.as_ref().is_some_and(|deleted| {
        life.restored
            .as_ref()
            .is_none_or(|restored| deleted > restored)
    })
}

fn real_entity_name(op: &Operation) -> &str {
    projection::real_entity(op)
}

/// Pairs that involve `new_ops`, unioned with `prior`. A tombstone or a live
/// deletion recomputes from the retained set; delivery order stays irrelevant.
pub(super) fn ingest(
    state: &JournalState,
    groups: &mut FieldGroups,
    ops: &mut Vec<Operation>,
    prior: &[Conflict],
    new_ops: &[Operation],
) -> Result<Vec<Conflict>> {
    let known: HashSet<&str> = ops.iter().map(|old| old.op.as_str()).collect();
    let fresh: Vec<&Operation> = new_ops
        .iter()
        .filter(|op| !known.contains(op.op.as_str()))
        .collect();
    drop(known);
    if fresh.is_empty() {
        return Ok(prior.to_vec());
    }
    let rebuild = fresh.iter().any(|op| {
        matches!(op.entity.as_str(), "tombstone" | "restore") || live_deletion(state, op)
    }) || ops.iter().any(|op| {
        matches!(op.entity.as_str(), "tombstone" | "restore") || live_deletion(state, op)
    });
    if rebuild {
        ops.extend(fresh.into_iter().cloned());
        *groups = group_ops(ops);
        return conflicts_of(state, ops);
    }
    let mut by_field: BTreeMap<(String, String, String, String), Vec<&Operation>> = BTreeMap::new();
    for op in &fresh {
        for field in op
            .set
            .keys()
            .filter(|field| !matches!(field.as_str(), "draft" | "updated_at"))
        {
            by_field
                .entry((
                    op.project.clone(),
                    op.entity.clone(),
                    op.id.clone(),
                    field.clone(),
                ))
                .or_default()
                .push(*op);
        }
    }
    let mut result = Vec::new();
    for (key, fresh_ops) in &by_field {
        let existing = groups.get(key).map(Vec::as_slice).unwrap_or(&[]);
        // The same node cannot conflict with itself. Skip the pairwise walk
        // when this field has no other node and no conflict to revalidate.
        let node = fresh_ops.first().map(|op| op.hlc.2.as_str());
        if node.is_some_and(|node| {
            fresh_ops.iter().all(|op| op.hlc.2 == node)
                && existing.iter().all(|op| op.hlc.2 == node)
                && prior.iter().all(|conflict| {
                    (
                        &conflict.project,
                        &conflict.entity,
                        &conflict.id,
                        &conflict.field,
                    ) != (&key.0, &key.1, &key.2, &key.3)
                })
        }) {
            continue;
        }
        let mut versions: Vec<&Operation> = existing.iter().map(Arc::as_ref).collect();
        versions.extend(fresh_ops.iter().copied());
        let lineage = Lineage::new(&key.3, &versions);
        for conflict in prior {
            if (
                &conflict.project,
                &conflict.entity,
                &conflict.id,
                &conflict.field,
            ) != (&key.0, &key.1, &key.2, &key.3)
            {
                continue;
            }
            let Some(winner) = lineage.op(&conflict.winner) else {
                continue;
            };
            let Some(loser) = lineage.op(&conflict.loser) else {
                continue;
            };
            push_pair(
                &mut result,
                (&key.0, &key.1, &key.2, &key.3),
                &lineage,
                winner,
                loser,
            );
        }
        for new_op in fresh_ops {
            for old in existing {
                push_pair(
                    &mut result,
                    (&key.0, &key.1, &key.2, &key.3),
                    &lineage,
                    old,
                    new_op,
                );
            }
        }
        for (i, a) in fresh_ops.iter().enumerate() {
            for b in &fresh_ops[i + 1..] {
                push_pair(
                    &mut result,
                    (&key.0, &key.1, &key.2, &key.3),
                    &lineage,
                    a,
                    b,
                );
            }
        }
    }
    for conflict in prior {
        let key = (
            conflict.project.clone(),
            conflict.entity.clone(),
            conflict.id.clone(),
            conflict.field.clone(),
        );
        if !by_field.contains_key(&key) {
            result.push(conflict.clone());
        }
    }
    for op in &fresh {
        push_group(groups, &Arc::new((*op).clone()));
    }
    ops.extend(fresh.into_iter().cloned());
    result.sort_by(|a, b| {
        (&a.project, &a.entity, &a.id, &a.field, &a.winner, &a.loser)
            .cmp(&(&b.project, &b.entity, &b.id, &b.field, &b.winner, &b.loser))
    });
    result.dedup();
    Ok(result)
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
        let lineage = Lineage::new(field, &versions);
        for (a, b) in lineage.concurrent() {
            push_pair(&mut result, (project, entity, id, field), &lineage, a, b);
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

/// Ancestry of one field as entry/exit intervals of a depth-first walk over
/// the `prev` forest. "Is `older` an ancestor of `newer`" is then O(1) instead
/// of a chain walk per pair, and only the concurrent pairs are visited. A
/// `prev` outside the group (pruned or not yet delivered) starts a new root,
/// exactly where a chain walk would stop.
struct Lineage<'a> {
    span: BTreeMap<&'a Hlc, (usize, usize)>,
    order: Vec<&'a Operation>,
}

impl<'a> Lineage<'a> {
    fn new(field: &str, versions: &[&'a Operation]) -> Self {
        let by_clock: BTreeMap<&'a Hlc, &'a Operation> =
            versions.iter().map(|op| (&op.hlc, *op)).collect();
        let mut children: BTreeMap<&'a Hlc, Vec<&'a Hlc>> = BTreeMap::new();
        let mut roots = Vec::new();
        for (clock, op) in &by_clock {
            match op
                .prev
                .get(field)
                .and_then(|parent| by_clock.get_key_value(parent))
                .filter(|(parent, _)| parent != &clock)
            {
                Some((parent, _)) => children.entry(*parent).or_default().push(*clock),
                None => roots.push(*clock),
            }
        }
        let mut span = BTreeMap::new();
        let mut order = Vec::with_capacity(by_clock.len());
        // Roots first; a malformed `prev` cycle has none, so any clock left
        // unvisited starts its own walk.
        for start in roots.into_iter().chain(by_clock.keys().copied()) {
            if span.contains_key(start) {
                continue;
            }
            span.insert(start, (order.len(), order.len()));
            order.push(by_clock[start]);
            let mut stack = vec![(start, 0usize)];
            while let Some(&(clock, next)) = stack.last() {
                let child = children.get(clock).and_then(|kids| kids.get(next));
                if let Some(child) = child {
                    if let Some(top) = stack.last_mut() {
                        top.1 += 1;
                    }
                    if !span.contains_key(child) {
                        span.insert(*child, (order.len(), order.len()));
                        order.push(by_clock[child]);
                        stack.push((*child, 0));
                    }
                } else {
                    stack.pop();
                    if let Some(entry) = span.get_mut(clock) {
                        entry.1 = order.len() - 1;
                    }
                }
            }
        }
        Self { span, order }
    }

    fn op(&self, clock: &Hlc) -> Option<&'a Operation> {
        self.span.get(clock).map(|(entry, _)| self.order[*entry])
    }

    fn ancestor(&self, older: &Hlc, newer: &Hlc) -> bool {
        match (self.span.get(older), self.span.get(newer)) {
            (Some(a), Some(b)) => a.0 < b.0 && b.1 <= a.1,
            _ => false,
        }
    }

    /// Every pair where neither is an ancestor of the other, once each: a
    /// later entry outside a version's subtree is neither its descendant nor
    /// (entering later) its ancestor.
    fn concurrent(&self) -> impl Iterator<Item = (&'a Operation, &'a Operation)> + '_ {
        self.span.values().flat_map(|&(entry, exit)| {
            let a = self.order[entry];
            self.order[exit + 1..].iter().map(move |b| (a, *b))
        })
    }
}

/// The loser already lost to the version the winner was written on top of:
/// that conflict was reported once, and the later edit superseded it.
/// Re-pairing would post a fresh note for every causal edit. Clocks grow
/// along a field chain, so the direct predecessor decides.
fn covered(field: &str, loser: &Hlc, winner: &Operation) -> bool {
    winner.prev.get(field).is_some_and(|prev| prev > loser)
}

/// Removes blobs past the grace period that nothing references. `retained`
/// is read only when such a candidate is left after the cheap references
/// (drift, current values, pending remote ops): parsing the whole retained
/// history every maintenance tick was most of the server's cost.
fn gc_blobs(
    sync: &std::path::Path,
    state: &JournalState,
    retained: impl FnOnce() -> Result<Vec<Operation>>,
) -> Result<()> {
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
    let mut candidates = Vec::new();
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
        candidates.push(path);
    }
    if candidates.is_empty() {
        return Ok(());
    }
    for op in retained()? {
        if let Some(hash) = op.blob_hash() {
            keep.insert(hash.to_owned());
        }
    }
    for path in candidates {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| !keep.contains(name))
        {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Live conflicts plus the archive, for readers that outlive a prune.
pub(super) fn stored_conflicts(sync: &std::path::Path) -> Result<Vec<Conflict>> {
    let mut all = read_conflicts(&sync.join("meta/conflicts.jsonl"))?;
    all.extend(read_conflicts(&sync.join("meta/conflicts-archive.jsonl"))?);
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(wall: u64, node: &str, prev: Option<(u64, &str)>) -> Operation {
        Operation {
            v: 1,
            op: format!("{node}:{wall}"),
            hlc: Hlc(wall, 0, node.into()),
            store: String::new(),
            project: "p".into(),
            entity: "task".into(),
            id: "t".into(),
            set: BTreeMap::from([("title".into(), Value::from(wall))]),
            prev: prev
                .map(|(wall, node)| BTreeMap::from([("title".into(), Hlc(wall, 0, node.into()))]))
                .unwrap_or_default(),
            actor: "human".into(),
            extra: BTreeMap::new(),
        }
    }

    fn walk(versions: &[&Operation], older: &Hlc, newer: &Operation) -> bool {
        let mut cursor = newer.prev.get("title");
        while let Some(clock) = cursor {
            if clock == older {
                return true;
            }
            cursor = versions
                .iter()
                .find(|op| &op.hlc == clock)
                .and_then(|op| op.prev.get("title"));
        }
        false
    }

    #[test]
    fn lineage_matches_a_chain_walk_on_a_branched_forest() {
        let ops = [
            version(1, "a", None),
            version(2, "a", Some((1, "a"))),
            version(3, "b", Some((1, "a"))),
            version(4, "a", Some((2, "a"))),
            version(5, "c", Some((3, "b"))),
            version(6, "c", Some((99, "gone"))),
            version(7, "b", Some((6, "c"))),
        ];
        let versions: Vec<&Operation> = ops.iter().collect();
        let lineage = Lineage::new("title", &versions);
        let mut expected = BTreeSet::new();
        for a in &versions {
            for b in &versions {
                assert_eq!(
                    lineage.ancestor(&a.hlc, &b.hlc),
                    walk(&versions, &a.hlc, b),
                    "{:?} -> {:?}",
                    a.hlc,
                    b.hlc
                );
                if a.hlc < b.hlc && !walk(&versions, &a.hlc, b) && !walk(&versions, &b.hlc, a) {
                    expected.insert((a.hlc.clone(), b.hlc.clone()));
                }
            }
        }
        let found: BTreeSet<(Hlc, Hlc)> = lineage
            .concurrent()
            .map(|(a, b)| {
                let (a, b) = (a.hlc.clone(), b.hlc.clone());
                if a < b { (a, b) } else { (b, a) }
            })
            .collect();
        assert_eq!(found, expected);
        assert_eq!(lineage.concurrent().count(), expected.len());
    }
}
