//! Consistent snapshots of the store and a restore path that survives sync.
//!
//! A replica is not a backup: deletes replicate. `create` copies the allowlist
//! under the store → boards → sync locks, then compresses after those locks
//! are released. Restore of a board leaves `sync/` alone and calls
//! [`crate::core::sync::activate`], so a task that was deleted comes back as a
//! `restore` operation with a newer HLC.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDateTime, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::{Archive, Builder, Header};

use super::config::{
    BACKUP_ARTIFACT_WEEKLY_DAYS, BACKUP_PRE_RESTORE_DAYS, BackupArtifacts, BackupPolicy,
    default_backup_dir,
};
use super::error::{KanbanError, Result};
use super::project::ProjectStore;
use super::storage::{self, lock_file_reentrant};
use super::sync::{self, STORE_FORMAT};

const BACKUP_FORMAT: u32 = 1;
const BOARD_DIRS: &[&str] = &[
    "tasks",
    "threads",
    "instructions",
    "assets/images",
    "context",
    "stats",
];
const SYNC_FILES: &[&str] = &[
    "FORMAT",
    "hlc",
    "node.yaml",
    "server.yaml",
    "tokens.yaml",
    "cursor.json",
    "landing-intent.json",
    // Reconcile treats a missing index as "every file appeared". Keep it so a
    // full restore does not replay the whole board into the journal.
    "drift.jsonl",
];
const SYNC_DIRS: &[&str] = &["journal", "meta", "hub", "legacy-v1", "blobs"];
const IDENTITY_FILES: &[&str] = &["node.yaml", "server.yaml", "tokens.yaml", "cursor.json"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Manual,
    Daily,
    PreRestore,
}

impl Kind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "manual" => Some(Self::Manual),
            "daily" => Some(Self::Daily),
            "pre-restore" => Some(Self::PreRestore),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Daily => "daily",
            Self::PreRestore => "pre-restore",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Part {
    Core,
    Artifacts,
}

impl Part {
    fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Artifacts => "artifacts",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "core" => Some(Self::Core),
            "artifacts" => Some(Self::Artifacts),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    Board,
    Full,
}

impl RestoreMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub backup_format: u32,
    pub version: String,
    #[serde(default)]
    pub sync_format: Option<u32>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub kind: String,
    pub part: String,
    pub with_artifacts: bool,
    pub projects: Vec<ManifestProject>,
    pub files: Vec<ManifestFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_changing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestProject {
    pub id: String,
    pub name: String,
    pub data_root: String,
    #[serde(default)]
    pub external: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub dir: PathBuf,
    pub kind: Kind,
    pub with_artifacts: bool,
    pub policy: BackupPolicy,
}

#[derive(Debug, Clone)]
pub struct ArchiveInfo {
    pub path: PathBuf,
    pub bytes: u64,
    pub files: usize,
}

#[derive(Debug, Clone)]
pub struct Created {
    pub core: ArchiveInfo,
    pub artifacts: Option<ArchiveInfo>,
    /// Time spent copying while the store, board and sync locks were held.
    pub locked_copy: Duration,
}

#[derive(Debug, Clone)]
pub struct RestoreRequest {
    pub archive: PathBuf,
    pub dir: PathBuf,
    pub mode: RestoreMode,
    pub project: Option<String>,
    pub identity: bool,
    /// Confirms for `restore`; `restore_with` asks its callback instead.
    pub yes: bool,
    pub force: bool,
    pub policy: BackupPolicy,
}

/// What a restore replaces. `restore_with` hands it to the confirm callback
/// before anything is written, and returns it once the swap is done.
#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub mode: String,
    pub projects: Vec<String>,
    pub tasks_added: usize,
    pub tasks_replaced: usize,
    pub tasks_removed: usize,
    pub threads_added: usize,
    pub threads_replaced: usize,
    pub threads_removed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListRow {
    pub created: String,
    pub kind: String,
    pub bucket: String,
    pub core_bytes: u64,
    pub artifacts_bytes: u64,
    pub verified: bool,
    pub core: Option<String>,
    pub artifacts: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ArchiveEntry {
    key: String,
    path: PathBuf,
    created: DateTime<Utc>,
    kind: Kind,
    part: Part,
    bytes: u64,
    verified: bool,
    seq: u32,
}

#[derive(Debug, Clone)]
struct Item {
    abs: PathBuf,
    archive_path: String,
}

#[derive(Debug, Clone)]
struct Board {
    id: String,
    name: String,
    data_root: PathBuf,
    data_root_rel: String,
    external: bool,
}

struct DirLock {
    file: File,
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl DirLock {
    fn acquire(dir: &Path, block: bool) -> Result<Option<Self>> {
        fs::create_dir_all(dir)?;
        set_mode(dir, 0o700)?;
        let path = dir.join(".lock");
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path)?;
        set_mode(&path, 0o600)?;
        if block {
            file.lock_exclusive()?;
        } else {
            match file.try_lock_exclusive() {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(err) => return Err(err.into()),
            }
        }
        Ok(Some(Self { file }))
    }
}

struct Staged {
    archive_path: String,
    path: PathBuf,
    mode: u32,
    size: u64,
    sha256: String,
}

struct Undo {
    from: PathBuf,
    to: PathBuf,
}

/// Backup directory from policy / `--dir`, rejected when it lives in the store.
pub fn resolve_dir(store: &Path, override_dir: Option<&Path>) -> Result<(BackupPolicy, PathBuf)> {
    let policy = ProjectStore::at(store)
        .load_global_config()?
        .backup_policy();
    resolve_dir_with(store, policy, override_dir)
}

fn resolve_dir_with(
    store: &Path,
    mut policy: BackupPolicy,
    override_dir: Option<&Path>,
) -> Result<(BackupPolicy, PathBuf)> {
    if let Some(dir) = override_dir {
        let abs = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            std::env::current_dir()?.join(dir)
        };
        policy.dir = Some(abs);
    }
    let dir = policy.dir.clone().unwrap_or_else(default_backup_dir);
    ensure_outside_store(store, &dir)?;
    Ok((policy, dir))
}

pub fn create(store: &Path, request: &CreateRequest) -> Result<Created> {
    let guard = DirLock::acquire(&request.dir, true)?.expect("blocking lock");
    create_locked(store, request, &guard)
}

fn create_locked(store: &Path, request: &CreateRequest, _guard: &DirLock) -> Result<Created> {
    ensure_outside_store(store, &request.dir)?;
    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let seq = free_seq(&request.dir, &stamp, request.kind)?;
    let staging_path = request.dir.join(format!(".staging-{stamp}-{seq}"));
    let _staging = TempDir::new(staging_path);
    let (boards, core_items) = {
        let _store_lock = ProjectStore::at(store).lock()?;
        let boards = boards_of(store)?;
        let mut guards = Vec::new();
        for board in &boards {
            let kanban = board.data_root.join(".kanban");
            if kanban.is_dir() && !kanban.is_symlink() {
                guards.push(storage::Storage::new(&board.data_root).lock()?);
            }
        }
        let sync_guard = if store.join("sync").is_dir() {
            Some(lock_file_reentrant(&store.join("sync/.lock"))?)
        } else {
            None
        };
        let started = Instant::now();
        let items = core_items(store, &boards)?;
        let staged = stage_copy(&items, &_staging.path)?;
        let locked_copy = started.elapsed();
        drop(sync_guard);
        drop(guards);
        drop(_store_lock);
        (boards, (staged, locked_copy))
    };
    let (core_staged, locked_copy) = core_items;
    let projects = manifest_projects(&boards);
    let with_artifacts = request.with_artifacts;
    let core_manifest = manifest_for(
        store,
        request.kind,
        Part::Core,
        with_artifacts,
        &projects,
        &core_staged,
        &[],
    )?;
    let core_path = archive_name(&request.dir, &stamp, request.kind, seq, Part::Core);
    pack(&core_manifest, &core_staged, &core_path)?;
    let artifacts = if with_artifacts {
        let mut skipped = Vec::new();
        let art_items = artifact_items(store, &boards)?;
        let art_staged = stage_stable(&art_items, &_staging.path, &mut skipped)?;
        let art_manifest = manifest_for(
            store,
            request.kind,
            Part::Artifacts,
            true,
            &projects,
            &art_staged,
            &skipped,
        )?;
        let art_path = archive_name(&request.dir, &stamp, request.kind, seq, Part::Artifacts);
        pack(&art_manifest, &art_staged, &art_path)?;
        Some(finish_archive(&art_path)?)
    } else {
        None
    };
    let core = finish_archive(&core_path)?;
    prune_locked(&request.dir, &request.policy, false)?;
    Ok(Created {
        core,
        artifacts,
        locked_copy,
    })
}

fn finish_archive(path: &Path) -> Result<ArchiveInfo> {
    let manifest = verify(path)?;
    mark_verified(path)?;
    let meta = fs::metadata(path)?;
    Ok(ArchiveInfo {
        path: path.to_path_buf(),
        bytes: meta.len(),
        files: manifest.files.len(),
    })
}

pub fn verify(path: &Path) -> Result<Manifest> {
    let file = File::open(path)
        .map_err(|err| KanbanError::BackupVerifyFailed(format!("{}: {err}", path.display())))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let mut manifest: Option<Manifest> = None;
    let mut seen: BTreeMap<String, (u64, String, u32)> = BTreeMap::new();
    let entries = archive
        .entries()
        .map_err(|err| KanbanError::BackupVerifyFailed(format!("{}: {err}", path.display())))?;
    for entry in entries {
        let mut entry = entry
            .map_err(|err| KanbanError::BackupVerifyFailed(format!("{}: {err}", path.display())))?;
        let raw = entry
            .path()
            .map_err(|err| KanbanError::BackupVerifyFailed(err.to_string()))?
            .to_string_lossy()
            .to_string();
        let rel = safe_rel(&raw)?;
        if rel == "MANIFEST.json" {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|err| KanbanError::BackupVerifyFailed(format!("MANIFEST.json: {err}")))?;
            let parsed: Manifest = serde_json::from_str(&text)
                .map_err(|err| KanbanError::BackupVerifyFailed(format!("MANIFEST.json: {err}")))?;
            check_format(&parsed)?;
            manifest = Some(parsed);
            continue;
        }
        if manifest.is_none() {
            return Err(KanbanError::BackupVerifyFailed(
                "MANIFEST.json is not the first archive entry".into(),
            ));
        }
        // Hash as a stream; only the files `parse_entry` checks are buffered,
        // so a large artifact never sits in memory whole.
        let mode = entry.header().mode().unwrap_or(0) & 0o777;
        let mut hasher = HashingSink {
            hasher: Sha256::new(),
            kept: needs_parse(&rel).then(Vec::new),
        };
        let size = io::copy(&mut entry, &mut hasher)
            .map_err(|err| KanbanError::BackupVerifyFailed(format!("{rel}: {err}")))?;
        let hash = format!("{:x}", hasher.hasher.finalize());
        if seen.insert(rel.clone(), (size, hash, mode)).is_some() {
            return Err(KanbanError::BackupVerifyFailed(format!(
                "duplicate archive path {rel}"
            )));
        }
        if let Some(bytes) = hasher.kept {
            parse_entry(&rel, &bytes)?;
        }
    }
    let manifest = manifest
        .ok_or_else(|| KanbanError::BackupVerifyFailed("archive has no MANIFEST.json".into()))?;
    if seen.len() != manifest.files.len() {
        return Err(KanbanError::BackupVerifyFailed(format!(
            "archive has {} files, manifest lists {}",
            seen.len(),
            manifest.files.len()
        )));
    }
    for file in &manifest.files {
        let Some((size, hash, mode)) = seen.get(&file.path) else {
            return Err(KanbanError::BackupVerifyFailed(format!(
                "missing archive entry {}",
                file.path
            )));
        };
        if *size != file.size || hash != &file.sha256 || *mode != file.mode {
            return Err(KanbanError::BackupVerifyFailed(format!(
                "checksum mismatch {}",
                file.path
            )));
        }
    }
    Ok(manifest)
}

