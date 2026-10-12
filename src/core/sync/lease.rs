//! Immutable ID grants and explicit authority transfers. Consumption is
//! machine-local and durable before returning an ID.
use super::*;
use crate::core::storage::Storage;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdLease {
    pub project: String,
    pub node: String,
    pub from: u64,
    pub to: u64,
    #[serde(default)]
    pub reserve: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Allocation {
    used: BTreeMap<String, u64>,
    confirmed: BTreeSet<String>,
}

fn number(id: &str) -> Option<u64> {
    id.strip_prefix("TASK-")?.parse().ok()
}

pub(super) fn decode(op: &Operation) -> Result<IdLease> {
    let node = op
        .set
        .get("node")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("invalid lease node"))?;
    if node.is_empty() || node.contains(['/', '\\', '\0']) {
        return Err(invalid("invalid lease node"));
    }
    let from = op
        .set
        .get("from")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("invalid lease start"))?;
    let to = op
        .set
        .get("to")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("invalid lease end"))?;
    if from == 0
        || to < from
        || to == u64::MAX
        || op.id != op.op
        || !op.prev.is_empty()
        || op.set.len() != if op.set.contains_key("reserve") { 4 } else { 3 }
        || op.project.is_empty()
    {
        return Err(invalid("invalid immutable ID lease"));
    }
    Ok(IdLease {
        project: op.project.clone(),
        node: node.into(),
        from,
        to,
        reserve: match op.set.get("reserve") {
            Some(value) => value
                .as_bool()
                .ok_or_else(|| invalid("invalid lease reserve"))?,
            None => false,
        },
    })
}

