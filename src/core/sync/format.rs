//! Store, operation and protocol versions. `sync/FORMAT` is the commit point
//! of a migration: readers newer than the file refuse to write anything.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use super::*;

pub const STORE_FORMAT: u32 = 5;
/// Bootstrap snapshots still carry the format 4 single `meta/uids.json`
/// (written from the tables, split again on install), so nodes on format 4
/// and 5 exchange them.
pub(super) const SNAPSHOT_FORMAT: u32 = 4;
/// Append-only segments. Format 3 only shards `meta/projection.json` into
/// `meta/projection/`, which the loader does on the next checkpoint.
pub(super) const SEGMENTS_FORMAT: u32 = 2;
pub const OP_FORMAT: u32 = 1;
pub const SYNC_PROTOCOL: u32 = 3;
pub const MIN_SYNC_PROTOCOL: u32 = 3;

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

struct LimitsCache {
    mtime: Option<SystemTime>,
    len: u64,
    limits: JournalLimits,
}

static LIMITS_CACHE: LazyLock<Mutex<std::collections::HashMap<PathBuf, LimitsCache>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

impl JournalLimits {
    pub(super) fn load(store: &Path) -> Result<Self> {
        let path = store.join("config.yaml");
        let meta = fs::metadata(&path).ok();
        let mtime = meta.as_ref().and_then(|meta| meta.modified().ok());
        let len = meta.as_ref().map(|meta| meta.len()).unwrap_or(0);
        {
            let cache = LIMITS_CACHE.lock().unwrap_or_else(|err| err.into_inner());
            if let Some(hit) = cache.get(store)
                && hit.mtime == mtime
                && hit.len == len
            {
                return Ok(hit.limits.clone());
            }
        }
        let limits = Self::read(&path)?;
        LIMITS_CACHE
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(
                store.to_path_buf(),
                LimitsCache {
                    mtime,
                    len,
                    limits: limits.clone(),
                },
            );
        Ok(limits)
    }