fn check_format(manifest: &Manifest) -> Result<()> {
    if manifest.backup_format > BACKUP_FORMAT {
        return Err(KanbanError::BackupFormatNewer(format!(
            "backup format {} is newer than this binary ({BACKUP_FORMAT})",
            manifest.backup_format
        )));
    }
    if manifest.backup_format != BACKUP_FORMAT {
        return Err(KanbanError::BackupVerifyFailed(format!(
            "unsupported backup format {}",
            manifest.backup_format
        )));
    }
    if let Some(version) = manifest.sync_format
        && version > STORE_FORMAT
    {
        return Err(KanbanError::BackupFormatNewer(format!(
            "sync format {version} is newer than this binary ({STORE_FORMAT})"
        )));
    }
    if Part::parse(&manifest.part).is_none() || Kind::parse(&manifest.kind).is_none() {
        return Err(KanbanError::BackupVerifyFailed(
            "manifest kind or part is unknown".into(),
        ));
    }
    Ok(())
}

struct HashingSink {
    hasher: Sha256,
    kept: Option<Vec<u8>>,
}

impl Write for HashingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        if let Some(kept) = &mut self.kept {
            kept.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Paths `parse_entry` actually parses; everything else is only hashed.
fn needs_parse(rel: &str) -> bool {
    (rel.contains("/.kanban/tasks/") && rel.ends_with(".md"))
        || (rel.contains("/.kanban/threads/") && (rel.ends_with(".yaml") || rel.ends_with(".yml")))
        || rel == "store/config.yaml"
        || rel.ends_with("/.kanban/config.yaml")
}

fn parse_entry(rel: &str, bytes: &[u8]) -> Result<()> {
    let text = |err: &str| {
        std::str::from_utf8(bytes).map_err(|_| KanbanError::BackupVerifyFailed(err.into()))
    };
    if rel.contains("/.kanban/tasks/") && rel.ends_with(".md") {
        let text = text(&format!("{rel} is not utf-8"))?;
        parse_task(text).map_err(|err| KanbanError::BackupVerifyFailed(format!("{rel}: {err}")))?;
    } else if rel.contains("/.kanban/threads/") && (rel.ends_with(".yaml") || rel.ends_with(".yml"))
    {
        let text = text(&format!("{rel} is not utf-8"))?;
        serde_yaml_ng::from_str::<super::models::Thread>(text)
            .map_err(|err| KanbanError::BackupVerifyFailed(format!("{rel}: {err}")))?;
    } else if rel == "store/config.yaml" || rel.ends_with("/.kanban/config.yaml") {
        let text = text(&format!("{rel} is not utf-8"))?;
        if !text.trim().is_empty() {
            let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(text)
                .map_err(|err| KanbanError::BackupVerifyFailed(format!("{rel}: {err}")))?;
            if !value.is_mapping() && !value.is_null() {
                return Err(KanbanError::BackupVerifyFailed(format!(
                    "{rel} is not a mapping"
                )));
            }
        }
    }
    Ok(())
}

fn parse_task(content: &str) -> std::result::Result<(), String> {
    let mut parts = content.splitn(3, "---");
    let (head, front, _) = match (parts.next(), parts.next(), parts.next()) {
        (Some(head), Some(front), Some(body)) if head.trim().is_empty() => (head, front, body),
        _ => return Err("invalid task file".into()),
    };
    let _ = head;
    serde_yaml_ng::from_str::<super::models::Task>(front).map_err(|err| err.to_string())?;
    Ok(())
}

pub fn resolve_archive(dir: &Path, target: &str) -> Result<PathBuf> {
    if target == "latest" {
        return scan(dir)?
            .into_iter()
            .filter(|entry| entry.part == Part::Core)
            .max_by_key(|entry| (entry.created, entry.seq))
            .map(|entry| entry.path)
            .ok_or_else(|| KanbanError::Invalid(format!("no backups in {}", dir.display())));
    }
    let path = PathBuf::from(target);
    if path.is_file() {
        return Ok(path);
    }
    let joined = dir.join(target);
    if joined.is_file() {
        return Ok(joined);
    }
    Err(KanbanError::Invalid(format!("backup not found: {target}")))
}

pub fn list(dir: &Path, policy: &BackupPolicy) -> Result<Vec<ListRow>> {
    let entries = scan(dir)?;
    let now = Utc::now();
    let (_keep, delete, buckets) = select_keep(&entries, now, &retention(policy));
    let deleted: BTreeSet<&str> = delete.iter().map(|entry| entry.key.as_str()).collect();
    let mut groups: BTreeMap<(DateTime<Utc>, Kind, u32), Vec<&ArchiveEntry>> = BTreeMap::new();
    for entry in &entries {
        groups
            .entry((entry.created, entry.kind, entry.seq))
            .or_default()
            .push(entry);
    }
    let mut rows = Vec::new();
    for ((created, kind, _), group) in groups {
        let core = group.iter().find(|entry| entry.part == Part::Core).copied();
        let artifacts = group
            .iter()
            .find(|entry| entry.part == Part::Artifacts)
            .copied();
        let label_of = |entry: &ArchiveEntry| {
            if deleted.contains(entry.key.as_str()) {
                "delete"
            } else {
                buckets.get(&entry.key).copied().unwrap_or("keep")
            }
        };
        let bucket = core
            .map(label_of)
            .or_else(|| artifacts.map(label_of))
            .unwrap_or("keep");
        rows.push(ListRow {
            created: created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            kind: kind.to_string(),
            bucket: bucket.to_string(),
            core_bytes: core.map(|entry| entry.bytes).unwrap_or(0),
            artifacts_bytes: artifacts.map(|entry| entry.bytes).unwrap_or(0),
            verified: core.is_some_and(|entry| entry.verified)
                || (core.is_none() && artifacts.is_some_and(|entry| entry.verified)),
            core: core.map(|entry| entry.path.display().to_string()),
            artifacts: artifacts.map(|entry| entry.path.display().to_string()),
        });
    }
    rows.sort_by(|a, b| b.created.cmp(&a.created));
    Ok(rows)
}

pub fn prune(dir: &Path, policy: &BackupPolicy, dry_run: bool) -> Result<Vec<PathBuf>> {
    let _guard = DirLock::acquire(dir, true)?.expect("blocking lock");
    prune_locked(dir, policy, dry_run)
}

fn prune_locked(dir: &Path, policy: &BackupPolicy, dry_run: bool) -> Result<Vec<PathBuf>> {
    let entries = scan(dir)?;
    let (_keep, delete, _) = select_keep(&entries, Utc::now(), &retention(policy));
    let mut doomed: Vec<PathBuf> = delete.into_iter().map(|entry| entry.path).collect();
    doomed.extend(stale_scratch(dir)?);
    doomed.sort();
    doomed.dedup();
    if !dry_run {
        for path in &doomed {
            if path.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
                let _ = fs::remove_file(verified_sidecar(path));
            }
        }
    }
    Ok(doomed)
}

/// Grandfather-father-son retention plus the size cap.
///
/// Daily/weekly/monthly candidates are `daily` and `manual` archives. Core
/// and artifacts are selected independently, then the size cap drops the
/// oldest artifacts first and never drops the newest verified core archive.
pub(crate) fn select_keep(
    entries: &[ArchiveEntry],
    now: DateTime<Utc>,
    policy: &Retention,
) -> (
    Vec<ArchiveEntry>,
    Vec<ArchiveEntry>,
    HashMap<String, &'static str>,
) {
    let mut buckets = HashMap::new();
    let mut keep_keys = BTreeSet::new();
    for part in [Part::Core, Part::Artifacts] {
        let mine: Vec<&ArchiveEntry> = entries.iter().filter(|entry| entry.part == part).collect();
        gfs(&mine, now, policy, &mut keep_keys, &mut buckets);
    }
    let protected = entries
        .iter()
        .filter(|entry| entry.part == Part::Core && entry.verified)
        .max_by(|a, b| {
            a.created
                .cmp(&b.created)
                .then_with(|| a.seq.cmp(&b.seq))
                .then_with(|| a.key.cmp(&b.key))
        })
        .map(|entry| entry.key.clone());
    if let Some(key) = &protected {
        keep_keys.insert(key.clone());
        buckets.entry(key.clone()).or_insert("keep");
    }
    let mut keep: Vec<ArchiveEntry> = entries
        .iter()
        .filter(|entry| keep_keys.contains(&entry.key))
        .cloned()
        .collect();
    let mut delete: Vec<ArchiveEntry> = entries
        .iter()
        .filter(|entry| !keep_keys.contains(&entry.key))
        .cloned()
        .collect();
    while sum_bytes(&keep) > policy.max_total_bytes {
        let Some(index) = victim_index(&keep, protected.as_deref()) else {
            break;
        };
        let removed = keep.remove(index);
        buckets.insert(removed.key.clone(), "delete");
        delete.push(removed);
    }
    (keep, delete, buckets)
}

#[derive(Debug, Clone)]
pub struct Retention {
    pub keep_daily: usize,
    pub keep_weekly: usize,
    pub keep_monthly: usize,
    pub keep_manual: usize,
    pub pre_restore_days: i64,
    pub max_total_bytes: u64,
}

fn retention(policy: &BackupPolicy) -> Retention {
    Retention {
        keep_daily: policy.keep_daily,
        keep_weekly: policy.keep_weekly,
        keep_monthly: policy.keep_monthly,
        keep_manual: policy.keep_manual,
        pre_restore_days: BACKUP_PRE_RESTORE_DAYS,
        max_total_bytes: policy.max_total_bytes(),
    }
}

