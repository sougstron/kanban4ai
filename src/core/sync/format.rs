//! Store, operation and protocol versions. `sync/FORMAT` is the commit point
//! of a migration: readers newer than the file refuse to write anything.
use std::fs;
use std::path::Path;

use super::*;

pub const STORE_FORMAT: u32 = 2;
pub const OP_FORMAT: u32 = 1;
pub const SYNC_PROTOCOL: u32 = 2;
pub const MIN_SYNC_PROTOCOL: u32 = 1;

pub(super) fn default_op_version() -> u32 {
    OP_FORMAT
}

pub(super) fn snapshot_format_v1() -> u32 {
    1
}

/// Thresholds from the machine-local `sync.journal` section.
#[derive(Debug, Clone)]
pub(super) struct JournalLimits {
    pub segment_bytes: u64,
    pub snapshot_ops: u64,
    pub snapshot_secs: u64,
}

impl Default for JournalLimits {
    fn default() -> Self {
        Self {
            segment_bytes: 4 * 1024 * 1024,
            snapshot_ops: 1000,
            snapshot_secs: 300,
        }
    }
}

impl JournalLimits {
    pub(super) fn load(store: &Path) -> Result<Self> {
        let mut limits = Self::default();
        let text = match fs::read_to_string(store.join("config.yaml")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(limits),
            Err(e) => return Err(e.into()),
        };
        let config: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text)?;
        let Some(journal) = config.get("sync").and_then(|s| s.get("journal")) else {
            return Ok(limits);
        };
        if let Some(value) = journal.get("segment_bytes") {
            limits.segment_bytes = positive(value, "sync.journal.segment_bytes")?;
        }
        if let Some(value) = journal.get("snapshot_ops") {
            limits.snapshot_ops = positive(value, "sync.journal.snapshot_ops")?;
        }
        if let Some(value) = journal.get("snapshot_secs") {
            limits.snapshot_secs = positive(value, "sync.journal.snapshot_secs")?;
        }
        Ok(limits)
    }
}

fn positive(value: &serde_yaml_ng::Value, name: &str) -> Result<u64> {
    let number = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .filter(|n| *n > 0)
        .ok_or_else(|| invalid(&format!("{name} must be a positive integer")))?;
    Ok(number)
}

pub(super) fn format_path(store: &Path) -> std::path::PathBuf {
    store.join("sync/FORMAT")
}

pub(super) fn read_format(store: &Path) -> Result<Option<u32>> {
    let path = format_path(store);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let number = text.trim().parse::<u32>().map_err(|_| {
                invalid(&format!(
                    "SYNC_STORE_TOO_NEW: unreadable store format; binary supports {STORE_FORMAT}"
                ))
            })?;
            Ok(Some(number))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn write_format(store: &Path, version: u32) -> Result<()> {
    atomic_write_text_raw(&format_path(store), &format!("{version}\n"))
}

/// Poison for binaries that still deserialize `meta/state.json` as a v1 journal.
pub(super) fn write_stub(sync_root: &Path) -> Result<()> {
    fs::create_dir_all(sync_root.join("meta"))?;
    atomic_write_text_raw(
        &sync_root.join("meta/state.json"),
        "{\"format\":2,\"vector\":\"store migrated to sync format 2; upgrade kanban4ai\"}\n",
    )
}

pub(super) fn is_stub(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("vector")?.as_str().map(|_| ()))
        .is_some()
}

