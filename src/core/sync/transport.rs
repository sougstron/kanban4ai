//! Paged journal reads and bounded-memory NDJSON replica installation.
use super::*;
use crate::core::project::ProjectStore;
use crate::core::storage::{BoardGuard, Storage};
use sha2::{Digest, Sha256};
use std::io::BufWriter;

/// One page of operations a peer lacks. `next` is the paging cursor: per
/// node, the highest sequence included so far.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Page {
    pub operations: Vec<Operation>,
    pub next: VersionVector,
    pub more: bool,
}

const BOARD_TREES: [&str; 2] = ["instructions", "assets/images"];
const STATUSES: [&str; 5] = ["todo", "in_progress", "review", "done", "archive"];

impl Journal {
    pub fn vector(&self) -> Result<VersionVector> {
        Ok(self.state()?.vector)
    }

    /// Project alias -> replicated UID.
    pub fn project_uids(&self) -> Result<BTreeMap<String, String>> {
        Ok(self.state()?.project_uids)
    }

    /// Operations newer than `after`, oldest HLC first. A node's HLC grows
    /// with its sequence, so each node's slice of a page is a sequence prefix.
    /// Tombstones lead the page (section 10.2).
    pub fn operations_after(&self, after: &VersionVector, limit: usize) -> Result<Page> {
        let state = self.state()?;
        for (node, compacted) in &state.compacted {
            if *compacted > 0 && after.get(node).copied().unwrap_or(0) < *compacted {
                return Err(invalid("SYNC_SNAPSHOT_REQUIRED"));
            }
        }
        let limit = limit.max(1);
        let nodes = segments::node_names(&self.root)?;
        // k-way merge by HLC. A node's clock grows with its sequence, so each
        // cursor is read only until the page is full. Ahead-of-sequence ops
        // are not part of the vector.
        let mut cursors = Vec::with_capacity(nodes.len());
        let mut heads = Vec::with_capacity(nodes.len());
        for node in &nodes {
            let mut cursor =
                segments::Cursor::open(&self.root, node, after.get(node).copied().unwrap_or(0))?;
            heads.push(cursor.next_op()?);
            cursors.push(cursor);
        }
        let mut ops = Vec::new();
        while ops.len() <= limit {
            let mut best: Option<usize> = None;
            for (index, head) in heads.iter().enumerate() {
                let Some(op) = head else {
                    continue;
                };
                let older = match best {
                    None => true,
                    Some(chosen) => {
                        let other = heads[chosen].as_ref().expect("chosen head");
                        op.hlc < other.hlc || (op.hlc == other.hlc && op.op < other.op)
                    }
                };
                if older {
                    best = Some(index);
                }
            }
            let Some(slot) = best else {
                break;
            };
            let op = heads[slot].take().expect("head");
            ops.push(op);
            heads[slot] = cursors[slot].next_op()?;
        }
        let more = ops.len() > limit;
        ops.truncate(limit);
        let mut next = after.clone();
        for op in &ops {
            let seq = sequence(op)?;
            let entry = next.entry(op.hlc.2.clone()).or_default();
            *entry = (*entry).max(seq);
        }
        ops.sort_by_key(|op| op.entity != "tombstone");
        Ok(Page {
            operations: ops,
            next,
            more,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotHeader {
    pub format: u32,
    pub store_id: String,
    pub node_id: String,
    pub hub_epoch: String,
    pub seq: u64,
    pub hlc: Hlc,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Record {
    Header {
        #[serde(flatten)]
        header: SnapshotHeader,
    },
    File {
        path: String,
        text: String,
    },
    Blob {
        path: String,
        sha256: String,
        size: u64,
    },
    End {
        files: u64,
    },
}

fn record(writer: &mut impl Write, value: &Record) -> Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(|e| invalid(&e.to_string()))?;
    writer.write_all(b"\n")?;
    Ok(())
}
fn relative(store: &Path, path: &Path) -> Result<String> {
    let name = path
        .strip_prefix(store)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| invalid("invalid snapshot path"))?;
    allowed_path(name)?;
    Ok(name.to_owned())
}
fn blob_path(path: &str) -> bool {
    path.starts_with("sync/blobs/")
        || path
            .split('/')
            .skip(2)
            .collect::<Vec<_>>()
            .starts_with(&[".kanban", "instructions"])
        || path
            .split('/')
            .skip(2)
            .collect::<Vec<_>>()
            .starts_with(&[".kanban", "assets", "images"])
}
fn file_hash(path: &Path) -> Result<(String, u64)> {
    no_symlinks(path)?;
    let mut input = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size += count as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}
fn hash_valid(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
/// Validate an existing cached blob without allocating its contents.
pub fn has_blob(store: &Path, hash: &str) -> Result<bool> {
    if !hash_valid(hash) {
        return Err(invalid("invalid blob hash"));
    }
    let path = store.join("sync/blobs").join(hash);
    match file_hash(&path) {
        Ok((actual, _)) => Ok(actual == hash),
        Err(KanbanError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
/// Verify and persist a separately transferred blob without loading it into memory.
pub fn put_blob_file(store: &Path, hash: &str, source: &Path) -> Result<()> {
    if !hash_valid(hash) || file_hash(source)?.0 != hash {
        return Err(invalid("blob hash mismatch"));
    }
    let target = store.join("sync/blobs").join(hash);
    if target.is_file() && file_hash(&target)?.0 == hash {
        return Ok(());
    }
    copy_atomic(source, &target)
}
fn copy_atomic(source: &Path, target: &Path) -> Result<()> {
    no_symlinks(source)?;
    no_symlinks(target)?;
    let parent = target
        .parent()
        .ok_or_else(|| invalid("missing file parent"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".tmp_")
        .tempfile_in(parent)?;
    std::io::copy(&mut fs::File::open(source)?, &mut temporary)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(target)
        .map_err(|e| KanbanError::Io(e.error))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Capture one stable replica into an owned spool; HTTP never holds store locks.
pub fn snapshot_spool(store: &Path) -> Result<tempfile::NamedTempFile> {
    let journal = Journal::open(store)?;
    journal.reconcile()?;
    apply::recover_remote(&journal.root)?;
    let _store = lock_file_reentrant(&store.join(".lock"))?;
    let boards = boards(store)?;
    let _boards: Vec<BoardGuard> = boards
        .iter()
        .map(|p| Storage::new(p).lock())
        .collect::<Result<_>>()?;
    let _sync = lock_file_reentrant(&journal.root.join(".lock"))?;
    journal.recover()?;
    let (mut state, prune_meta) = projection::load_state(&journal.root)?;
    for board in &boards {
        if board.join("project.yaml").is_file() {
            let alias = board
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("invalid project"))?;
            if !state.project_uids.contains_key(alias) {
                state.project_uids.insert(alias.into(), uuid()?);
            }
        }
    }
    projection::save_uids(&journal.root, &state)?;
    projection::checkpoint(&journal.root, &state, &prune_meta, true)?;
    let hub = super::hub::Hub::open(store)?;
    let seq = hub.ingest()?;
    let header = SnapshotHeader {
        format: STORE_FORMAT,
        store_id: journal.node.store_id.clone(),
        node_id: journal.node.node_id.clone(),
        hub_epoch: hub.epoch()?,
        seq,
        hlc: journal.clock()?,
    };
    let dir = journal.root.join("hub/spool");
    fs::create_dir_all(&dir)?;
    let mut spool = tempfile::Builder::new()
        .prefix("snapshot-")
        .suffix(".ndjson")
        .tempfile_in(&dir)?;
    let mut writer = BufWriter::new(spool.as_file_mut());
    record(&mut writer, &Record::Header { header })?;
    let mut files = 0u64;
    let mut hashes = BTreeSet::new();
    for board in &boards {
        for path in board_files(board)? {
            let name = relative(store, &path)?;
            if blob_path(&name) {
                let (hash, size) = file_hash(&path)?;
                put_blob_file(store, &hash, &path)?;
                hashes.insert(hash.clone());
                record(
                    &mut writer,
                    &Record::Blob {
                        path: name,
                        sha256: hash,
                        size,
                    },
                )?;
            } else {
                let text = fs::read_to_string(&path)?;
                let text = if let Some(t) =
                    target(&path).filter(|t| matches!(t.entity, "task" | "project"))
                {
                    replicated(&text, t.entity, false)?
                } else {
                    text
                };
                record(&mut writer, &Record::File { path: name, text })?;
            }
            files += 1;
        }
    }
    let config = store.join("config.yaml");
    let text = if config.is_file() {
        replicated(&fs::read_to_string(config)?, "config", true)?
    } else {
        "{}\n".into()
    };
    record(
        &mut writer,
        &Record::File {
            path: "config.yaml".into(),
            text,
        },
    )?;
    files += 1;
    for path in walk(&journal.root.join("meta"))? {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(
            name,
            "index.json" | "state.json" | "since.json" | "relay-ack.json"
        ) {
            continue;
        }
        record(
            &mut writer,
            &Record::File {
                path: relative(store, &path)?,
                text: fs::read_to_string(path)?,
            },
        )?;
        files += 1;
    }
    for node in segments::node_names(&journal.root)? {
        for path in read_files(&journal.root.join("journal").join(&node))? {
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            for line in text.lines().filter(|line| !line.is_empty()) {
                let op: Operation =
                    serde_json::from_str(line).map_err(|e| invalid(&e.to_string()))?;
                if let Some(hash) = op.blob_hash() {
                    hashes.insert(hash.to_owned());
                }
            }
            record(
                &mut writer,
                &Record::File {
                    path: relative(store, &path)?,
                    text,
                },
            )?;
            files += 1;
        }
    }
    for (key, fields) in &state.fields {
        if serde_json::from_str::<(String, String, String)>(key)
            .is_ok_and(|(_, entity, _)| entity == "file")
        {
            if let Some(hash) = fields.values.get("blob").and_then(Value::as_str) {
                hashes.insert(hash.to_owned());
            }
        }
    }
    for hash in hashes {
        let path = journal.root.join("blobs").join(&hash);
        let (actual, size) = file_hash(&path)?;
        if actual != hash {
            return Err(invalid("blob hash mismatch"));
        }
        record(
            &mut writer,
            &Record::Blob {
                path: format!("sync/blobs/{hash}"),
                sha256: hash,
                size,
            },
        )?;
        files += 1;
    }
    record(&mut writer, &Record::End { files })?;
    writer.flush()?;
    drop(writer);
    spool.as_file().sync_all()?;
    Ok(spool)
}

/// Validated on-disk staging; dropping before installation removes the partial copy.
pub struct StagedSnapshot {
    pub header: SnapshotHeader,
    pub blobs: Vec<String>,
    stage: tempfile::TempDir,
}
fn validate_header(header: &SnapshotHeader) -> Result<()> {
    if header.format > STORE_FORMAT {
        return Err(invalid("SYNC_STORE_TOO_NEW: upgrade kanban4ai"));
    }
    if header.format != STORE_FORMAT {
        return Err(invalid(
            "SYNC_PROTOCOL_MISMATCH: snapshot format is obsolete",
        ));
    }
    for value in [&header.store_id, &header.node_id, &header.hub_epoch] {
        component(value)?;
    }
    Ok(())
}
pub fn stage_snapshot(store: &Path, input: &Path) -> Result<StagedSnapshot> {
    let base = store.join("sync/.bootstrap");
    no_symlinks(&base)?;
    fs::create_dir_all(&base)?;
    let base = fs::canonicalize(&base)?;
    let stage = tempfile::Builder::new()
        .prefix("stage-")
        .tempdir_in(&base)?;
    let mut lines = BufReader::new(fs::File::open(input)?).lines();
    let first = lines
        .next()
        .ok_or_else(|| invalid("snapshot header is missing"))??;
    let header =
        match serde_json::from_str::<Record>(&first).map_err(|e| invalid(&e.to_string()))? {
            Record::Header { header } => header,
            _ => return Err(invalid("snapshot must begin with a header")),
        };
    validate_header(&header)?;
    let mut manifest = BufWriter::new(fs::File::create(stage.path().join("manifest.ndjson"))?);
    record(
        &mut manifest,
        &Record::Header {
            header: header.clone(),
        },
    )?;
    let mut count = 0u64;
    let mut blobs = BTreeSet::new();
    let mut ended = false;
    for line in lines {
        let line = line?;
        if ended {
            return Err(invalid("snapshot contains data after end"));
        }
        let item: Record = serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
        match &item {
            Record::Header { .. } => return Err(invalid("duplicate snapshot header")),
            Record::End { files } => {
                if *files != count {
                    return Err(invalid("snapshot file count mismatch"));
                }
                ended = true;
            }
            Record::File { path, text } => {
                allowed_path(path)?;
                if blob_path(path) {
                    return Err(invalid("binary snapshot paths require blob references"));
                }
                if path.starts_with("sync/journal/") {
                    for line in text.lines().filter(|line| !line.is_empty()) {
                        let op: Operation =
                            serde_json::from_str(line).map_err(|e| invalid(&e.to_string()))?;
                        if op.store != header.store_id {
                            return Err(invalid(
                                "SYNC_STORE_MISMATCH: snapshot journal identity differs",
                            ));
                        }
                        if op.v > OP_FORMAT {
                            return Err(invalid("SYNC_OP_FORMAT_TOO_NEW"));
                        }
                    }
                }
                stage_unique(stage.path(), path)?;
                write_bytes(&stage.path().join("data").join(path), text.as_bytes())?;
                count += 1;
            }
            Record::Blob {
                path,
                sha256,
                size: _,
            } => {
                allowed_path(path)?;
                if !blob_path(path) || !hash_valid(sha256) {
                    return Err(invalid("invalid snapshot blob reference"));
                }
                if let Some(hash) = path.strip_prefix("sync/blobs/") {
                    if hash != sha256 {
                        return Err(invalid("snapshot blob path/hash mismatch"));
                    }
                }
                stage_unique(stage.path(), path)?;
                write_bytes(&stage.path().join("refs").join(path), b"")?;
                blobs.insert(sha256.clone());
                count += 1;
            }
        }
        record(&mut manifest, &item)?;
    }
    if !ended {
        return Err(invalid("snapshot end record is missing"));
    }
    manifest.flush()?;
    manifest.get_ref().sync_all()?;
    drop(manifest);
    Ok(StagedSnapshot {
        header,
        blobs: blobs.into_iter().collect(),
        stage,
    })
}
fn stage_unique(stage: &Path, relative: &str) -> Result<()> {
    if stage.join("data").join(relative).exists() || stage.join("refs").join(relative).exists() {
        return Err(invalid("duplicate snapshot path"));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Bootstrap {
    stage: String,
    header: SnapshotHeader,
}
fn manifest(stage: &Path) -> Result<impl Iterator<Item = Result<Record>>> {
    let input = fs::File::open(stage.join("manifest.ndjson"))?;
    Ok(BufReader::new(input)
        .lines()
        .map(|line| serde_json::from_str(&line?).map_err(|e| invalid(&e.to_string()))))
}
/// Read only the compacted vector and segment heads, without loading task shards.
fn retained_vector(sync: &Path) -> Result<VersionVector> {
    #[derive(Default, Deserialize)]
    struct Floor {
        #[serde(default)]
        compacted: VersionVector,
    }
    let head = projection::head_path(sync);
    let floor: Floor = match fs::read_to_string(head) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| invalid(&e.to_string()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let path = sync.join("meta/projection.json");
            match fs::read_to_string(path) {
                Ok(text) => serde_json::from_str(&text).map_err(|e| invalid(&e.to_string()))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Floor::default(),
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) => return Err(e.into()),
    };
    segments::heads(sync, &floor.compacted)
}
fn affected_boards(store: &Path, stage: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut all: BTreeSet<_> = boards(store)?.into_iter().collect();
    for item in manifest(stage)? {
        let relative = match item? {
            Record::File { path, .. } | Record::Blob { path, .. } => path,
            _ => continue,
        };
        if let Some(rest) = relative.strip_prefix("projects/") {
            let alias = rest
                .split('/')
                .next()
                .ok_or_else(|| invalid("invalid project path"))?;
            component(alias)?;
            all.insert(store.join("projects").join(alias));
        }
    }
    for board in &all {
        no_symlinks(board)?;
    }
    Ok(all)
}
fn local_file(
    store: &Path,
    relative: &str,
    remote: &projection::Uids,
    local: &projection::Uids,
) -> PathBuf {
    let parts: Vec<_> = relative.split('/').collect();
    if parts.first() != Some(&"projects") || parts.len() < 3 {
        return store.join(relative);
    }
    let alias = parts[1];
    let project = remote
        .project_uids
        .get(alias)
        .map(String::as_str)
        .unwrap_or(alias);
    let local_alias = local
        .project_uids
        .iter()
        .find(|(_, uid)| uid.as_str() == project)
        .map(|(alias, _)| alias.as_str())
        .unwrap_or(alias);
    let board = store.join("projects").join(local_alias);
    if let [_, _, ".kanban", "tasks", _, file] = parts.as_slice() {
        let display = file.trim_end_matches(".md");
        let key = serde_json::to_string(&(project, "task", display)).unwrap_or_default();
        let uid = remote
            .task_uids
            .get(&key)
            .map(String::as_str)
            .unwrap_or(display);
        let local_display = local
            .task_uids
            .iter()
            .find_map(|(key, value)| {
                let (p, entity, display): (String, String, String) =
                    serde_json::from_str(key).ok()?;
                (p == project && entity == "task" && value == uid).then_some(display)
            })
            .unwrap_or_else(|| display.to_owned());
        for status in STATUSES {
            let path = board
                .join(".kanban/tasks")
                .join(status)
                .join(format!("{local_display}.md"));
            if path.is_file() {
                return path;
            }
        }
    }
    board.join(parts[2..].join("/"))
}
fn merge_file(local: &Path, remote: &str, entity: &str) -> Result<String> {
    let mut value = parse(remote, entity)?;
    let local_value = match fs::read_to_string(local) {
        Ok(text) => Some(parse(&text, entity)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let map = value
        .as_object_mut()
        .ok_or_else(|| invalid("snapshot file is not a mapping"))?;
    for field in local_fields(entity, false) {
        map.remove(*field);
        if let Some(old) = local_value.as_ref().and_then(|v| v.get(*field)) {
            map.insert((*field).into(), old.clone());
        }
    }
    if entity == "project" {
        map.entry("work_path".to_owned())
            .or_insert(Value::String(String::new()));
    }
    if entity == "task" {
        let description = map
            .remove("description")
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        Ok(format!(
            "---\n{}---\n{description}\n",
            serde_yaml_ng::to_string(&value)?
        ))
    } else {
        Ok(serde_yaml_ng::to_string(&value)?)
    }
}
fn sync_directories(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if fs::symlink_metadata(&path)?.is_dir() {
            sync_directories(&path)?;
        }
    }
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
// Flush every newly-created live ancestor represented in the staged tree,
// without descending into excluded local artifacts or session directories.
fn sync_installed_directories(staged: &Path, live: &Path) -> Result<()> {
    for entry in fs::read_dir(staged)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_installed_directories(&entry.path(), &live.join(entry.file_name()))?;
        }
    }
    fs::File::open(live)?.sync_all()?;
    Ok(())
}
impl StagedSnapshot {
    /// Commit a validated, blob-complete replica. All local edits made while the
    /// response was downloading are checked while writers are excluded.
    pub fn install(self, store: &Path) -> Result<()> {
        let base = fs::canonicalize(store.join("sync/.bootstrap"))?;
        if self.stage.path().parent() != Some(base.as_path()) {
            return Err(invalid("snapshot was staged for another store"));
        }
        let journal = Journal::open(store)?;
        journal.reconcile()?;
        apply::recover_remote(&journal.root)?;
        let _store = lock_file_reentrant(&store.join(".lock"))?;
        let boards = affected_boards(store, self.stage.path())?;
        let _boards: Vec<BoardGuard> = boards
            .iter()
            .map(|p| Storage::new(p).lock())
            .collect::<Result<_>>()?;
        let _sync = lock_file_reentrant(&journal.root.join(".lock"))?;
        journal.recover()?;
        let nonempty = !ProjectStore::at(store).list()?.is_empty();
        if nonempty && journal.node.store_id != self.header.store_id {
            return Err(invalid("SYNC_STORE_MISMATCH: use join --fresh"));
        }
        let staged_sync = self.stage.path().join("data/sync");
        if !projection::head_path(&staged_sync).is_file()
            || !staged_sync.join("meta/uids.json").is_file()
            || !self.stage.path().join("data/config.yaml").is_file()
        {
            return Err(invalid("snapshot is missing replica metadata"));
        }
        let local_heads = retained_vector(&journal.root)?;
        let remote_heads = retained_vector(&staged_sync)?;
        let id = &journal.node.node_id;
        if nonempty
            && local_heads.get(id).copied().unwrap_or(0)
                > remote_heads.get(id).copied().unwrap_or(0)
        {
            return Err(invalid(
                "SYNC_SERVER_BEHIND: local outbox changed during snapshot download",
            ));
        }
        let local_uids = projection::load_uids(&journal.root)?;
        let remote_uids = projection::load_uids(&staged_sync)?;
        // Prepare final local-field-preserving contents before writing the redo.
        for item in manifest(self.stage.path())? {
            match item? {
                Record::File { path, text } => {
                    let staged = self.stage.path().join("data").join(&path);
                    let local = local_file(store, &path, &remote_uids, &local_uids);
                    no_symlinks(&local)?;
                    if path == "config.yaml" {
                        write_bytes(&staged, merge_global(&local, &text)?.as_bytes())?;
                    } else if let Some(target) = target(&store.join(&path))
                        .filter(|t| matches!(t.entity, "task" | "project"))
                    {
                        write_bytes(
                            &staged,
                            merge_file(&local, &text, target.entity)?.as_bytes(),
                        )?;
                    }
                }
                Record::Blob { path, sha256, size } => {
                    let source = journal.root.join("blobs").join(&sha256);
                    let actual = file_hash(&source)?;
                    if actual != (sha256, size) {
                        return Err(invalid("snapshot blob hash or size mismatch"));
                    }
                    copy_atomic(&source, &self.stage.path().join("data").join(path))?;
                }
                _ => (),
            }
        }
        sync_directories(self.stage.path())?;
        let name = self
            .stage
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("invalid stage directory"))?
            .to_owned();
        let redo = Bootstrap {
            stage: name,
            header: self.header,
        };
        // Keep staging before the redo commit; an orphan is harmless, whereas
        // a committed redo must never point at a TempDir removed on error.
        let retained = self.stage.keep();
        fs::File::open(retained.parent().expect("bootstrap parent"))?.sync_all()?;
        atomic_write_text_raw(&journal.root.join("bootstrap.json"), &json(&redo)?)?;
        finish_bootstrap(store, &redo, &boards)?;
        Ok(())
    }
}
/// Finish an interrupted committed install before Journal::open exposes the store.
pub(super) fn recover_bootstrap(store: &Path) -> Result<()> {
    let path = store.join("sync/bootstrap.json");
    if !path.exists() {
        return Ok(());
    }
    let _store = lock_file_reentrant(&store.join(".lock"))?;
    if !path.exists() {
        return Ok(());
    }
    let redo: Bootstrap =
        serde_json::from_str(&fs::read_to_string(&path)?).map_err(|e| invalid(&e.to_string()))?;
    component(&redo.stage)?;
    validate_header(&redo.header)?;
    let stage = store.join("sync/.bootstrap").join(&redo.stage);
    no_symlinks(&stage)?;
    let boards = affected_boards(store, &stage)?;
    let _boards: Vec<BoardGuard> = boards
        .iter()
        .map(|p| Storage::new(p).lock())
        .collect::<Result<_>>()?;
    let _sync = lock_file_reentrant(&store.join("sync/.lock"))?;
    finish_bootstrap(store, &redo, &boards)
}
fn finish_bootstrap(store: &Path, redo: &Bootstrap, boards: &BTreeSet<PathBuf>) -> Result<()> {
    let root = store.join("sync");
    let stage = root.join(".bootstrap").join(&redo.stage);
    let data = stage.join("data");
    // Delete only approved replicated files; sessions, artifacts and context stay.
    for board in boards {
        for path in board_files(board)? {
            let relative = relative(store, &path)?;
            if !data.join(relative).is_file() {
                no_symlinks(&path)?;
                fs::remove_file(&path)?;
                fs::File::open(path.parent().expect("replicated parent"))?.sync_all()?;
            }
        }
    }
    for stale in ["journal", "meta", "snapshot"] {
        let path = root.join(stale);
        no_symlinks(&path)?;
        match fs::remove_dir_all(path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    for name in ["pending.json", "remote-pending.json", "drift.jsonl"] {
        match fs::remove_file(root.join(name)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    projection::forget(&root);
    prune::forget_groups(&root);
    for item in manifest(&stage)? {
        match item? {
            Record::File { path, .. } | Record::Blob { path, .. } => {
                allowed_path(&path)?;
                copy_atomic(&data.join(&path), &store.join(path))?;
            }
            _ => (),
        }
    }
    for board in boards.iter().filter(|p| p.join("project.yaml").is_file()) {
        for status in STATUSES {
            let path = board.join(".kanban/tasks").join(status);
            fs::create_dir_all(&path)?;
            fs::File::open(path)?.sync_all()?;
        }
        let threads = board.join(".kanban/threads");
        fs::create_dir_all(&threads)?;
        fs::File::open(threads)?.sync_all()?;
        fs::File::open(board.join(".kanban/tasks"))?.sync_all()?;
        fs::File::open(board.join(".kanban"))?.sync_all()?;
    }
    sync_installed_directories(&data, store)?;
    let mut clock: Hlc = load(&root.join("hlc"))?;
    if redo.header.hlc > clock {
        clock = redo.header.hlc.clone();
    }
    atomic_write_text_raw(&root.join("hlc"), &json(&clock)?)?;
    let node_path = root.join("node.yaml");
    let mut node: NodeIdentity = serde_yaml_ng::from_str(&fs::read_to_string(&node_path)?)?;
    node.store_id = redo.header.store_id.clone();
    let journal = Journal {
        root: root.clone(),
        node: node.clone(),
    };
    for item in manifest(&stage)? {
        let path = match item? {
            Record::File { path, .. } | Record::Blob { path, .. } => path,
            _ => continue,
        };
        if !path.starts_with("sync/") {
            drift::record(&journal, &store.join(path))?;
        }
    }
    let own = retained_vector(&root)?
        .get(&node.node_id)
        .copied()
        .unwrap_or(0);
    atomic_write_text_raw(&root.join("cursor.json"), &serde_json::json!({
        "server_node_id":redo.header.node_id,"store_id":redo.header.store_id,
        "hub_epoch":redo.header.hub_epoch,"last_seq":redo.header.seq,"pushed":own,"migrating":false
    }).to_string())?;
    format::write_format(store, STORE_FORMAT)?;
    format::write_stub(&root)?;
    // Identity is the final live-data commit point; the redo remains until durable.
    atomic_write_text_raw(&node_path, &serde_yaml_ng::to_string(&node)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&node_path, fs::Permissions::from_mode(0o600))?;
    }
    fs::File::open(&root)?.sync_all()?;
    fs::remove_file(root.join("bootstrap.json"))?;
    fs::File::open(&root)?.sync_all()?;
    fs::remove_dir_all(stage)?;
    Ok(())
}
/// Snapshot paths are store-relative and limited to replicated locations.
pub(crate) fn allowed_path(path: &str) -> Result<()> {
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts {
        component(part)?;
    }
    let ok = match parts.as_slice() {
        ["config.yaml"] => true,
        ["sync", "journal", _] => true,
        ["sync", "journal", _, file] if file.ends_with(".jsonl") => true,
        ["sync", "meta", "projection.json"] => true,
        ["sync", "meta", "uids.json"] => true,
        ["sync", "meta", rest @ ..] => !rest.is_empty(),
        ["sync", "blobs", hash] => {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        }
        ["projects", _, "project.yaml"] => true,
        ["projects", _, ".kanban", "config.yaml"] => true,
        ["projects", _, ".kanban", "tasks", status, file] => {
            STATUSES.contains(status) && file.ends_with(".md")
        }
        ["projects", _, ".kanban", "threads", file] => file.ends_with(".yaml"),
        ["projects", _, ".kanban", rest @ ..] => BOARD_TREES.iter().any(|tree| {
            let mut parts = rest.iter().copied();
            tree.split('/').all(|part| parts.next() == Some(part)) && parts.next().is_some()
        }),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(invalid("snapshot contains a machine-local path"))
    }
}

fn component(part: &str) -> Result<()> {
    if part.is_empty() || part == "." || part == ".." || part.contains(['/', '\\', '\0']) {
        return Err(invalid("invalid snapshot path"));
    }
    Ok(())
}

/// Strip machine-local fields while preserving key order and file format.
fn replicated(text: &str, entity: &str, global: bool) -> Result<String> {
    let mut value = parse(text, entity)?;
    let map = value
        .as_object_mut()
        .ok_or_else(|| invalid("snapshot file is not a mapping"))?;
    for field in local_fields(entity, global) {
        map.remove(*field);
    }
    match entity {
        "task" => {
            let description = map
                .remove("description")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            Ok(format!(
                "---\n{}---\n{description}\n",
                serde_yaml_ng::to_string(&value)?
            ))
        }
        "project" => {
            // The receiving node has its own working folder (section 3).
            map.insert("work_path".into(), Value::String(String::new()));
            Ok(serde_yaml_ng::to_string(&value)?)
        }
        _ => Ok(serde_yaml_ng::to_string(&value)?),
    }
}

/// Replicated global keys come from the snapshot; local sections stay.
fn merge_global(local_path: &Path, remote: &str) -> Result<String> {
    let mut merged: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(remote)?;
    let local: serde_yaml_ng::Mapping = match fs::read_to_string(local_path) {
        Ok(text) => serde_yaml_ng::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e.into()),
    };
    for field in local_fields("config", true) {
        merged.remove(*field);
        if let Some(value) = local.get(*field) {
            merged.insert((*field).into(), value.clone());
        }
    }
    Ok(serde_yaml_ng::to_string(&merged)?)
}

pub(super) fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    write_bytes_inner(path, bytes, true)
}

/// Visible to later reads in this store, without a durability barrier.
pub(super) fn write_bytes_cached(path: &Path, bytes: &[u8]) -> Result<()> {
    write_bytes_inner(path, bytes, false)
}

fn write_bytes_inner(path: &Path, bytes: &[u8], durable: bool) -> Result<()> {
    no_symlinks(path)?;
    let parent = path.parent().ok_or_else(|| invalid("missing parent"))?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp_")
        .tempfile_in(parent)?;
    tmp.write_all(bytes)?;
    if durable {
        tmp.as_file().sync_all()?;
    } else {
        std::io::Write::flush(&mut tmp)?;
    }
    tmp.persist(path).map_err(|e| KanbanError::Io(e.error))?;
    if durable {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn no_symlinks(path: &Path) -> Result<()> {
    if path.ancestors().any(Path::is_symlink) {
        return Err(invalid("replicated path contains a symlink"));
    }
    Ok(())
}

fn entries(dir: &Path) -> Result<Vec<PathBuf>> {
    if dir.is_symlink() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                // Skip in-flight temp files and anything not plain UTF-8.
                if name.to_str().is_some_and(|n| !n.starts_with(".tmp_")) {
                    result.push(entry.path());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    result.sort();
    Ok(result)
}

fn read_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in entries(dir)? {
        if fs::symlink_metadata(&path)?.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

fn read_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for path in entries(dir)? {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if fs::symlink_metadata(&path)?.is_dir() && STATUSES.contains(&name) {
            dirs.push(path);
        }
    }
    Ok(dirs)
}

fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in entries(dir)? {
        let kind = fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            files.extend(walk(&path)?);
        } else if kind.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

pub(super) fn boards(store: &Path) -> Result<Vec<PathBuf>> {
    entries(&store.join("projects"))?
        .into_iter()
        .filter_map(|path| match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => Some(Ok(path)),
            Ok(_) => None,
            Err(e) => Some(Err(e.into())),
        })
        .collect()
}

pub(super) fn board_files(board: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if board.join(".kanban").is_symlink() {
        return Ok(files);
    }
    for path in [
        board.join("project.yaml"),
        board.join(".kanban/config.yaml"),
    ] {
        if path.is_file() && !path.is_symlink() {
            files.push(path);
        }
    }
    for status in read_dirs(&board.join(".kanban/tasks"))? {
        files.extend(
            read_files(&status)?
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "md")),
        );
    }
    files.extend(
        read_files(&board.join(".kanban/threads"))?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "yaml")),
    );
    for tree in BOARD_TREES {
        let path = board.join(".kanban").join(tree);
        if !path.is_symlink() && !path.parent().is_some_and(Path::is_symlink) {
            files.extend(walk(&path)?);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::storage::NewTask;

    fn node(name: &str, role: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("config.yaml"),
            format!("node:\n  name: {name}\n  role: {role}\nsync:\n  enabled: true\n"),
        )
        .unwrap();
        Journal::open(root.path()).unwrap();
        root
    }
    fn seed(root: &Path, work: &Path) -> (crate::core::project::Project, String) {
        fs::create_dir_all(work).unwrap();
        let project = ProjectStore::at(root).add(work, None).unwrap().project;
        let storage = Storage::new(&project.data_root);
        storage.init_board().unwrap();
        let task = storage.create_task(NewTask::titled("initial")).unwrap();
        (project, task.id)
    }
    fn stage(source: &Path, receiver: &Path) -> StagedSnapshot {
        let spool = snapshot_spool(source).unwrap();
        let staged = stage_snapshot(receiver, spool.path()).unwrap();
        for hash in &staged.blobs {
            put_blob_file(receiver, hash, &source.join("sync/blobs").join(hash)).unwrap();
        }
        staged
    }
    #[test]
    fn install_rejects_new_outbox_created_during_download_without_changing_replica() {
        let source = node("server", "server");
        let receiver = node("laptop", "laptop");
        let work = tempfile::tempdir().unwrap();
        let (project, id) = seed(source.path(), &work.path().join("demo"));
        stage(source.path(), receiver.path())
            .install(receiver.path())
            .unwrap();
        let staged = stage(source.path(), receiver.path());
        let copy = Storage::new(receiver.path().join("projects").join(&project.id));
        {
            let _board = copy.lock().unwrap();
            let mut task = copy.load_task(&id).unwrap().unwrap();
            task.title = "new offline edit".into();
            copy.save_task(&task).unwrap();
        }
        let error = staged.install(receiver.path()).unwrap_err().to_string();
        assert!(error.contains("SYNC_SERVER_BEHIND"), "{error}");
        assert_eq!(
            copy.load_task(&id).unwrap().unwrap().title,
            "new offline edit"
        );
        assert!(!receiver.path().join("sync/bootstrap.json").exists());
    }
    #[test]
    fn lagging_snapshot_preserves_local_fields_and_removes_only_replicated_files() {
        let source = node("server", "server");
        let receiver = node("laptop", "laptop");
        let work = tempfile::tempdir().unwrap();
        let (project, id) = seed(source.path(), &work.path().join("demo"));
        let image = project.data_root.join(".kanban/assets/images/old.png");
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        fs::write(&image, [0, 255, 128]).unwrap();
        stage(source.path(), receiver.path())
            .install(receiver.path())
            .unwrap();
        let copied = receiver.path().join("projects").join(&project.id);
        let task = copied.join(".kanban/tasks/todo").join(format!("{id}.md"));
        let text = fs::read_to_string(&task).unwrap();
        write_bytes(
            &task,
            text.replacen(
                "---\n",
                "---\nsession: ses-local\nworktree: /local/worktree\n",
                1,
            )
            .as_bytes(),
        )
        .unwrap();
        let local_project = copied.join("project.yaml");
        let mut value: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&fs::read_to_string(&local_project).unwrap()).unwrap();
        value["work_path"] = "/local/work".into();
        write_bytes(
            &local_project,
            serde_yaml_ng::to_string(&value).unwrap().as_bytes(),
        )
        .unwrap();
        let config_path = receiver.path().join("config.yaml");
        let mut config: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        config["tui"] = serde_yaml_ng::from_str("theme: local").unwrap();
        write_bytes(
            &config_path,
            serde_yaml_ng::to_string(&config).unwrap().as_bytes(),
        )
        .unwrap();
        let artifact = copied.join(".kanban/context/local.md");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "local context").unwrap();
        fs::remove_file(&image).unwrap();
        {
            let original = Storage::new(&project.data_root);
            let _board = original.lock().unwrap();
            let mut task = original.load_task(&id).unwrap().unwrap();
            task.title = "new server title".into();
            original.save_task(&task).unwrap();
        }
        stage(source.path(), receiver.path())
            .install(receiver.path())
            .unwrap();
        let text = fs::read_to_string(task).unwrap();
        assert!(
            text.contains("new server title")
                && text.contains("ses-local")
                && text.contains("/local/worktree"),
            "{text}"
        );
        assert!(
            fs::read_to_string(local_project)
                .unwrap()
                .contains("/local/work")
        );
        assert!(
            fs::read_to_string(config_path)
                .unwrap()
                .contains("theme: local")
        );
        assert_eq!(fs::read_to_string(artifact).unwrap(), "local context");
        assert!(!copied.join(".kanban/assets/images/old.png").exists());
    }
    #[test]
    fn committed_redo_recovers_after_journal_was_removed_and_keeps_local_identity() {
        let source = node("server", "server");
        let receiver = node("phone", "mobile");
        let original = Journal::open(receiver.path()).unwrap().node;
        let work = tempfile::tempdir().unwrap();
        let (project, task) = seed(source.path(), &work.path().join("demo"));
        let staged = stage(source.path(), receiver.path());
        for item in manifest(staged.stage.path()).unwrap() {
            match item.unwrap() {
                Record::Blob { path, sha256, .. } => copy_atomic(
                    &receiver.path().join("sync/blobs").join(sha256),
                    &staged.stage.path().join("data").join(path),
                )
                .unwrap(),
                Record::File { path, text } if path == "config.yaml" => {
                    let text = merge_global(&receiver.path().join(&path), &text).unwrap();
                    write_bytes(
                        &staged.stage.path().join("data").join(path),
                        text.as_bytes(),
                    )
                    .unwrap();
                }
                _ => (),
            }
        }
        sync_directories(staged.stage.path()).unwrap();
        let name = staged
            .stage
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let redo = Bootstrap {
            stage: name,
            header: staged.header,
        };
        let _retained = staged.stage.keep();
        let root = receiver.path().join("sync");
        atomic_write_text_raw(&root.join("bootstrap.json"), &json(&redo).unwrap()).unwrap();
        fs::create_dir_all(root.join("journal")).unwrap();
        fs::remove_dir_all(root.join("journal")).unwrap();
        let recovered = Journal::open(receiver.path()).unwrap();
        assert_eq!(recovered.node.node_id, original.node_id);
        assert_eq!(recovered.node.name, "phone");
        assert_eq!(recovered.node.store_id, redo.header.store_id);
        assert!(Storage::new(receiver.path().join("projects").join(project.id)).task_exists(&task));
        let cursor: Value =
            serde_json::from_str(&fs::read_to_string(root.join("cursor.json")).unwrap()).unwrap();
        assert_eq!(cursor["last_seq"], redo.header.seq);
        assert!(!root.join("bootstrap.json").exists());
        assert!(!root.join(".bootstrap").join(redo.stage).exists());
    }
    #[test]
    fn staging_cannot_be_installed_into_another_store() {
        let source = node("server", "server");
        let receiver = node("laptop", "laptop");
        let other = node("phone", "mobile");
        let work = tempfile::tempdir().unwrap();
        seed(source.path(), &work.path().join("demo"));
        let staged = stage(source.path(), receiver.path());
        let error = staged.install(other.path()).unwrap_err().to_string();
        assert!(
            error.contains("staged for another store") || error.contains("No such file"),
            "{error}"
        );
        assert!(ProjectStore::at(other.path()).list().unwrap().is_empty());
        assert!(!other.path().join("sync/bootstrap.json").exists());
    }
}