fn gfs(
    entries: &[&ArchiveEntry],
    now: DateTime<Utc>,
    policy: &Retention,
    keep: &mut BTreeSet<String>,
    buckets: &mut HashMap<String, &'static str>,
) {
    let mut by_kind = |kind: Kind, limit: usize, label: &'static str| {
        let mut matching: Vec<&&ArchiveEntry> =
            entries.iter().filter(|entry| entry.kind == kind).collect();
        matching.sort_by(|a, b| b.created.cmp(&a.created).then(b.seq.cmp(&a.seq)));
        for entry in matching.into_iter().take(limit) {
            keep.insert(entry.key.clone());
            remember(buckets, &entry.key, label);
        }
    };
    by_kind(Kind::Daily, policy.keep_daily, "daily");
    by_kind(Kind::Manual, policy.keep_manual, "manual");
    let candidates: Vec<&&ArchiveEntry> = entries
        .iter()
        .filter(|entry| matches!(entry.kind, Kind::Daily | Kind::Manual))
        .collect();
    for week in last_weeks(now, policy.keep_weekly) {
        if let Some(best) = candidates
            .iter()
            .filter(|entry| week_key(entry.created) == week)
            .max_by(|a, b| a.created.cmp(&b.created).then(a.seq.cmp(&b.seq)))
        {
            keep.insert(best.key.clone());
            remember(buckets, &best.key, "weekly");
        }
    }
    for month in last_months(now, policy.keep_monthly) {
        if let Some(best) = candidates
            .iter()
            .filter(|entry| (entry.created.year(), entry.created.month()) == month)
            .max_by(|a, b| a.created.cmp(&b.created).then(a.seq.cmp(&b.seq)))
        {
            keep.insert(best.key.clone());
            remember(buckets, &best.key, "monthly");
        }
    }
    let horizon = now - ChronoDuration::days(policy.pre_restore_days);
    for entry in entries
        .iter()
        .filter(|entry| entry.kind == Kind::PreRestore)
    {
        if entry.created >= horizon {
            keep.insert(entry.key.clone());
            remember(buckets, &entry.key, "pre-restore");
        }
    }
}

fn remember(buckets: &mut HashMap<String, &'static str>, key: &str, label: &'static str) {
    let rank = |label: &str| match label {
        "monthly" => 5,
        "weekly" => 4,
        "daily" => 3,
        "manual" => 2,
        "pre-restore" => 1,
        _ => 0,
    };
    match buckets.get(key) {
        Some(old) if rank(old) >= rank(label) => {}
        _ => {
            buckets.insert(key.to_string(), label);
        }
    }
}

fn victim_index(keep: &[ArchiveEntry], protected: Option<&str>) -> Option<usize> {
    let eligible = |entry: &&ArchiveEntry| protected != Some(entry.key.as_str());
    let pick = |part: Part| {
        keep.iter()
            .enumerate()
            .filter(|(_, entry)| entry.part == part && eligible(entry))
            .min_by(|(_, a), (_, b)| a.created.cmp(&b.created).then(a.seq.cmp(&b.seq)))
            .map(|(index, _)| index)
    };
    pick(Part::Artifacts).or_else(|| pick(Part::Core))
}

fn sum_bytes(entries: &[ArchiveEntry]) -> u64 {
    entries
        .iter()
        .fold(0u64, |total, entry| total.saturating_add(entry.bytes))
}

fn last_weeks(now: DateTime<Utc>, count: usize) -> Vec<(i32, u32)> {
    let mut keys = Vec::new();
    let mut cursor = now;
    for _ in 0..count {
        let key = week_key(cursor);
        if !keys.contains(&key) {
            keys.push(key);
        }
        cursor -= ChronoDuration::days(7);
    }
    keys
}

fn last_months(now: DateTime<Utc>, count: usize) -> Vec<(i32, u32)> {
    let mut year = now.year();
    let mut month = now.month();
    let mut keys = Vec::new();
    for _ in 0..count {
        keys.push((year, month));
        if month == 1 {
            year -= 1;
            month = 12;
        } else {
            month -= 1;
        }
    }
    keys
}

fn week_key(time: DateTime<Utc>) -> (i32, u32) {
    let week = time.iso_week();
    (week.year(), week.week())
}

/// `restore_with` that proceeds only when `request.yes` is set.
pub fn restore(store: &Path, request: &RestoreRequest) -> Result<RestoreReport> {
    restore_with(store, request, &mut |_| Ok(request.yes))
}

/// Verify, check, then ask `confirm` with the pending report; nothing is
/// written (not even the pre-restore snapshot) unless it returns `true`.
pub fn restore_with(
    store: &Path,
    request: &RestoreRequest,
    confirm: &mut dyn FnMut(&RestoreReport) -> Result<bool>,
) -> Result<RestoreReport> {
    let _guard = DirLock::acquire(&request.dir, true)?.expect("blocking lock");
    let _clear_fail = FailGuard;
    let manifest = verify(&request.archive)?;
    if manifest.part == Part::Artifacts.as_str() {
        let projects: Vec<&ManifestProject> = manifest.projects.iter().collect();
        ensure_idle(store, &projects, request.force)?;
        let pending = RestoreReport {
            mode: "artifacts".into(),
            projects: projects.iter().map(|p| p.id.clone()).collect(),
            ..RestoreReport::default()
        };
        if !confirm(&pending)? {
            return Err(cancelled());
        }
        create_locked(
            store,
            &CreateRequest {
                dir: request.dir.clone(),
                kind: Kind::PreRestore,
                with_artifacts: false,
                policy: request.policy.clone(),
            },
            &_guard,
        )?;
        restore_artifacts(store, &manifest, &request.archive)?;
        return Ok(pending);
    }
    if manifest.part != Part::Core.as_str() {
        return Err(KanbanError::BackupVerifyFailed(
            "archive part is not core".into(),
        ));
    }
    if request.project.is_some() && request.mode == RestoreMode::Full {
        return Err(KanbanError::Invalid(
            "--project restores one board; it cannot be combined with --mode full".into(),
        ));
    }
    let selected = selected_projects(&manifest, request.project.as_deref())?;
    if request.mode == RestoreMode::Full && !request.force && store_has_tasks(store) {
        return Err(KanbanError::Invalid(
            "full restore would replace an existing board; pass --force".into(),
        ));
    }
    ensure_idle(store, &selected, request.force)?;
    let mut counts = diff_counts(store, &manifest, &selected)?;
    counts.mode = request.mode.as_str().to_string();
    counts.projects = selected.iter().map(|p| p.id.clone()).collect();
    if !confirm(&counts)? {
        return Err(cancelled());
    }
    create_locked(
        store,
        &CreateRequest {
            dir: request.dir.clone(),
            kind: Kind::PreRestore,
            with_artifacts: false,
            policy: request.policy.clone(),
        },
        &_guard,
    )?;
    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let job = store.join(format!(".restore-{stamp}"));
    let staged_root = job.join("staged");
    let old_root = job.join("old");
    fs::create_dir_all(&staged_root)?;
    let _job = TempDir::new(job.clone());
    extract(&request.archive, &staged_root)?;
    reparse_tree(&staged_root)?;
    prepare_staged(store, &staged_root, &selected, request)?;
    let swap_result: Result<()> = {
        let _store_lock = ProjectStore::at(store).lock()?;
        let mut board_guards = Vec::new();
        for project in &selected {
            let Some(root) = live_root(store, project)? else {
                continue;
            };
            let kanban = root.join(".kanban");
            if kanban.is_dir() || request.mode == RestoreMode::Full {
                if let Some(parent) = kanban.parent() {
                    fs::create_dir_all(parent)?;
                }
                board_guards.push(storage::Storage::new(&root).lock()?);
            }
        }
        // Do not create sync/ for a store or archive that has none. Full
        // restore of an archive that does include sync/ locks that directory.
        let staged_sync = staged_root.join("store/sync");
        let sync_guard = if store.join("sync").is_dir()
            || (request.mode == RestoreMode::Full && staged_sync.is_dir())
        {
            fs::create_dir_all(store.join("sync"))?;
            Some(lock_file_reentrant(&store.join("sync/.lock"))?)
        } else {
            None
        };
        let mut undo = Vec::new();
        let swapped = swap_all(
            store,
            &staged_root,
            &old_root,
            &selected,
            request,
            &mut undo,
        );
        if let Err(err) = swapped {
            rollback(&undo);
            return Err(err);
        }
        let _ = fs::remove_dir_all(&old_root);
        drop(sync_guard);
        drop(board_guards);
        drop(_store_lock);
        Ok(())
    };
    swap_result?;
    // Locks are gone. Reconcile restored files into the journal so a
    // tombstoned task is explicitly restored and wins LWW.
    sync::activate(store)?;
    Ok(counts)
}

fn cancelled() -> KanbanError {
    KanbanError::Invalid("restore cancelled; nothing was changed".into())
}

fn restore_artifacts(store: &Path, manifest: &Manifest, archive: &Path) -> Result<()> {
    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let job = store.join(format!(".restore-{stamp}"));
    let staged_root = job.join("staged");
    fs::create_dir_all(&staged_root)?;
    let _job = TempDir::new(job);
    extract(archive, &staged_root)?;
    for file in &manifest.files {
        let Some(dest) = artifact_dest(store, manifest, &file.path)? else {
            eprintln!(
                "warning: skipping {}: project data root is not available",
                file.path
            );
            continue;
        };
        let src = staged_root.join(&file.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dest)?;
        set_mode(&dest, file.mode)?;
    }
    Ok(())
}

fn selected_projects<'a>(
    manifest: &'a Manifest,
    only: Option<&str>,
) -> Result<Vec<&'a ManifestProject>> {
    if let Some(id) = only {
        let project = manifest
            .projects
            .iter()
            .find(|project| project.id == id)
            .ok_or_else(|| KanbanError::Invalid(format!("backup has no project {id}")))?;
        return Ok(vec![project]);
    }
    Ok(manifest.projects.iter().collect())
}

fn ensure_idle(store: &Path, projects: &[&ManifestProject], force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    for project in projects {
        let Some(root) = live_root(store, project)? else {
            continue;
        };
        let active = super::session::SessionManager::new(&root).list_active_sessions();
        if !active.is_empty() {
            return Err(KanbanError::BackupActiveSessions(format!(
                "project {} has {} active agent session(s); pass --force",
                project.id,
                active.len()
            )));
        }
    }
    Ok(())
}

fn store_has_tasks(store: &Path) -> bool {
    boards_of(store)
        .ok()
        .into_iter()
        .flatten()
        .any(|board| !task_ids(&board.data_root.join(".kanban/tasks")).is_empty())
}