/// Called from `Journal::open` after bootstrap recovery and before remote redo.
/// A format newer than this binary is refused with no writes.
pub(super) fn ensure(store: &Path) -> Result<()> {
    let root = store.join("sync");
    if !root.exists() {
        return Ok(());
    }
    match read_format(store)? {
        Some(version) if version > STORE_FORMAT => Err(invalid(&format!(
            "SYNC_STORE_TOO_NEW: store format {version}, binary supports {STORE_FORMAT}; upgrade kanban4ai"
        ))),
        Some(version) if version == STORE_FORMAT => {
            if leftovers(&root)? {
                crate::core::sync::migrate_v1::finish_v1_move(store)?;
            }
            Ok(())
        }
        Some(version) => {
            // Ordered migrations. Each one commits by writing FORMAT only after success.
            if version < 1 {
                return Err(invalid(&format!(
                    "SYNC_STORE_TOO_NEW: store format {version}, binary supports {STORE_FORMAT}; upgrade kanban4ai"
                )));
            }
            crate::core::sync::migrate_v1::migrate(store)
        }
        None if v1_present(&root)? => crate::core::sync::migrate_v1::migrate(store),
        None => {
            let _guard = lock_file_reentrant(&root.join(".lock"))?;
            if read_format(store)?.is_none() && !v1_present(&root)? {
                write_format(store, STORE_FORMAT)?;
                write_stub(&root)?;
            } else if v1_present(&root)? {
                drop(_guard);
                return crate::core::sync::migrate_v1::migrate(store);
            }
            Ok(())
        }
    }
}

fn leftovers(root: &Path) -> Result<bool> {
    if root.join("snapshot").is_dir() || root.join(".staging").exists() {
        return Ok(true);
    }
    if flat_journal_files(root)?.into_iter().next().is_some() {
        return Ok(true);
    }
    match fs::read_to_string(root.join("meta/state.json")) {
        Ok(text) if !is_stub(&text) => Ok(true),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn v1_present(root: &Path) -> Result<bool> {
    if root.join("meta/state.json").is_file()
        && !is_stub(&fs::read_to_string(root.join("meta/state.json"))?)
    {
        return Ok(true);
    }
    Ok(flat_journal_files(root)?.into_iter().next().is_some())
}

pub(super) fn flat_journal_files(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let journal = root.join("journal");
    if !journal.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(&journal)? {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Inclusive protocol bounds. Missing fields (`0`) are a legacy peer at version 1.
pub fn protocol_bounds(protocol: u32, min_protocol: u32) -> (u32, u32) {
    if protocol == 0 && min_protocol == 0 {
        (1, 1)
    } else {
        let max = if protocol == 0 { 1 } else { protocol };
        let min = if min_protocol == 0 { 1 } else { min_protocol };
        (min, max)
    }
}

/// Highest operation `v` a peer speaking `protocol` can accept.
/// Protocol 2 carries op v ≤ 1; a future protocol 3 would carry v ≤ 2.
pub fn max_op_version(protocol: u32) -> u32 {
    if protocol <= 1 { 1 } else { protocol - 1 }
}

/// Effective protocol when the ranges overlap, otherwise `None` (no exchange).
pub fn negotiate(
    local_min: u32,
    local_max: u32,
    remote_protocol: u32,
    remote_min: u32,
) -> Option<u32> {
    let (remote_lo, remote_hi) = protocol_bounds(remote_protocol, remote_min);
    let lo = local_min.max(remote_lo);
    let hi = local_max.min(remote_hi);
    (lo <= hi).then_some(hi)
}

pub fn accept_store_protocol(remote_protocol: u32, remote_min: u32) -> Result<u32> {
    negotiate(
        MIN_SYNC_PROTOCOL,
        SYNC_PROTOCOL,
        remote_protocol,
        remote_min,
    )
    .ok_or_else(|| invalid("SYNC_PROTOCOL_MISMATCH"))
}

/// True when this store has no paired peers and git relay is off.
/// A config that does not parse still counts as "do not prune".
pub(super) fn solo_store(store: &Path) -> bool {
    let Ok(text) = fs::read_to_string(store.join("config.yaml")) else {
        return true;
    };
    let Ok(config) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) else {
        return false;
    };
    let Some(sync) = config.get("sync") else {
        return true;
    };
    let peers = sync
        .get("peers")
        .and_then(|v| v.as_sequence())
        .is_some_and(|s| !s.is_empty());
    let relay = sync
        .get("relay_remote")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty());
    !peers && !relay
}