pub(super) fn validate_state(state: &JournalState) -> Result<()> {
    let projects: BTreeSet<_> = state
        .operations
        .values()
        .filter(|op| op.entity == "lease")
        .map(|op| op.project.as_str())
        .collect();
    for project in projects {
        authority(state, project)?;
        let grants = grants(state, project)?;
        for pair in grants.windows(2) {
            if pair[0].1.to >= pair[1].1.from {
                return Err(invalid("SYNC_LEASE_CONFLICT"));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_operation(op: &Operation) -> Result<()> {
    if !op.set.contains_key("issuer") {
        decode(op)?;
        return Ok(());
    }
    if op.id != op.op
        || !op.prev.is_empty()
        || op.set.len() != 3
        || op.project.is_empty()
        || op.set.get("from_issuer").and_then(Value::as_str) != Some(&op.hlc.2)
        || op
            .set
            .get("epoch")
            .and_then(Value::as_u64)
            .is_none_or(|epoch| epoch == 0)
        || op
            .set
            .get("issuer")
            .and_then(Value::as_str)
            .is_none_or(|id| id.is_empty() || id == op.hlc.2 || id.contains(['/', '\\', '\0']))
    {
        return Err(invalid("invalid lease authority transfer"));
    }
    Ok(())
}

pub(super) fn authority(state: &JournalState, project: &str) -> Result<Option<(String, u64)>> {
    let mut operations: Vec<_> = state
        .operations
        .values()
        .filter(|op| op.entity == "lease" && op.project == project)
        .collect();
    operations.sort_by(|a, b| a.hlc.cmp(&b.hlc));
    let mut current: Option<(String, u64)> = None;
    for op in operations {
        validate_operation(op)?;
        let (issuer, epoch) = current.get_or_insert_with(|| (op.hlc.2.clone(), 0));
        if *issuer != op.hlc.2 {
            return Err(invalid("SYNC_LEASE_CONFLICT"));
        }
        if let Some(next) = op.set.get("issuer").and_then(Value::as_str) {
            if op.set["epoch"].as_u64() != epoch.checked_add(1) {
                return Err(invalid("SYNC_LEASE_CONFLICT"));
            }
            *issuer = next.into();
            *epoch += 1;
        }
    }
    Ok(current)
}

pub(super) fn grants<'a>(
    state: &'a JournalState,
    project: &str,
) -> Result<Vec<(&'a Operation, IdLease)>> {
    let mut result = state
        .operations
        .values()
        .filter(|op| {
            op.entity == "lease" && op.project == project && !op.set.contains_key("issuer")
        })
        .map(|op| Ok((op, decode(op)?)))
        .collect::<Result<Vec<_>>>()?;
    result.sort_by_key(|(_, lease)| lease.from);
    Ok(result)
}

/// Before the first grant, prefer the paired server. Unpaired stores retain
/// legacy allocation; node_id breaks ties between equal roles.
pub(super) fn fallback_issuer(store: &Path, node: &NodeIdentity) -> Result<bool> {
    let config: Value = match fs::read_to_string(store.join("config.yaml")) {
        Ok(text) => serde_yaml_ng::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e.into()),
    };
    let rank = |role: &str, id: &str| (role != "server", role == "mobile", id.to_owned());
    let own = rank(
        match node.role {
            NodeRole::Laptop => "laptop",
            NodeRole::Server => "server",
            NodeRole::Mobile => "mobile",
        },
        &node.node_id,
    );
    Ok(config
        .get("sync")
        .and_then(|sync| sync.get("peers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|peer| {
            Some(rank(
                peer.get("role")?.as_str()?,
                peer.get("node_id")?.as_str()?,
            ))
        })
        .filter(|peer| peer.2 != node.node_id)
        .all(|peer| own < peer))
}

fn maximum(state: &JournalState, project: &str, existing: u64) -> u64 {
    // Historical operations survive deletion and journal compaction. Never
    // recycle a task number merely because its live file disappeared.
    state
        .operations
        .values()
        .filter(|op| op.project == project)
        .filter(|op| {
            op.entity == "task"
                || (op.entity == "tombstone"
                    && op.set.get("entity").and_then(Value::as_str) == Some("task"))
        })
        .filter_map(|op| number(&op.id))
        .fold(existing, u64::max)
}

fn remaining(grants: &[(&Operation, IdLease)], node: &str, allocation: &Allocation) -> u64 {
    grants
        .iter()
        .filter(|(_, lease)| lease.node == node)
        .map(|(op, lease)| {
            lease.to.saturating_sub(
                allocation
                    .used
                    .get(&op.op)
                    .copied()
                    .unwrap_or(lease.from - 1),
            )
        })
        .fold(0, u64::saturating_add)
}

impl Journal {
    /// Prepare grants, or transfer an existing laptop authority to its server.
    /// The transport must first exchange operations and supply the peer's vector
    /// and reported number of unused IDs. Apply the returned operations on the
    /// peer, then call `confirm_id_leases` with its new vector. Retransmit after
    /// a lost response; unacknowledged grants are returned without extending them.
    pub fn prepare_id_leases(
        &self,
        project: &str,
        peer: &str,
        acknowledged: &VersionVector,
        peer_remaining: u64,
    ) -> Result<Vec<Operation>> {
        if peer.is_empty() || peer == self.node.node_id || peer.contains(['/', '\\', '\0']) {
            return Err(invalid("invalid lease peer"));
        }
        if project.is_empty()
            || project == "."
            || project == ".."
            || project.contains(['/', '\\', '\0'])
        {
            return Err(invalid("invalid project alias"));
        }
        let store = self.root.parent().expect("sync parent");
        let board = Storage::new(store.join("projects").join(project));
        if !board.tasks_dir.is_dir() {
            return Err(invalid("lease project does not exist"));
        }
        apply::recover_remote(&self.root)?;
        let _board = board.lock()?;
        let _sync = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let mut state: JournalState = load(&self.root.join("meta/state.json"))?;
        if !state.project_uids.contains_key(project) {
            state.project_uids.insert(project.into(), uuid()?);
        }
        let uid = state.project_uids[project].clone();
        let existing = grants(&state, &uid)?;
        let authority = authority(&state, &uid)?;
        if authority
            .as_ref()
            .is_some_and(|(issuer, _)| *issuer != self.node.node_id)
            || (authority.is_none() && !fallback_issuer(store, &self.node)?)
        {
            return Err(invalid("SYNC_LEASE_AUTHORITY"));
        }
        let pending: Vec<_> = state
            .operations
            .values()
            .filter(|op| op.entity == "lease" && op.project == uid)
            .filter(|op| {
                sequence(op).expect("valid lease") > *acknowledged.get(&op.hlc.2).unwrap_or(&0)
            })
            .cloned()
            .collect();
        if !pending.is_empty() {
            return Ok(pending);
        }
        if state
            .vector
            .iter()
            .any(|(node, seq)| acknowledged.get(node).copied().unwrap_or(0) < *seq)
        {
            return Err(invalid("SYNC_LEASE_VECTOR_UNCONFIRMED"));
        }
        let config: Value = match fs::read_to_string(store.join("config.yaml")) {
            Ok(text) => serde_yaml_ng::from_str(&text)?,
            Err(e) => return Err(e.into()),
        };
        let block = match config.get("sync").and_then(|sync| sync.get("id_block")) {
            Some(value) => value
                .as_u64()
                .filter(|size| *size > 0)
                .ok_or_else(|| invalid("sync.id_block must be a positive integer"))?,
            None => 50,
        };
        let peer_role = config
            .get("sync")
            .and_then(|sync| sync.get("peers"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|entry| entry.get("node_id").and_then(Value::as_str) == Some(peer))
            .and_then(|entry| entry.get("role"))
            .and_then(Value::as_str);
        let reserve = match config
            .get("sync")
            .and_then(|sync| sync.get("lease_reserve"))
        {
            Some(value) => value
                .as_u64()
                .filter(|size| *size > 0)
                .ok_or_else(|| invalid("sync.lease_reserve must be a positive integer"))?,
            None => 200,
        };
        let allocation_path = self.root.join("meta/allocation.json");
        let allocation: Allocation = load(&allocation_path)?;
        let own_remaining = remaining(&existing, &self.node.node_id, &allocation);
        let mut nodes = Vec::new();
        if existing.is_empty() || own_remaining < block.div_ceil(2) {
            nodes.push((self.node.node_id.clone(), block, false));
        }
        if peer_role == Some("mobile") {
            if peer_remaining < reserve {
                nodes.push((peer.to_owned(), reserve - peer_remaining, false));
            }
            if !existing
                .iter()
                .any(|(_, grant)| grant.node == peer && grant.reserve)
            {
                nodes.push((peer.to_owned(), reserve, true));
            }
        } else if !existing.iter().any(|(_, grant)| grant.node == peer)
            || peer_remaining < block.div_ceil(2)
        {
            nodes.push((peer.to_owned(), block, false));
        }
        let max_file = board.max_task_number();
        let mut end = existing
            .iter()
            .map(|(_, grant)| grant.to)
            .fold(maximum(&state, &uid, max_file), u64::max);
        let mut clock: Hlc = load(&self.root.join("hlc"))?;
        let mut operations = Vec::new();
        let transfer = authority
            .as_ref()
            .filter(|_| self.node.role != NodeRole::Server && peer_role == Some("server"));
        if transfer.is_some() {
            nodes.clear();
            nodes.push((peer.to_owned(), 0, false));
        }
        for (node, size, is_reserve) in nodes {
            let from = end
                .checked_add(1)
                .ok_or_else(|| invalid("task IDs exhausted"))?;
            if size > 0 {
                end = from
                    .checked_add(size - 1)
                    .filter(|end| *end < u64::MAX)
                    .ok_or_else(|| invalid("task IDs exhausted"))?;
            }
            let seq = state
                .vector
                .get(&self.node.node_id)
                .copied()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| invalid("operation sequence exhausted"))?;
            clock = clock.tick(now_ms(), &self.node.node_id)?;
            let id = format!("{}:{seq}", self.node.node_id);
            let op = Operation {
                op: id.clone(),
                id,
                hlc: clock.clone(),
                store: self.node.store_id.clone(),
                project: uid.clone(),
                entity: "lease".into(),
                set: if let Some((_, epoch)) = transfer {
                    BTreeMap::from([
                        ("issuer".into(), Value::String(node)),
                        (
                            "from_issuer".into(),
                            Value::String(self.node.node_id.clone()),
                        ),
                        ("epoch".into(), Value::from(epoch + 1)),
                    ])
                } else {
                    let mut fields = BTreeMap::from([
                        ("node".into(), Value::String(node)),
                        ("from".into(), Value::from(from)),
                        ("to".into(), Value::from(end)),
                    ]);
                    if is_reserve {
                        fields.insert("reserve".into(), Value::Bool(true));
                    }
                    fields
                },
                prev: BTreeMap::new(),
                actor: "kanban".into(),
            };
            apply_meta(&mut state, &op);
            operations.push(op);
        }
        if !operations.is_empty() {
            let pending = Pending {
                path: allocation_path,
                content: Some(json(&allocation)?),
                operations: operations.clone(),
                state,
            };
            atomic_write_text_raw(&self.root.join("pending.json"), &json(&pending)?)?;
            self.finish(&pending, false)?;
            fs::remove_file(self.root.join("pending.json"))?;
        }
        Ok(operations)
    }

    /// Mark source-side grants spendable only after the paired peer applied them.
    pub fn confirm_id_leases(&self, peer: &str, acknowledged: &VersionVector) -> Result<()> {
        apply::recover_remote(&self.root)?;
        let _sync = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let state: JournalState = load(&self.root.join("meta/state.json"))?;
        let path = self.root.join("meta/allocation.json");
        let mut allocation: Allocation = load(&path)?;
        let projects: BTreeSet<_> = state
            .operations
            .values()
            .filter(|op| {
                op.entity == "lease"
                    && op.hlc.2 == self.node.node_id
                    && op.set.get("node").and_then(Value::as_str) == Some(peer)
            })
            .map(|op| op.project.as_str())
            .collect();
        for op in state.operations.values().filter(|op| {
            op.entity == "lease"
                && op.hlc.2 == self.node.node_id
                && projects.contains(op.project.as_str())
        }) {
            if sequence(op)? <= *acknowledged.get(&self.node.node_id).unwrap_or(&0) {
                allocation.confirmed.insert(op.op.clone());
            }
        }
        atomic_write_text_raw(&path, &json(&allocation)?)
    }

    /// Report durable unused capacity for the next online renewal handshake.
    pub fn remaining_task_ids(&self, project: &str) -> Result<u64> {
        self.available_task_ids(project, false)
    }

    /// Capacity advertised for renewal excludes the emergency mobile range.
    pub fn remaining_primary_task_ids(&self, project: &str) -> Result<u64> {
        self.available_task_ids(project, true)
    }

    fn available_task_ids(&self, project: &str, primary_only: bool) -> Result<u64> {
        apply::recover_remote(&self.root)?;
        let _sync = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let state: JournalState = load(&self.root.join("meta/state.json"))?;
        let Some(uid) = state.project_uids.get(project) else {
            return Ok(0);
        };
        let allocation: Allocation = load(&self.root.join("meta/allocation.json"))?;
        let available: Vec<_> = grants(&state, uid)?
            .into_iter()
            .filter(|(op, lease)| {
                lease.node == self.node.node_id
                    && (!primary_only || !lease.reserve)
                    && (op.hlc.2 != self.node.node_id || allocation.confirmed.contains(&op.op))
            })
            .collect();
        Ok(remaining(&available, &self.node.node_id, &allocation))
    }
}

pub(crate) fn allocate(board: &Storage) -> Result<Option<String>> {
    let path = board.tasks_dir.join("todo/TASK-001.md");
    let Some(target) = target(&path) else {
        return Ok(None);
    };
    let journal = Journal::open(&target.store)?;
    let _sync = lock_file_reentrant(&journal.root.join(".lock"))?;
    journal.recover()?;
    let state: JournalState = load(&journal.root.join("meta/state.json"))?;
    let uid = state.project_uids.get(&target.project);
    let all = match uid {
        Some(uid) => {
            let mut grants = grants(&state, uid)?;
            grants.sort_by_key(|(_, grant)| (grant.reserve, grant.from));
            grants
        }
        None => Vec::new(),
    };
    let Some(uid) = uid.filter(|_| !all.is_empty()) else {
        if fallback_issuer(&target.store, &journal.node)? {
            return Ok(None);
        }
        return Err(invalid(
            "SYNC_ID_BLOCK_EXHAUSTED: waiting for the first task ID lease from the paired server",
        ));
    };
    let allocation_path = journal.root.join("meta/allocation.json");
    let mut allocation: Allocation = load(&allocation_path)?;
    for (op, lease) in all {
        if lease.node != journal.node.node_id
            || (op.hlc.2 == journal.node.node_id && !allocation.confirmed.contains(&op.op))
        {
            continue;
        }
        let used = allocation
            .used
            .get(&op.op)
            .copied()
            .unwrap_or(lease.from - 1);
        let next = used
            .checked_add(1)
            .ok_or_else(|| invalid("task IDs exhausted"))?;
        let historical = state
            .operations
            .values()
            .filter(|op| op.project == *uid)
            .filter_map(|op| number(&op.id))
            .filter(|id| *id >= lease.from && *id <= lease.to)
            .max();
        let next = next.max(historical.map_or(lease.from, |id| id + 1));
        // Imported live files may predate capture. Skip any occupied number.
        let mut next = next;
        while next <= lease.to && board.task_exists(&format!("TASK-{next:03}")) {
            next += 1;
        }
        if next <= lease.to {
            allocation.used.insert(op.op.clone(), next);
            atomic_write_text_raw(&allocation_path, &json(&allocation)?)?;
            return Ok(Some(format!("TASK-{next:03}")));
        }
    }
    Err(invalid(
        "SYNC_ID_BLOCK_EXHAUSTED: reconnect to renew task ID leases",
    ))
}