fn diff_counts(
    store: &Path,
    manifest: &Manifest,
    selected: &[&ManifestProject],
) -> Result<RestoreReport> {
    // Counts come from the manifest paths, not a second extract.
    let mut tasks_added = 0;
    let mut tasks_replaced = 0;
    let mut tasks_removed = 0;
    let mut threads_added = 0;
    let mut threads_replaced = 0;
    let mut threads_removed = 0;
    for project in selected {
        let live = live_root(store, project)?.unwrap_or_else(|| store.join("missing"));
        let (added, replaced, removed) = diff_set(
            &task_ids(&live.join(".kanban/tasks")),
            &ids_in(manifest, &project.id, "/.kanban/tasks/", ".md"),
        );
        tasks_added += added;
        tasks_replaced += replaced;
        tasks_removed += removed;
        let (added, replaced, removed) = diff_set(
            &task_ids(&live.join(".kanban/threads")),
            &ids_in(manifest, &project.id, "/.kanban/threads/", ".yaml"),
        );
        threads_added += added;
        threads_replaced += replaced;
        threads_removed += removed;
    }
    Ok(RestoreReport {
        mode: "pending".into(),
        projects: Vec::new(),
        tasks_added,
        tasks_replaced,
        tasks_removed,
        threads_added,
        threads_replaced,
        threads_removed,
    })
}

fn diff_set(live: &BTreeSet<String>, staged: &BTreeSet<String>) -> (usize, usize, usize) {
    let added = staged.difference(live).count();
    let removed = live.difference(staged).count();
    let replaced = live.intersection(staged).count();
    (added, replaced, removed)
}

fn ids_in(manifest: &Manifest, project: &str, marker: &str, suffix: &str) -> BTreeSet<String> {
    let prefix = format!("boards/{project}");
    manifest
        .files
        .iter()
        .filter_map(|file| {
            let rest = file.path.strip_prefix(&prefix)?;
            if !rest.contains(marker) || !file.path.ends_with(suffix) {
                return None;
            }
            Path::new(&file.path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
        })
        .collect()
}

fn task_ids(dir: &Path) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|ext| ext == "md" || ext == "yaml" || ext == "yml")
            {
                if let Some(stem) = path.file_stem() {
                    ids.insert(stem.to_string_lossy().to_string());
                }
            }
        }
    }
    ids
}

fn prepare_staged(
    store: &Path,
    staged_root: &Path,
    selected: &[&ManifestProject],
    request: &RestoreRequest,
) -> Result<()> {
    for project in selected {
        let board = staged_root.join(format!("boards/{}/.kanban", project.id));
        for sub in BOARD_DIRS {
            fs::create_dir_all(board.join(sub))?;
        }
    }
    let config = staged_root.join("store/config.yaml");
    if config.is_file() {
        if request.mode == RestoreMode::Board && request.project.is_none() {
            let current = fs::read_to_string(store.join("config.yaml")).unwrap_or_default();
            let archived = fs::read_to_string(&config)?;
            let merged = merge_local_sections(&current, &archived)?;
            storage::atomic_write_text_raw(&config, &merged)?;
        } else if request.mode == RestoreMode::Full && !request.identity {
            strip_node(&config)?;
        }
    }
    if request.mode == RestoreMode::Full && !request.identity {
        for name in IDENTITY_FILES {
            let _ = fs::remove_file(staged_root.join("store/sync").join(name));
        }
    }
    Ok(())
}

fn strip_node(path: &Path) -> Result<()> {
    let text = fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(());
    }
    let mut value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text)?;
    if let Some(map) = value.as_mapping_mut() {
        map.remove(serde_yaml_ng::Value::String("node".into()));
    }
    storage::atomic_write_text_raw(path, &serde_yaml_ng::to_string(&value)?)
}

fn merge_local_sections(current: &str, archived: &str) -> Result<String> {
    let mut archived: serde_yaml_ng::Mapping = if archived.trim().is_empty() {
        serde_yaml_ng::Mapping::new()
    } else {
        serde_yaml_ng::from_str(archived)?
    };
    let current: serde_yaml_ng::Mapping = if current.trim().is_empty() {
        serde_yaml_ng::Mapping::new()
    } else {
        serde_yaml_ng::from_str(current)?
    };
    for section in sync::local_config_sections() {
        let key = serde_yaml_ng::Value::String((*section).to_string());
        archived.remove(&key);
        if let Some(value) = current.get(&key) {
            archived.insert(key, value.clone());
        }
    }
    Ok(serde_yaml_ng::to_string(&archived)?)
}

fn swap_all(
    store: &Path,
    staged_root: &Path,
    old_root: &Path,
    selected: &[&ManifestProject],
    request: &RestoreRequest,
    undo: &mut Vec<Undo>,
) -> Result<()> {
    for project in selected {
        let Some(root) = live_root(store, project)? else {
            eprintln!(
                "warning: skipping project {}: data root is not on this machine",
                project.id
            );
            continue;
        };
        if root.is_symlink() {
            return Err(KanbanError::Invalid(format!(
                "refusing to restore through symlink {}",
                root.display()
            )));
        }
        fs::create_dir_all(&root)?;
        let live_kanban = root.join(".kanban");
        let staged_kanban = staged_root.join(format!("boards/{}/.kanban", project.id));
        for sub in BOARD_DIRS {
            swap_tree(
                &live_kanban.join(sub),
                &staged_kanban.join(sub),
                &old_root.join(format!("boards/{}/{sub}", project.id)),
                undo,
            )?;
        }
        swap_file(
            &live_kanban.join("config.yaml"),
            &staged_kanban.join("config.yaml"),
            &old_root.join(format!("boards/{}/config.yaml", project.id)),
            undo,
        )?;
        let project_yaml = if project.external {
            staged_root.join(format!("boards/{}/project.yaml", project.id))
        } else {
            staged_root.join(format!("store/{}/project.yaml", project.data_root))
        };
        if request.mode == RestoreMode::Board {
            preserve_project_locals(&root.join("project.yaml"), &project_yaml)?;
        }
        swap_file(
            &root.join("project.yaml"),
            &project_yaml,
            &old_root.join(format!("projects/{}/project.yaml", project.id)),
            undo,
        )?;
        let removed = if project.external {
            staged_root.join(format!("boards/{}/project.yaml.removed", project.id))
        } else {
            staged_root.join(format!("store/{}/project.yaml.removed", project.data_root))
        };
        swap_file(
            &root.join("project.yaml.removed"),
            &removed,
            &old_root.join(format!("projects/{}/project.yaml.removed", project.id)),
            undo,
        )?;
    }
    if request.project.is_none() {
        swap_file(
            &store.join("config.yaml"),
            &staged_root.join("store/config.yaml"),
            &old_root.join("config.yaml"),
            undo,
        )?;
    }
    if request.mode == RestoreMode::Full {
        swap_sync(store, staged_root, old_root, request.identity, undo)?;
    }
    if let Ok(file) = File::open(store) {
        let _ = file.sync_all();
    }
    Ok(())
}

fn preserve_project_locals(live: &Path, staged: &Path) -> Result<()> {
    if !live.is_file() || !staged.is_file() {
        return Ok(());
    }
    let current: serde_yaml_ng::Value = serde_yaml_ng::from_str(&fs::read_to_string(live)?)?;
    let mut archived: serde_yaml_ng::Value = serde_yaml_ng::from_str(&fs::read_to_string(staged)?)?;
    let Some(archived_map) = archived.as_mapping_mut() else {
        return Ok(());
    };
    for key in sync::local_project_fields() {
        let yaml_key = serde_yaml_ng::Value::String((*key).to_string());
        archived_map.remove(&yaml_key);
        if let Some(value) = current.get(*key) {
            archived_map.insert(yaml_key, value.clone());
        }
    }
    storage::atomic_write_text_raw(staged, &serde_yaml_ng::to_string(&archived)?)
}

fn swap_sync(
    store: &Path,
    staged_root: &Path,
    old_root: &Path,
    identity: bool,
    undo: &mut Vec<Undo>,
) -> Result<()> {
    let staged = staged_root.join("store/sync");
    if !staged.exists() {
        return Ok(());
    }
    let live = store.join("sync");
    let kept = old_root.join("kept-identity");
    if !identity && live.is_dir() {
        fs::create_dir_all(&kept)?;
        for name in IDENTITY_FILES {
            let path = live.join(name);
            if path.is_file() && !path.is_symlink() {
                fs::copy(&path, kept.join(name))?;
            }
        }
    }
    swap_tree(&live, &staged, &old_root.join("sync"), undo)?;
    if !identity && live.is_dir() {
        for name in IDENTITY_FILES {
            let dest = live.join(name);
            let saved = kept.join(name);
            if saved.is_file() {
                let _ = fs::remove_file(&dest);
                fs::copy(&saved, &dest)?;
                set_mode(&dest, 0o600)?;
            } else if dest.is_file() {
                fs::remove_file(&dest)?;
            }
        }
    }
    Ok(())
}

fn swap_tree(live: &Path, staged: &Path, old: &Path, undo: &mut Vec<Undo>) -> Result<()> {
    if live.is_symlink() || staged.is_symlink() {
        return Err(KanbanError::Invalid(format!(
            "refusing to restore through symlink {}",
            live.display()
        )));
    }
    if live.exists() {
        if let Some(parent) = old.parent() {
            fs::create_dir_all(parent)?;
        }
        relocate(live, old, undo)?;
    }
    if staged.exists() {
        if let Some(parent) = live.parent() {
            fs::create_dir_all(parent)?;
        }
        relocate(staged, live, undo)?;
    }
    Ok(())
}

fn swap_file(live: &Path, staged: &Path, old: &Path, undo: &mut Vec<Undo>) -> Result<()> {
    if !staged.exists() && !live.exists() {
        return Ok(());
    }
    if live.is_symlink() || staged.is_symlink() {
        return Err(KanbanError::Invalid(format!(
            "refusing to restore through symlink {}",
            live.display()
        )));
    }
    if live.exists() {
        if let Some(parent) = old.parent() {
            fs::create_dir_all(parent)?;
        }
        relocate(live, old, undo)?;
    }
    if staged.exists() {
        if let Some(parent) = live.parent() {
            fs::create_dir_all(parent)?;
        }
        relocate(staged, live, undo)?;
    }
    Ok(())
}

fn relocate(from: &Path, to: &Path, undo: &mut Vec<Undo>) -> Result<()> {
    fs::rename(from, to)?;
    undo.push(Undo {
        from: to.to_path_buf(),
        to: from.to_path_buf(),
    });
    fail_point()
}

fn rollback(undo: &[Undo]) {
    for step in undo.iter().rev() {
        let _ = fs::rename(&step.from, &step.to);
    }
}

fn live_root(store: &Path, project: &ManifestProject) -> Result<Option<PathBuf>> {
    if project.external {
        let path = PathBuf::from(&project.data_root);
        if path.is_absolute() && path.parent().is_some_and(|parent| parent.is_dir()) {
            Ok(Some(path))
        } else {
            Ok(None)
        }
    } else if !safe_data_root(&project.data_root) {
        Err(KanbanError::BackupVerifyFailed(format!(
            "unsafe data root {}",
            project.data_root
        )))
    } else {
        Ok(Some(store.join(&project.data_root)))
    }
}