    fn read(path: &Path) -> Result<Self> {
        let mut limits = Self::default();
        let text = match fs::read_to_string(path) {
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
    let version = read_format(store)?;
    if version.is_some_and(|v| v > STORE_FORMAT || v == 0) {
        return Err(invalid(&format!(
            "SYNC_STORE_TOO_NEW: binary supports {STORE_FORMAT}"
        )));
    }
    if version == Some(STORE_FORMAT) {
        return Ok(());
    }
    if version.is_some_and(|v| v < SEGMENTS_FORMAT) || (version.is_none() && v1_present(&root)?) {
        migrate_v1::migrate(store)?;
    } else if leftovers(&root)? {
        migrate_v1::finish_v1_move(store)?;
    }
    if read_format(store)?.is_none() {
        let _guard = lock_file_reentrant(&root.join(".lock"))?;
        write_format(store, 3)?;
        write_stub(&root)?;
    }
    migrate_v4::migrate(store)?;
    migrate_v5(store)
}

/// 4 → 5: the uid tables move from `meta/uids.json` into one file per project
/// (`projection::split_uids`). A format 4 binary would read the missing file
/// as empty tables, so the bump keeps it off the store.
fn migrate_v5(store: &Path) -> Result<()> {
    let root = store.join("sync");
    let _sync = lock_file_reentrant(&root.join(".lock"))?;
    if read_format(store)?.is_some_and(|v| v >= 5) {
        return Ok(());
    }
    projection::split_uids(&root)?;
    write_format(store, 5)
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
    let _ = protocol;
    OP_FORMAT
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

/// `sync/armed` lives beside the journal, not under `sync/meta`: meta is part of
/// the sessions fingerprint and of snapshots. The empty file means the drift
/// index matches disk, so the next capture may append a delta safely.
pub(super) fn armed_path(store: &Path) -> PathBuf {
    store.join("sync/armed")
}

pub(super) fn armed(store: &Path) -> bool {
    armed_path(store).is_file()
}

pub(super) fn mark_armed(store: &Path) -> Result<()> {
    let path = armed_path(store);
    if path.is_file() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::File::create(path)?;
    Ok(())
}

pub(super) fn disarm(store: &Path) -> Result<()> {
    match fs::remove_file(armed_path(store)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Explicit false disables sync; otherwise a server link or server clients arms it.
pub fn enabled(store: &Path) -> bool {
    read_enabled(&store.join("config.yaml"))
}

fn read_enabled(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(config) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) else {
        return false;
    };
    let Some(sync) = config.get("sync") else {
        return false;
    };
    match sync.get("enabled") {
        Some(serde_yaml_ng::Value::Bool(value)) => *value,
        _ => {
            auto_enabled(sync)
                || (config
                    .get("node")
                    .and_then(|n| n.get("role"))
                    .and_then(|r| r.as_str())
                    == Some("server")
                    && path.parent().is_some_and(has_clients))
        }
    }
}

fn auto_enabled(sync: &serde_yaml_ng::Value) -> bool {
    sync.get("server")
        .and_then(|v| v.as_mapping())
        .is_some_and(|s| !s.is_empty())
}
fn has_clients(store: &Path) -> bool {
    fs::read_to_string(store.join("sync/hub/clients.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<BTreeMap<String, Value>>(&s).ok())
        .is_some_and(|c| !c.is_empty())
}

/// Test and setup helper: turn journaling on without discarding the rest of
/// `config.yaml`, and arm capture. Production turns sync on by pairing with a
/// server (`sync.server`), registering a hub client, or `sync.enabled` and
/// then [`super::activate`].
pub fn enable_sync(store: &Path) -> Result<()> {
    let path = store.join("config.yaml");
    let mut config: serde_yaml_ng::Value = match fs::read_to_string(&path) {
        Ok(text) => serde_yaml_ng::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_yaml_ng::Value::Mapping(Default::default())
        }
        Err(e) => return Err(e.into()),
    };
    if !config.is_mapping() {
        config = serde_yaml_ng::Value::Mapping(Default::default());
    }
    if !config.get("sync").is_some_and(|sync| sync.is_mapping()) {
        config["sync"] = serde_yaml_ng::Value::Mapping(Default::default());
    }
    config["sync"]["enabled"] = serde_yaml_ng::Value::Bool(true);
    atomic_write_text_raw(&path, &serde_yaml_ng::to_string(&config)?)?;
    mark_armed(store)
}

/// Role recorded in `sync/node.yaml`, without creating identity or the journal.
/// `None` means this store has no sync identity yet (treated as a laptop).
pub fn node_role(store: &Path) -> Result<Option<super::NodeRole>> {
    match fs::read_to_string(store.join("sync/node.yaml")) {
        Ok(text) => {
            let node: super::NodeIdentity = serde_yaml_ng::from_str(&text)?;
            Ok(Some(node.role))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// True when this store has no sync server and, as a server, no hub clients.
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
    !auto_enabled(sync) && !has_clients(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(store: &Path, body: &str) {
        fs::write(store.join("config.yaml"), body).unwrap();
    }

    #[test]
    fn enabled_follows_the_kill_switch_and_server() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        assert!(!enabled(store));
        write_config(store, "tui: {}\n");
        assert!(!enabled(store));
        write_config(store, "sync: {}\n");
        assert!(!enabled(store));
        write_config(store, "sync:\n  peers: []\n");
        assert!(!enabled(store));
        write_config(store, "sync:\n  peers:\n    - {name: a}\n");
        assert!(!enabled(store));
        write_config(store, "sync:\n  relay_remote: origin\n");
        assert!(!enabled(store));
        write_config(store, "sync:\n  server: {node_id: server}\n");
        assert!(enabled(store));
        write_config(store, "sync:\n  relay_remote: \"\"\n");
        assert!(!enabled(store));
        write_config(store, "sync:\n  enabled: true\n");
        assert!(enabled(store));
        write_config(
            store,
            "sync:\n  enabled: false\n  peers:\n    - {name: a}\n  relay_remote: origin\n",
        );
        assert!(!enabled(store));
        write_config(store, "[\n");
        assert!(!enabled(store));
        assert!(node_role(store).unwrap().is_none());
    }
}