fn artifact_dest(store: &Path, manifest: &Manifest, rel: &str) -> Result<Option<PathBuf>> {
    let Some(rest) = rel.strip_prefix("boards/") else {
        return Ok(None);
    };
    let Some((id, rest)) = rest.split_once("/.kanban/") else {
        return Ok(None);
    };
    let Some(project) = manifest.projects.iter().find(|project| project.id == id) else {
        return Ok(None);
    };
    let Some(root) = live_root(store, project)? else {
        return Ok(None);
    };
    Ok(Some(root.join(".kanban").join(rest)))
}

fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive)?;
    let decoder = GzDecoder::new(file);
    let mut tar = Archive::new(decoder);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.to_string_lossy().to_string();
        let rel = safe_rel(&raw)?;
        if rel == "MANIFEST.json" {
            continue;
        }
        let path = dest.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&path)?;
        io::copy(&mut entry, &mut out)?;
        let mode = entry.header().mode().unwrap_or(0o644) & 0o777;
        set_mode(&path, mode)?;
    }
    Ok(())
}

fn reparse_tree(root: &Path) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if needs_parse(&rel) {
                parse_entry(&rel, &fs::read(&path)?)?;
            }
        }
    }
    Ok(())
}

/// Start a daily snapshot on a background thread when one is due.
///
/// Stores under the system temp directory are skipped: the test suite pumps
/// those, and an automatic snapshot must not escape into the user's state dir.
pub fn poke_auto(store: &Path) {
    if under_temp(store) {
        return;
    }
    let mut slot = AUTO_JOB.lock().unwrap_or_else(|err| err.into_inner());
    if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
        return;
    }
    let quiet = QUIET_UNTIL.lock().unwrap_or_else(|err| err.into_inner());
    if *quiet > Instant::now() {
        return;
    }
    drop(quiet);
    let store = store.to_path_buf();
    *slot = Some(std::thread::spawn(move || {
        let result = auto_if_due(&store);
        if let Ok(mut quiet) = QUIET_UNTIL.lock() {
            *quiet = Instant::now() + Duration::from_secs(15 * 60);
        }
        if let Err(err) = result {
            warn_once(&err.to_string());
        }
    }));
}

pub fn join_auto() {
    let handle = AUTO_JOB
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take();
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

pub fn auto_if_due(store: &Path) -> Result<bool> {
    let policy = ProjectStore::at(store)
        .load_global_config()?
        .backup_policy();
    if !policy.auto {
        return Ok(false);
    }
    let (policy, dir) = resolve_dir_with(store, policy, None)?;
    let Some(guard) = DirLock::try_acquire(&dir)? else {
        return Ok(false);
    };
    let entries = scan(&dir)?;
    let now = Utc::now();
    let interval = ChronoDuration::hours(i64::try_from(policy.interval_hours).unwrap_or(24));
    let core_due = !entries.iter().any(|entry| {
        entry.part == Part::Core
            && entry.kind == Kind::Daily
            && now.signed_duration_since(entry.created) < interval
    });
    if !core_due {
        return Ok(false);
    }
    let with_artifacts = artifacts_due(&entries, now, policy.artifacts);
    create_locked(
        store,
        &CreateRequest {
            dir,
            kind: Kind::Daily,
            with_artifacts,
            policy,
        },
        &guard,
    )?;
    Ok(true)
}

fn artifacts_due(entries: &[ArchiveEntry], now: DateTime<Utc>, policy: BackupArtifacts) -> bool {
    match policy {
        BackupArtifacts::Off => false,
        BackupArtifacts::Always => true,
        BackupArtifacts::Weekly => !entries.iter().any(|entry| {
            entry.part == Part::Artifacts
                && now.signed_duration_since(entry.created)
                    < ChronoDuration::days(BACKUP_ARTIFACT_WEEKLY_DAYS)
        }),
    }
}

fn warn_once(message: &str) {
    let mut seen = WARNED.lock().unwrap_or_else(|err| err.into_inner());
    if seen.insert(message.to_string()) {
        eprintln!(
            "{} warning: backup: {message}",
            super::timefmt::format(&super::timefmt::now())
        );
    }
}

static AUTO_JOB: LazyLock<Mutex<Option<std::thread::JoinHandle<()>>>> =
    LazyLock::new(|| Mutex::new(None));
static QUIET_UNTIL: LazyLock<Mutex<Instant>> =
    LazyLock::new(|| Mutex::new(Instant::now() - Duration::from_secs(1)));
static WARNED: LazyLock<Mutex<BTreeSet<String>>> = LazyLock::new(|| Mutex::new(BTreeSet::new()));

fn under_temp(path: &Path) -> bool {
    let tmp = std::env::temp_dir();
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let tmp = tmp.canonicalize().unwrap_or(tmp);
    path.starts_with(tmp)
}

fn boards_of(store: &Path) -> Result<Vec<Board>> {
    let mut boards = Vec::new();
    let projects = store.join("projects");
    let entries = match fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(boards),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if !path.is_dir() || path.is_symlink() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !path.join("project.yaml").is_file() {
            continue;
        }
        let name = project_name(&path.join("project.yaml"), id);
        boards.push(Board {
            id: id.to_string(),
            name,
            data_root_rel: format!("projects/{id}"),
            data_root: path,
            external: false,
        });
    }
    boards.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(boards)
}

fn project_name(path: &Path, id: &str) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return id.to_string();
    };
    let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) else {
        return id.to_string();
    };
    value
        .get("name")
        .and_then(|value| value.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(id)
        .to_string()
}

fn core_items(store: &Path, boards: &[Board]) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    push_file(
        store.join("config.yaml"),
        "store/config.yaml".into(),
        &mut items,
    )?;
    let projects = store.join("projects");
    if projects.is_dir() {
        for entry in fs::read_dir(&projects)? {
            let path = entry?.path();
            if !path.is_dir() || path.is_symlink() {
                continue;
            }
            let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            push_file(
                path.join("project.yaml"),
                format!("store/projects/{id}/project.yaml"),
                &mut items,
            )?;
            push_file(
                path.join("project.yaml.removed"),
                format!("store/projects/{id}/project.yaml.removed"),
                &mut items,
            )?;
        }
    }
    for board in boards {
        let kanban = board.data_root.join(".kanban");
        let prefix = format!("boards/{}/.kanban", board.id);
        push_file(
            kanban.join("config.yaml"),
            format!("{prefix}/config.yaml"),
            &mut items,
        )?;
        for (dir, leaf) in [
            ("tasks", "tasks"),
            ("threads", "threads"),
            ("instructions", "instructions"),
            ("context", "context"),
            ("stats", "stats"),
        ] {
            walk_files(
                &kanban.join(dir),
                &format!("{prefix}/{leaf}"),
                &mut items,
                None,
            )?;
        }
        walk_files(
            &kanban.join("assets/images"),
            &format!("{prefix}/assets/images"),
            &mut items,
            None,
        )?;
    }
    if store.join("sync").is_dir() {
        for name in SYNC_FILES {
            push_file(
                store.join("sync").join(name),
                format!("store/sync/{name}"),
                &mut items,
            )?;
        }
        for dir in SYNC_DIRS {
            walk_files(
                &store.join("sync").join(dir),
                &format!("store/sync/{dir}"),
                &mut items,
                (*dir == "hub").then_some("spool"),
            )?;
        }
    }
    items.sort_by(|a, b| a.archive_path.cmp(&b.archive_path));
    Ok(items)
}

fn artifact_items(store: &Path, boards: &[Board]) -> Result<Vec<Item>> {
    let _ = store;
    let mut items = Vec::new();
    for board in boards {
        let assets = board.data_root.join(".kanban/assets");
        walk_files(
            &assets,
            &format!("boards/{}/.kanban/assets", board.id),
            &mut items,
            Some("images"),
        )?;
    }
    items.sort_by(|a, b| a.archive_path.cmp(&b.archive_path));
    Ok(items)
}

fn walk_files(
    root: &Path,
    archive_prefix: &str,
    out: &mut Vec<Item>,
    skip_dir: Option<&str>,
) -> Result<()> {
    if !root.is_dir() || root.is_symlink() {
        return Ok(());
    }
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let mut children: Vec<_> = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for entry in children {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') || name.ends_with(".lock") {
                continue;
            }
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if skip_dir == Some(name) && dir == root {
                    continue;
                }
                dirs.push(path);
            } else if meta.is_file() {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                let rel = rel.to_string_lossy().replace('\\', "/");
                out.push(Item {
                    abs: path,
                    archive_path: format!("{archive_prefix}/{rel}"),
                });
            }
        }
    }
    Ok(())
}

fn push_file(abs: PathBuf, archive_path: String, out: &mut Vec<Item>) -> Result<()> {
    if abs.is_file() && !abs.is_symlink() {
        out.push(Item { abs, archive_path });
    }
    Ok(())
}

fn manifest_projects(boards: &[Board]) -> Vec<ManifestProject> {
    boards
        .iter()
        .map(|board| ManifestProject {
            id: board.id.clone(),
            name: board.name.clone(),
            data_root: board.data_root_rel.clone(),
            external: board.external,
        })
        .collect()
}

fn stage_copy(items: &[Item], staging: &Path) -> Result<Vec<Staged>> {
    let mut staged = Vec::new();
    for item in items {
        let dest = staging.join(&item.archive_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let meta = fs::symlink_metadata(&item.abs)?;
        if !meta.is_file() {
            continue;
        }
        fs::copy(&item.abs, &dest)?;
        let mode = file_mode(&meta);
        set_mode(&dest, mode)?;
        let (size, sha256) = hash_file(&dest)?;
        staged.push(Staged {
            archive_path: item.archive_path.clone(),
            path: dest,
            mode,
            size,
            sha256,
        });
    }
    Ok(staged)
}

fn stage_stable(items: &[Item], staging: &Path, skipped: &mut Vec<String>) -> Result<Vec<Staged>> {
    let mut staged = Vec::new();
    for item in items {
        let dest = staging.join(&item.archive_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        if !copy_stable(&item.abs, &dest)? {
            skipped.push(item.archive_path.clone());
            let _ = fs::remove_file(&dest);
            continue;
        }
        let meta = fs::symlink_metadata(&dest)?;
        let mode = file_mode(&fs::symlink_metadata(&item.abs).unwrap_or(meta));
        set_mode(&dest, mode)?;
        let (size, sha256) = hash_file(&dest)?;
        staged.push(Staged {
            archive_path: item.archive_path.clone(),
            path: dest,
            mode,
            size,
            sha256,
        });
    }
    Ok(staged)
}

fn copy_stable(src: &Path, dest: &Path) -> Result<bool> {
    let before = stamp(src)?;
    fs::copy(src, dest)?;
    let after = stamp(src)?;
    if before == after {
        return Ok(true);
    }
    let before = stamp(src)?;
    fs::copy(src, dest)?;
    let after = stamp(src)?;
    Ok(before == after)
}

fn stamp(path: &Path) -> Result<(u64, u128)> {
    let meta = fs::symlink_metadata(path)?;
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let nanos = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok((meta.len(), nanos))
}

fn manifest_for(
    store: &Path,
    kind: Kind,
    part: Part,
    with_artifacts: bool,
    projects: &[ManifestProject],
    files: &[Staged],
    skipped: &[String],
) -> Result<Manifest> {
    let mut skipped = skipped.to_vec();
    skipped.sort();
    Ok(Manifest {
        backup_format: BACKUP_FORMAT,
        version: env!("CARGO_PKG_VERSION").to_string(),
        sync_format: sync_format(store),
        created_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        node_id: node_id(store),
        kind: kind.as_str().to_string(),
        part: part.as_str().to_string(),
        with_artifacts,
        projects: projects.to_vec(),
        files: files
            .iter()
            .map(|file| ManifestFile {
                path: file.archive_path.clone(),
                size: file.size,
                sha256: file.sha256.clone(),
                mode: file.mode,
            })
            .collect(),
        skipped_changing: skipped,
    })
}

fn sync_format(store: &Path) -> Option<u32> {
    fs::read_to_string(store.join("sync/FORMAT"))
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

fn node_id(store: &Path) -> Option<String> {
    let text = fs::read_to_string(store.join("sync/node.yaml")).ok()?;
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).ok()?;
    value
        .get("node_id")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn pack(manifest: &Manifest, files: &[Staged], dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
        set_mode(parent, 0o700)?;
    }
    let partial = partial_path(dest);
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&partial)?;
    let written = (|| -> Result<()> {
        set_mode(&partial, 0o600)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        let body = serde_json::to_vec_pretty(manifest)
            .map_err(|err| KanbanError::Invalid(err.to_string()))?;
        append_bytes(&mut builder, "MANIFEST.json", &body, 0o600)?;
        for staged in files {
            let mut header = Header::new_gnu();
            header.set_size(staged.size);
            header.set_mode(staged.mode);
            // `append_data` writes a GNU long-name entry for paths past the
            // 100-byte header field (e.g. nested board image paths).
            builder.append_data(&mut header, &staged.archive_path, File::open(&staged.path)?)?;
        }
        let encoder = builder.into_inner()?;
        let file = encoder.finish()?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = written {
        let _ = fs::remove_file(&partial);
        return Err(err);
    }
    fs::rename(&partial, dest)?;
    set_mode(dest, 0o600)?;
    if let Some(parent) = dest.parent() {
        let _ = File::open(parent).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

fn append_bytes<W: Write>(
    builder: &mut Builder<W>,
    path: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    builder.append_data(&mut header, path, bytes)?;
    Ok(())
}

fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        total += read as u64;
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

fn scan(dir: &Path) -> Result<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
    let read = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(entries),
        Err(err) => return Err(err.into()),
    };
    for entry in read {
        let path = entry?.path();
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(parsed) = parse_archive_name(&name) else {
            continue;
        };
        let meta = match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => meta,
            _ => continue,
        };
        let verified = is_verified(&path);
        entries.push(ArchiveEntry {
            key: name,
            path,
            created: parsed.created,
            kind: parsed.kind,
            part: parsed.part,
            bytes: meta.len(),
            verified,
            seq: parsed.seq,
        });
    }
    Ok(entries)
}

struct ParsedName {
    created: DateTime<Utc>,
    kind: Kind,
    part: Part,
    seq: u32,
}

fn parse_archive_name(name: &str) -> Option<ParsedName> {
    let rest = name.strip_prefix("kanban-")?.strip_suffix(".tar.gz")?;
    let (rest, part) = if let Some(rest) = rest.strip_suffix("-artifacts") {
        (rest, Part::Artifacts)
    } else {
        (rest, Part::Core)
    };
    let (head, kind) = strip_kind(rest)?;
    let (stamp, seq) = split_seq(head)?;
    let created = NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S")
        .ok()?
        .and_utc();
    Some(ParsedName {
        created,
        kind,
        part,
        seq,
    })
}

fn strip_kind(value: &str) -> Option<(&str, Kind)> {
    for (suffix, kind) in [
        ("-pre-restore", Kind::PreRestore),
        ("-manual", Kind::Manual),
        ("-daily", Kind::Daily),
    ] {
        if let Some(head) = value.strip_suffix(suffix) {
            return Some((head, kind));
        }
    }
    None
}

fn split_seq(head: &str) -> Option<(&str, u32)> {
    if let Some((left, last)) = head.rsplit_once('-')
        && last.len() < 6
        && !last.is_empty()
        && last.chars().all(|ch| ch.is_ascii_digit())
        && left.len() == 15
    {
        return Some((left, last.parse().ok()?));
    }
    if head.len() == 15 {
        return Some((head, 0));
    }
    None
}

fn is_verified(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(text) = fs::read_to_string(verified_sidecar(path)) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let nanos = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    text.trim() == format!("{} {nanos}", meta.len())
}

fn mark_verified(path: &Path) -> Result<()> {
    let meta = fs::metadata(path)?;
    let nanos = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sidecar = verified_sidecar(path);
    storage::atomic_write_text_raw(&sidecar, &format!("{} {nanos}\n", meta.len()))?;
    set_mode(&sidecar, 0o600)?;
    Ok(())
}

fn verified_sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".verified");
    PathBuf::from(name)
}

fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".partial");
    PathBuf::from(name)
}

fn stale_scratch(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut doomed = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(doomed),
        Err(err) => return Err(err.into()),
    };
    let hour = Duration::from_secs(3600);
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let old = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > hour);
        if !old {
            continue;
        }
        if name.ends_with(".partial") || name.starts_with(".staging-") {
            doomed.push(path);
        }
    }
    Ok(doomed)
}

fn archive_name(dir: &Path, stamp: &str, kind: Kind, seq: u32, part: Part) -> PathBuf {
    let seq = if seq == 0 {
        String::new()
    } else {
        format!("-{seq}")
    };
    let artifacts = if part == Part::Artifacts {
        "-artifacts"
    } else {
        ""
    };
    // seq sits before kind so the parser can strip the kind suffix.
    // kanban-20261016-120000-1-manual.tar.gz
    dir.join(format!(
        "kanban-{stamp}{seq}-{kind}{artifacts}.tar.gz",
        kind = kind.as_str()
    ))
}

fn free_seq(dir: &Path, stamp: &str, kind: Kind) -> Result<u32> {
    for seq in 0..1000 {
        if !archive_name(dir, stamp, kind, seq, Part::Core).exists()
            && !archive_name(dir, stamp, kind, seq, Part::Artifacts).exists()
        {
            return Ok(seq);
        }
    }
    Err(KanbanError::Invalid(
        "too many backups in the same second".into(),
    ))
}

fn ensure_outside_store(store: &Path, dir: &Path) -> Result<()> {
    let store = canonical_existing(store);
    let dir = canonical_lexically(dir);
    if dir == store || dir.starts_with(&store) {
        return Err(KanbanError::BackupDirInsideStore(format!(
            "{}",
            dir.display()
        )));
    }
    Ok(())
}

fn canonical_existing(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn canonical_lexically(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}

fn safe_rel(path: &str) -> Result<String> {
    let path = path.trim_start_matches("./");
    if path.is_empty()
        || path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(KanbanError::BackupVerifyFailed(format!(
            "unsafe archive path {path}"
        )));
    }
    Ok(path.to_string())
}

fn safe_data_root(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn file_mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct FailGuard;

impl Drop for FailGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        FAIL_AFTER.with(|cell| cell.set(None));
    }
}

fn fail_point() -> Result<()> {
    #[cfg(test)]
    {
        let stop = FAIL_AFTER.with(|cell| match cell.get() {
            Some(0) => {
                cell.set(None);
                true
            }
            Some(n) => {
                cell.set(Some(n - 1));
                false
            }
            None => false,
        });
        if stop {
            return Err(KanbanError::Invalid("injected restore failure".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

impl DirLock {
    fn try_acquire(dir: &Path) -> Result<Option<Self>> {
        Self::acquire(dir, false)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::core::error::KanbanError;
    use crate::core::project::ProjectStore;
    use crate::core::session::SessionManager;
    use crate::core::storage::{self, NewTask};
    use crate::core::sync::{self, Journal};

    struct Fix {
        _root: tempfile::TempDir,
        store: PathBuf,
        dir: PathBuf,
        data_root: PathBuf,
        project_id: String,
    }

    fn fix() -> Fix {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let work = root.path().join("work");
        let dir = root.path().join("backups");
        fs::create_dir_all(&work).unwrap();
        let added = ProjectStore::at(&store).add(&work, Some("demo")).unwrap();
        storage::Storage::new(&added.project.data_root)
            .init_board()
            .unwrap();
        Fix {
            store,
            dir,
            data_root: added.project.data_root,
            project_id: added.project.id,
            _root: root,
        }
    }

    fn policy_at(dir: &Path) -> BackupPolicy {
        BackupPolicy {
            dir: Some(dir.to_path_buf()),
            ..BackupPolicy::default()
        }
    }

    fn request(dir: &Path, artifacts: bool) -> CreateRequest {
        CreateRequest {
            dir: dir.to_path_buf(),
            kind: Kind::Manual,
            with_artifacts: artifacts,
            policy: policy_at(dir),
        }
    }

    fn restore_of(
        fix: &Fix,
        archive: &Path,
        mode: RestoreMode,
        identity: bool,
        force: bool,
    ) -> RestoreRequest {
        RestoreRequest {
            archive: archive.to_path_buf(),
            dir: fix.dir.clone(),
            mode,
            project: None,
            identity,
            yes: true,
            force,
            policy: policy_at(&fix.dir),
        }
    }

    fn arm(store: &Path) {
        sync::enable_sync(store).unwrap();
        sync::activate(store).unwrap();
    }

    fn task_bytes(root: &Path, id: &str) -> Vec<u8> {
        let tasks = root.join(".kanban/tasks");
        for entry in fs::read_dir(&tasks).unwrap() {
            let dir = entry.unwrap().path();
            let path = dir.join(format!("{id}.md"));
            if path.is_file() {
                return fs::read(path).unwrap();
            }
        }
        panic!("missing task {id}");
    }

    fn allowlist(store: &Path) -> BTreeMap<String, Vec<u8>> {
        let boards = boards_of(store).unwrap();
        let mut out = BTreeMap::new();
        for item in core_items(store, &boards).unwrap() {
            out.insert(item.archive_path, fs::read(&item.abs).unwrap());
        }
        out
    }

    #[test]
    fn archive_names_round_trip() {
        let path = archive_name(
            Path::new("/tmp"),
            "20261016-120000",
            Kind::Manual,
            1,
            Part::Artifacts,
        );
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "kanban-20261016-120000-1-manual-artifacts.tar.gz");
        let parsed = parse_archive_name(name).unwrap();
        assert_eq!(parsed.seq, 1);
        assert_eq!(parsed.kind, Kind::Manual);
        assert_eq!(parsed.part, Part::Artifacts);
    }

    #[test]
    fn create_verify_restore_full_matches_allowlist() {
        let fix = fix();
        fs::write(fix.data_root.join(".kanban/logs/nope.txt"), b"log").unwrap();
        fs::write(fix.data_root.join(".kanban/assets/images/pic.png"), b"png").unwrap();
        fs::create_dir_all(fix.data_root.join(".kanban/assets")).unwrap();
        fs::write(fix.data_root.join(".kanban/assets/foo.csv"), b"a,b\n").unwrap();
        arm(&fix.store);
        let storage = storage::Storage::new(&fix.data_root);
        let task = storage.create_task(NewTask::titled("kept")).unwrap();
        let spool = fix.store.join("sync/hub/spool/in-flight.ndjson");
        fs::create_dir_all(spool.parent().unwrap()).unwrap();
        fs::write(&spool, b"unfinished snapshot\n").unwrap();
        let created = create(&fix.store, &request(&fix.dir, true)).unwrap();
        let manifest = verify(&created.core.path).unwrap();
        let paths: BTreeSet<_> = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("assets/images/pic.png"))
        );
        assert!(
            paths
                .iter()
                .all(|path| !path.ends_with("foo.csv") && !path.contains("/logs/"))
        );
        assert!(!paths.iter().any(|path| path.contains("/hub/spool/")));
        let art = verify(&created.artifacts.as_ref().unwrap().path).unwrap();
        assert!(art.files.iter().any(|file| file.path.ends_with("foo.csv")));
        assert!(art.files.iter().all(|file| !file.path.contains("/images/")));

        let before = allowlist(&fix.store);
        let empty = fix._root.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        restore(
            &empty,
            &restore_of(&fix, &created.core.path, RestoreMode::Full, true, false),
        )
        .unwrap();
        let after = allowlist(&empty);
        let mut differ = Vec::new();
        for (path, bytes) in &before {
            if path.ends_with("drift.jsonl") {
                continue;
            }
            if after.get(path) != Some(bytes) {
                differ.push(path.clone());
            }
        }
        assert!(differ.is_empty(), "allowlist mismatch: {differ:?}");
        assert_eq!(
            task_bytes(
                &empty.join(format!("projects/{}", fix.project_id)),
                &task.id
            ),
            task_bytes(&fix.data_root, &task.id)
        );
    }

    #[test]
    fn full_restore_identity_is_opt_in() {
        let fix = fix();
        arm(&fix.store);
        storage::Storage::new(&fix.data_root)
            .create_task(NewTask::titled("id"))
            .unwrap();
        let credentials = "node_id: server\ntoken: backup-secret\nurls: [http://127.0.0.1:7420]\n";
        let cursor = r#"{"server_node_id":"server","store_id":"store","hub_epoch":"epoch","last_seq":42,"pushed":7}"#;
        fs::write(fix.store.join("sync/server.yaml"), credentials).unwrap();
        fs::write(fix.store.join("sync/cursor.json"), cursor).unwrap();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        let old = node_id(&fix.store).unwrap();
        let kept = fix._root.path().join("kept");
        let fresh = fix._root.path().join("fresh");
        fs::create_dir_all(&kept).unwrap();
        fs::create_dir_all(&fresh).unwrap();
        restore(
            &kept,
            &restore_of(&fix, &created.core.path, RestoreMode::Full, true, false),
        )
        .unwrap();
        restore(
            &fresh,
            &restore_of(&fix, &created.core.path, RestoreMode::Full, false, false),
        )
        .unwrap();
        assert_eq!(node_id(&kept).unwrap(), old);
        assert_ne!(node_id(&fresh).unwrap(), old);
        assert_eq!(
            fs::read_to_string(kept.join("sync/server.yaml")).unwrap(),
            credentials
        );
        assert_eq!(
            fs::read_to_string(kept.join("sync/cursor.json")).unwrap(),
            cursor
        );
        assert!(!fresh.join("sync/server.yaml").exists());
        assert!(!fresh.join("sync/cursor.json").exists());
    }

    #[test]
    fn sync_off_create_and_restore_do_not_create_sync() {
        let fix = fix();
        let storage = storage::Storage::new(&fix.data_root);
        let task = storage.create_task(NewTask::titled("offline")).unwrap();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        assert!(!fix.store.join("sync").exists());
        storage.delete_task(&task.id).unwrap();
        assert!(!storage.task_exists(&task.id));
        restore(
            &fix.store,
            &restore_of(&fix, &created.core.path, RestoreMode::Board, false, false),
        )
        .unwrap();
        assert!(storage.task_exists(&task.id));
        assert!(!fix.store.join("sync").exists());
        let empty = fix._root.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        restore(
            &empty,
            &restore_of(&fix, &created.core.path, RestoreMode::Full, false, false),
        )
        .unwrap();
        assert!(!empty.join("sync").exists());
    }

    #[test]
    fn board_restore_revives_tombstone() {
        let fix = fix();
        arm(&fix.store);
        let storage = storage::Storage::new(&fix.data_root);
        let task = storage.create_task(NewTask::titled("gone")).unwrap();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        assert!(storage.delete_task(&task.id).unwrap());
        assert!(!storage.task_exists(&task.id));
        let report = restore(
            &fix.store,
            &restore_of(&fix, &created.core.path, RestoreMode::Board, false, false),
        )
        .unwrap();
        assert_eq!(report.mode, "board");
        assert!(
            storage.task_exists(&task.id),
            "restored task file is missing"
        );
        let journal = Journal::open(&fix.store).unwrap();
        let ops = journal.operations(None).unwrap();
        let tomb = ops
            .iter()
            .filter(|op| op.entity == "tombstone")
            .map(|op| op.hlc.clone())
            .max();
        let revived = ops
            .iter()
            .filter(|op| op.entity == "restore")
            .map(|op| op.hlc.clone())
            .max();
        let (Some(tomb), Some(revived)) = (tomb, revived) else {
            panic!("expected tombstone and restore ops, got {}", ops.len());
        };
        assert!(
            revived > tomb,
            "restore HLC {revived:?} is not newer than tombstone {tomb:?}"
        );
        let state = journal.state().unwrap();
        assert!(
            state.lifecycle.values().any(|life| {
                matches!((&life.restored, &life.deleted), (Some(restored), Some(deleted)) if restored > deleted)
            }),
            "projection lifecycle does not show the task as restored"
        );
    }

    #[test]
    fn board_restore_keeps_local_project_fields() {
        let fix = fix();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        let other = fix._root.path().join("elsewhere");
        fs::create_dir_all(&other).unwrap();
        let store = ProjectStore::at(&fix.store);
        store.rename(&fix.project_id, "renamed").unwrap();
        store.set_path(&fix.project_id, &other).unwrap();
        restore(
            &fix.store,
            &restore_of(&fix, &created.core.path, RestoreMode::Board, false, false),
        )
        .unwrap();
        let project = store
            .list()
            .unwrap()
            .into_iter()
            .find(|project| project.id == fix.project_id)
            .unwrap();
        assert_eq!(project.name, "demo");
        assert_eq!(project.work_path, other);
    }

    #[test]
    fn flipped_byte_refuses_restore() {
        let fix = fix();
        let storage = storage::Storage::new(&fix.data_root);
        let task = storage.create_task(NewTask::titled("stable")).unwrap();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        let before = task_bytes(&fix.data_root, &task.id);
        let mut bytes = fs::read(&created.core.path).unwrap();
        let index = bytes.len() / 2;
        bytes[index] ^= 0xff;
        fs::write(&created.core.path, &bytes).unwrap();
        let err = verify(&created.core.path).unwrap_err();
        assert!(matches!(err, KanbanError::BackupVerifyFailed(_)), "{err}");
        let err = restore(
            &fix.store,
            &restore_of(&fix, &created.core.path, RestoreMode::Board, false, false),
        )
        .unwrap_err();
        assert!(matches!(err, KanbanError::BackupVerifyFailed(_)), "{err}");
        assert_eq!(task_bytes(&fix.data_root, &task.id), before);
    }

    #[test]
    fn declined_restore_writes_nothing() {
        let fix = fix();
        let storage = storage::Storage::new(&fix.data_root);
        let task = storage.create_task(NewTask::titled("kept")).unwrap();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        storage
            .create_task(NewTask::titled("after backup"))
            .unwrap();
        let before = task_bytes(&fix.data_root, &task.id);
        let archives = fs::read_dir(&fix.dir).unwrap().count();
        let mut asked = None;
        let err = restore_with(
            &fix.store,
            &restore_of(&fix, &created.core.path, RestoreMode::Board, false, false),
            &mut |pending| {
                asked = Some(pending.clone());
                Ok(false)
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
        let asked = asked.expect("confirm was asked");
        assert_eq!(asked.mode, "board");
        assert_eq!((asked.tasks_replaced, asked.tasks_removed), (1, 1));
        assert_eq!(task_bytes(&fix.data_root, &task.id), before);
        assert_eq!(fs::read_dir(&fix.dir).unwrap().count(), archives);
        assert_eq!(task_ids(&fix.data_root.join(".kanban/tasks")).len(), 2);
    }

    #[test]
    fn paths_past_the_tar_name_field_round_trip() {
        let fix = fix();
        let images = fix.data_root.join(".kanban/assets/images");
        fs::create_dir_all(&images).unwrap();
        let name = format!("{}.png", "x".repeat(140));
        fs::write(images.join(&name), b"png").unwrap();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        let manifest = verify(&created.core.path).unwrap();
        assert!(
            manifest
                .files
                .iter()
                .any(|file| file.path.len() > 100 && file.path.ends_with(&name))
        );
        let leftovers: Vec<_> = fs::read_dir(&fix.dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".partial"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn newer_sync_format_is_refused() {
        let fix = fix();
        arm(&fix.store);
        storage::Storage::new(&fix.data_root)
            .create_task(NewTask::titled("fmt"))
            .unwrap();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        rewrite_sync_format(&created.core.path, sync::STORE_FORMAT + 1);
        let err = verify(&created.core.path).unwrap_err();
        assert!(matches!(err, KanbanError::BackupFormatNewer(_)), "{err}");
        let err = restore(
            &fix.store,
            &restore_of(&fix, &created.core.path, RestoreMode::Full, true, true),
        )
        .unwrap_err();
        assert!(matches!(err, KanbanError::BackupFormatNewer(_)), "{err}");
    }

    fn rewrite_sync_format(path: &Path, version: u32) {
        let input = fs::File::open(path).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(input));
        let mut items = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mode = entry.header().mode().unwrap_or(0o644);
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            items.push((name, mode, bytes));
        }
        for (name, _, bytes) in &mut items {
            if name == "MANIFEST.json" {
                let mut manifest: Manifest = serde_json::from_slice(bytes).unwrap();
                manifest.sync_format = Some(version);
                *bytes = serde_json::to_vec_pretty(&manifest).unwrap();
            }
        }
        let partial = path.with_extension("repack");
        let file = fs::File::create(&partial).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, mode, bytes) in &items {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder.append(&header, bytes.as_slice()).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        fs::rename(&partial, path).unwrap();
    }

    #[test]
    fn backup_dir_inside_store_is_refused() {
        let fix = fix();
        let err = resolve_dir(&fix.store, Some(&fix.store.join("inside"))).unwrap_err();
        assert!(matches!(err, KanbanError::BackupDirInsideStore(_)), "{err}");
    }

    #[test]
    fn backup_section_does_not_journal() {
        let fix = fix();
        arm(&fix.store);
        storage::Storage::new(&fix.data_root)
            .create_task(NewTask::titled("cfg"))
            .unwrap();
        let path = fix.store.join("config.yaml");
        let before = Journal::open(&fix.store)
            .unwrap()
            .operations(None)
            .unwrap()
            .len();
        let mut value: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let mut backup = serde_yaml_ng::Mapping::new();
        backup.insert(
            serde_yaml_ng::Value::String("auto".into()),
            serde_yaml_ng::Value::Bool(false),
        );
        backup.insert(
            serde_yaml_ng::Value::String("dir".into()),
            serde_yaml_ng::Value::String(fix.dir.display().to_string()),
        );
        value["backup"] = serde_yaml_ng::Value::Mapping(backup);
        storage::atomic_write_text(&path, &serde_yaml_ng::to_string(&value).unwrap()).unwrap();
        let ops = Journal::open(&fix.store).unwrap().operations(None).unwrap();
        assert_eq!(ops.len(), before);
        assert!(ops.iter().all(|op| {
            op.set
                .keys()
                .all(|key| !key.starts_with("backup") && key != "backup")
        }));
    }

    #[test]
    fn active_session_blocks_restore_until_force() {
        let fix = fix();
        let storage = storage::Storage::new(&fix.data_root);
        let task = storage.create_task(NewTask::titled("live")).unwrap();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        SessionManager::new(&fix.data_root)
            .link_session(&task.id, "ses-test")
            .unwrap();
        let err = restore(
            &fix.store,
            &restore_of(&fix, &created.core.path, RestoreMode::Board, false, false),
        )
        .unwrap_err();
        assert!(matches!(err, KanbanError::BackupActiveSessions(_)), "{err}");
        restore(
            &fix.store,
            &restore_of(&fix, &created.core.path, RestoreMode::Board, false, true),
        )
        .unwrap();
    }

    #[test]
    fn restore_rollback_leaves_the_board() {
        let fix = fix();
        let storage = storage::Storage::new(&fix.data_root);
        let task = storage.create_task(NewTask::titled("roll")).unwrap();
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        let before = task_bytes(&fix.data_root, &task.id);
        storage.delete_task(&task.id).unwrap();
        FAIL_AFTER.with(|cell| cell.set(Some(0)));
        let err = restore(
            &fix.store,
            &restore_of(&fix, &created.core.path, RestoreMode::Board, false, false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        assert!(!storage.task_exists(&task.id));
        assert!(task_bytes_missing_or_unchanged(
            &fix.data_root,
            &task.id,
            &before
        ));
    }

    fn task_bytes_missing_or_unchanged(root: &Path, id: &str, before: &[u8]) -> bool {
        let tasks = root.join(".kanban/tasks");
        let mut stack = vec![tasks];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().and_then(|name| name.to_str())
                    == Some(&format!("{id}.md"))
                {
                    return fs::read(&path).ok().as_deref() == Some(before);
                }
            }
        }
        true
    }

    #[test]
    fn concurrent_saves_still_verify() {
        let fix = fix();
        let storage = storage::Storage::new(&fix.data_root);
        let task = storage.create_task(NewTask::titled("race")).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let root = fix.data_root.clone();
        let worker = std::thread::spawn(move || {
            let storage = storage::Storage::new(&root);
            let mut task = task;
            let mut n = 0u32;
            while !flag.load(Ordering::Relaxed) {
                task.title = format!("race-{n}");
                let _ = storage.save_task(&task);
                n += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        std::thread::sleep(Duration::from_millis(30));
        let created = create(&fix.store, &request(&fix.dir, false)).unwrap();
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        let manifest = verify(&created.core.path).unwrap();
        assert!(
            manifest
                .files
                .iter()
                .any(|file| file.path.contains("/.kanban/tasks/"))
        );
    }

    #[test]
    fn auto_if_due_is_once_a_day_and_artifacts_weekly() {
        let fix = fix();
        storage::Storage::new(&fix.data_root)
            .create_task(NewTask::titled("auto"))
            .unwrap();
        let config = format!(
            "backup:\n  auto: true\n  interval_hours: 24\n  dir: {}\n  artifacts: weekly\n  max_total_mb: 2048\n",
            fix.dir.display()
        );
        fs::create_dir_all(&fix.store).unwrap();
        fs::write(fix.store.join("config.yaml"), config).unwrap();
        assert!(auto_if_due(&fix.store).unwrap());
        assert_eq!(archives(&fix.dir).len(), 2);
        assert!(!auto_if_due(&fix.store).unwrap());
        assert_eq!(archives(&fix.dir).len(), 2);
        let stamp = (Utc::now() - chrono::Duration::days(2))
            .format("%Y%m%d-%H%M%S")
            .to_string();
        for path in archives(&fix.dir) {
            if !path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("-artifacts")
            {
                restamp(&path, &stamp);
            }
        }
        assert!(auto_if_due(&fix.store).unwrap());
        let names = archive_names(&fix.dir);
        assert_eq!(
            names
                .iter()
                .filter(|name| name.contains("-daily.tar.gz") || name.ends_with("-daily.tar.gz"))
                .count(),
            2
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| name.contains("-artifacts"))
                .count(),
            1
        );
    }

    fn archives(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".tar.gz"))
            })
            .collect()
    }

    fn archive_names(dir: &Path) -> Vec<String> {
        archives(dir)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn restamp(path: &Path, stamp: &str) {
        let name = path.file_name().unwrap().to_str().unwrap();
        let rest = &name["kanban-".len() + 15..];
        fs::rename(path, path.with_file_name(format!("kanban-{stamp}{rest}"))).unwrap();
    }

    #[test]
    fn select_keep_gfs_size_and_verified() {
        let now = Utc.with_ymd_and_hms(2026, 10, 16, 12, 0, 0).unwrap();
        let mut entries = Vec::new();
        for offset in 0..40 {
            entries.push(fake(
                &format!("core-daily-{offset}"),
                now - chrono::Duration::days(offset),
                Kind::Daily,
                Part::Core,
                1,
                true,
                0,
            ));
            entries.push(fake(
                &format!("art-daily-{offset}"),
                now - chrono::Duration::days(offset),
                Kind::Daily,
                Part::Artifacts,
                1,
                true,
                0,
            ));
        }
        for offset in 0..12 {
            entries.push(fake(
                &format!("core-manual-{offset}"),
                now - chrono::Duration::days(offset) + chrono::Duration::hours(6),
                Kind::Manual,
                Part::Core,
                1,
                true,
                0,
            ));
        }
        entries.push(fake(
            "pre-new",
            now - chrono::Duration::days(1),
            Kind::PreRestore,
            Part::Core,
            1,
            true,
            0,
        ));
        entries.push(fake(
            "pre-old",
            now - chrono::Duration::days(8),
            Kind::PreRestore,
            Part::Core,
            1,
            true,
            0,
        ));
        let policy = Retention {
            keep_daily: 7,
            keep_weekly: 4,
            keep_monthly: 6,
            keep_manual: 10,
            pre_restore_days: 7,
            max_total_bytes: u64::MAX,
        };
        let (keep, delete, buckets) = select_keep(&entries, now, &policy);
        let kept: BTreeSet<_> = keep.iter().map(|entry| entry.key.as_str()).collect();
        for offset in [0, 1, 2, 3, 4, 5, 6, 12, 16, 19] {
            assert!(
                kept.contains(format!("core-daily-{offset}").as_str()),
                "missing daily {offset}"
            );
            assert!(kept.contains(format!("art-daily-{offset}").as_str()));
        }
        assert!(!kept.contains("core-daily-7"));
        assert!(!kept.contains("core-daily-39"));
        for offset in 0..10 {
            assert!(kept.contains(format!("core-manual-{offset}").as_str()));
        }
        assert!(!kept.contains("core-manual-10"));
        assert!(kept.contains("pre-new"));
        assert!(!kept.contains("pre-old"));
        assert!(delete.iter().any(|entry| entry.key == "pre-old"));
        assert_eq!(buckets.get("core-daily-12").copied(), Some("weekly"));
        assert_eq!(buckets.get("core-daily-19").copied(), Some("weekly"));
        assert_eq!(buckets.get("core-daily-16").copied(), Some("monthly"));

        let mut sized = vec![
            fake(
                "new-unverified",
                now,
                Kind::Daily,
                Part::Core,
                500,
                false,
                2,
            ),
            fake(
                "old-verified",
                now - chrono::Duration::days(1),
                Kind::Daily,
                Part::Core,
                500,
                true,
                1,
            ),
            fake(
                "old-art",
                now - chrono::Duration::days(1),
                Kind::Daily,
                Part::Artifacts,
                500,
                true,
                1,
            ),
        ];
        let tight = Retention {
            keep_daily: 7,
            keep_weekly: 0,
            keep_monthly: 0,
            keep_manual: 0,
            pre_restore_days: 7,
            max_total_bytes: 500,
        };
        let (keep, _, _) = select_keep(&sized, now, &tight);
        let keys: BTreeSet<_> = keep.iter().map(|entry| entry.key.as_str()).collect();
        assert!(keys.contains("old-verified"));
        assert!(!keys.contains("new-unverified"));
        assert!(!keys.contains("old-art"));
        sized.retain(|entry| entry.key != "new-unverified");
        sized.push(fake(
            "newer-verified",
            now,
            Kind::Daily,
            Part::Core,
            500,
            true,
            3,
        ));
        let (keep, _, _) = select_keep(&sized, now, &tight);
        let keys: BTreeSet<_> = keep.iter().map(|entry| entry.key.as_str()).collect();
        assert!(keys.contains("newer-verified"));
        assert!(!keys.contains("old-verified"));
    }

    fn fake(
        key: &str,
        created: chrono::DateTime<Utc>,
        kind: Kind,
        part: Part,
        bytes: u64,
        verified: bool,
        seq: u32,
    ) -> ArchiveEntry {
        ArchiveEntry {
            key: key.to_string(),
            path: PathBuf::from(format!("/tmp/{key}")),
            created,
            kind,
            part,
            bytes,
            verified,
            seq,
        }
    }

    #[test]
    fn poke_auto_skips_temp_stores() {
        let fix = fix();
        poke_auto(&fix.store);
        join_auto();
        assert!(!fix.dir.exists() || archives(&fix.dir).is_empty());
    }
}
