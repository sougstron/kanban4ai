//! Append-only per-node operation segments. The active segment is the highest
//! numbered file; `ahead.jsonl` holds sequence holes and is not part of the vector.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::format::JournalLimits;
use super::*;

const MARK_EVERY: u64 = 128;

#[derive(Debug, Clone)]
pub(super) struct Span {
    pub segment: String,
    pub start_offset: u64,
    pub end_offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Mark {
    seq: u64,
    offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegInfo {
    file: String,
    first: u64,
    last: u64,
    bytes: u64,
    #[serde(default)]
    last_hlc: Hlc,
    #[serde(default)]
    marks: Vec<Mark>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    nodes: BTreeMap<String, Vec<SegInfo>>,
}

fn journal_root(sync: &Path) -> PathBuf {
    sync.join("journal")
}

fn index_path(sync: &Path) -> PathBuf {
    sync.join("meta/index.json")
}

fn node_dir(sync: &Path, node: &str) -> PathBuf {
    journal_root(sync).join(node)
}

pub(super) fn segment_name(first: u64) -> String {
    format!("{first:020}.jsonl")
}

fn is_segment(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit()))
}

fn segments_of(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() && is_segment(&path) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn load_index(sync: &Path) -> Result<Index> {
    load(&index_path(sync))
}

fn store_index(sync: &Path, index: &Index) -> Result<()> {
    fs::create_dir_all(sync.join("meta"))?;
    atomic_write_text_raw(&index_path(sync), &json(index)?)
}

/// Drop a torn final line. Shared with the drift log.
pub(super) fn repair_tail(path: &Path) -> Result<()> {
    drift::repair_tail(path)
}

fn last_complete_line(path: &Path) -> Result<Option<String>> {
    repair_tail(path)?;
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }
    let start = len.saturating_sub(64 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if start > 0 {
        if let Some(pos) = bytes.iter().position(|b| *b == b'\n') {
            bytes.drain(..=pos);
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(text
        .lines()
        .rev()
        .find(|l| !l.is_empty())
        .map(str::to_owned))
}

fn line_seq(line: &str) -> Result<u64> {
    let op: Operation = serde_json::from_str(line).map_err(|e| invalid(&e.to_string()))?;
    sequence(&op)
}

/// Contiguous head: sequence of the last line of the active segment.
/// An empty file opened by rollover inherits the previous segment's head.
pub(super) fn head_seq(dir: &Path) -> Result<u64> {
    let files = segments_of(dir)?;
    if files.is_empty() {
        return Ok(0);
    }
    if let Some(line) = last_complete_line(files.last().expect("segment"))? {
        return line_seq(&line);
    }
    if files.len() >= 2
        && let Some(line) = last_complete_line(&files[files.len() - 2])?
    {
        return line_seq(&line);
    }
    Ok(0)
}

pub(super) fn tail_hlc(sync: &Path, node: &str) -> Result<Option<Hlc>> {
    let files = segments_of(&node_dir(sync, node))?;
    let Some(path) = files.last() else {
        return Ok(None);
    };
    let Some(line) = last_complete_line(path)? else {
        if files.len() >= 2
            && let Some(line) = last_complete_line(&files[files.len() - 2])?
        {
            let op: Operation = serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
            return Ok(Some(op.hlc));
        }
        return Ok(None);
    };
    let op: Operation = serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
    Ok(Some(op.hlc))
}

pub(super) fn heads(sync: &Path, floor: &VersionVector) -> Result<VersionVector> {
    let mut vector = floor.clone();
    let root = journal_root(sync);
    if !root.is_dir() {
        return Ok(vector);
    }
    for entry in fs::read_dir(&root)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let Some(node) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if node.starts_with('.') {
            continue;
        }
        let seq = head_seq(&path)?;
        let slot = vector.entry(node.to_owned()).or_insert(0);
        *slot = (*slot).max(seq);
    }
    Ok(vector)
}

fn load_ahead(dir: &Path) -> Result<BTreeMap<u64, Operation>> {
    let path = dir.join("ahead.jsonl");
    repair_tail(&path)?;
    let mut ops = BTreeMap::new();
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ops),
        Err(e) => return Err(e.into()),
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let op: Operation = serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
        ops.insert(sequence(&op)?, op);
    }
    Ok(ops)
}

pub(super) fn ahead_seqs(sync: &Path, node: &str) -> Result<BTreeSet<u64>> {
    Ok(load_ahead(&node_dir(sync, node))?.into_keys().collect())
}

fn write_ahead(dir: &Path, ops: &BTreeMap<u64, Operation>) -> Result<()> {
    let path = dir.join("ahead.jsonl");
    if ops.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {
                fs::File::open(dir)?.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(());
    }
    let mut text = String::new();
    for op in ops.values() {
        text.push_str(&json(op)?);
        text.push('\n');
    }
    atomic_write_text_raw(&path, &text)
}

fn active_file(dir: &Path, head: u64) -> Result<PathBuf> {
    let files = segments_of(dir)?;
    if let Some(path) = files.last() {
        return Ok(path.clone());
    }
    let path = dir.join(segment_name(head + 1));
    fs::create_dir_all(dir)?;
    OpenOptions::new().create(true).append(true).open(&path)?;
    fs::File::open(dir)?.sync_all()?;
    Ok(path)
}

/// Seal the active segment when it is already at the size limit, so the
/// following append (and the intent that names it) agree on the file.
pub(super) fn prepare(sync: &Path, node: &str, limits: &JournalLimits) -> Result<Span> {
    let dir = node_dir(sync, node);
    fs::create_dir_all(&dir)?;
    let head = head_seq(&dir)?;
    roll_if_needed(sync, &dir, node, head, limits)?;
    let path = active_file(&dir, head_seq(&dir)?)?;
    let len = fs::metadata(&path)?.len();
    Ok(Span {
        segment: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned(),
        start_offset: len,
        end_offset: len,
    })
}

fn roll_if_needed(
    sync: &Path,
    dir: &Path,
    node: &str,
    head: u64,
    limits: &JournalLimits,
) -> Result<()> {
    let files = segments_of(dir)?;
    let Some(path) = files.last() else {
        return Ok(());
    };
    repair_tail(path)?;
    let len = fs::metadata(path)?.len();
    if len == 0 || len < limits.segment_bytes {
        return Ok(());
    }
    remember_segment(sync, node, path)?;
    let next = dir.join(segment_name(head + 1));
    OpenOptions::new().create(true).append(true).open(&next)?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn remember_segment(sync: &Path, node: &str, path: &Path) -> Result<()> {
    let info = scan_segment(path)?;
    let mut index = load_index(sync)?;
    let list = index.nodes.entry(node.to_owned()).or_default();
    if let Some(existing) = list.iter_mut().find(|s| s.file == info.file) {
        *existing = info;
    } else {
        list.push(info);
    }
    list.sort_by(|a, b| a.file.cmp(&b.file));
    store_index(sync, &index)
}

fn scan_segment(path: &Path) -> Result<SegInfo> {
    repair_tail(path)?;
    let file = fs::File::open(path)?;
    let bytes = file.metadata()?.len();
    let mut marks = Vec::new();
    let mut first = None;
    let mut last = 0;
    let mut last_hlc = Hlc::default();
    let mut offset = 0u64;
    let mut count = 0u64;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let start = offset;
        offset += line.len() as u64 + 1;
        if line.trim().is_empty() {
            continue;
        }
        let op: Operation = serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
        let seq = sequence(&op)?;
        if first.is_none() {
            first = Some(seq);
        }
        if count % MARK_EVERY == 0 {
            marks.push(Mark { seq, offset: start });
        }
        last = seq;
        last_hlc = op.hlc;
        count += 1;
    }
    Ok(SegInfo {
        file: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned(),
        first: first.unwrap_or(0),
        last,
        bytes,
        last_hlc,
        marks,
    })
}

/// Append `ops` (any order) for one node. Contiguous sequence grows the active
/// segment in a single `write_all`; holes land in `ahead.jsonl` until filled.
pub(super) fn append(
    sync: &Path,
    node: &str,
    ops: &[Operation],
    limits: &JournalLimits,
) -> Result<Span> {
    let dir = node_dir(sync, node);
    fs::create_dir_all(&dir)?;
    let head = head_seq(&dir)?;
    roll_if_needed(sync, &dir, node, head, limits)?;
    let head = head_seq(&dir)?;
    let path = active_file(&dir, head)?;
    repair_tail(&path)?;
    let start = fs::metadata(&path)?.len();
    let mut ahead = load_ahead(&dir)?;
    let mut incoming: Vec<Operation> = ops.to_vec();
    incoming.sort_by_key(|op| sequence(op).unwrap_or(0));
    let mut batch = Vec::new();
    let mut expect = head;
    for op in incoming {
        let seq = sequence(&op)?;
        if seq <= expect || ahead.contains_key(&seq) {
            continue;
        }
        if seq == expect + 1 {
            expect = seq;
            batch.push(op);
        } else {
            ahead.insert(seq, op);
        }
        while let Some(next) = ahead.remove(&(expect + 1)) {
            expect = sequence(&next)?;
            batch.push(next);
        }
    }
    while let Some(next) = ahead.remove(&(expect + 1)) {
        expect = sequence(&next)?;
        batch.push(next);
    }
    ahead.retain(|seq, _| *seq > expect);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    if !batch.is_empty() {
        let mut text = String::new();
        for op in &batch {
            text.push_str(&json(op)?);
            text.push('\n');
        }
        file.write_all(text.as_bytes())?;
    }
    file.sync_data()?;
    let end = file.metadata()?.len();
    drop(file);
    write_ahead(&dir, &ahead)?;
    let written = end - start;
    if written > 0 {
        let seqs = batch.len() as u64;
        let first = sequence(batch.first().expect("batch"))?;
        if seqs >= MARK_EVERY || first % MARK_EVERY == 1 || end >= limits.segment_bytes {
            let _ = remember_segment(sync, node, &path);
        }
    }
    Ok(Span {
        segment: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned(),
        start_offset: start,
        end_offset: end,
    })
}

pub(super) fn truncate(sync: &Path, node: &str, segment: &str, offset: u64) -> Result<()> {
    let path = node_dir(sync, node).join(segment);
    let file = OpenOptions::new().write(true).open(&path)?;
    file.set_len(offset)?;
    file.sync_all()?;
    Ok(())
}

/// Operations with `seq > after` from contiguous segments. `ahead.jsonl` is omitted.
pub(super) fn read_after(sync: &Path, node: &str, after: u64) -> Result<Vec<Operation>> {
    let dir = node_dir(sync, node);
    let files = segments_of(&dir)?;
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let index = load_index(sync).unwrap_or_default();
    let infos = index.nodes.get(node);
    let active = files.last().cloned();
    let mut out = Vec::new();
    for path in files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let first: u64 = name.trim_end_matches(".jsonl").parse().unwrap_or(1);
        let info = infos.and_then(|list| list.iter().find(|s| s.file == name));
        // The active segment grows after the index was written. Only a sealed
        // segment's recorded last sequence is safe to skip.
        if Some(&path) != active.as_ref() && info.is_some_and(|s| s.last <= after) {
            continue;
        }
        if first > after + 1 && !out.is_empty() {
            // later segments are higher; still need them
        }
        let seek = info
            .and_then(|s| {
                s.marks
                    .iter()
                    .rev()
                    .find(|m| m.seq <= after.saturating_add(1))
                    .map(|m| m.offset)
            })
            .unwrap_or(0);
        repair_tail(&path)?;
        let mut file = fs::File::open(&path)?;
        file.seek(SeekFrom::Start(seek))?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let op: Operation = serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
            if sequence(&op)? > after {
                out.push(op);
            }
        }
    }
    Ok(out)
}

pub(super) fn read_ahead(sync: &Path, node: &str) -> Result<Vec<Operation>> {
    Ok(load_ahead(&node_dir(sync, node))?.into_values().collect())
}

/// Retained operations (`seq > floor`) in sequence order, ahead included.
pub(super) fn read_retained(sync: &Path, node: &str, floor: u64) -> Result<Vec<Operation>> {
    let mut ops = read_after(sync, node, floor)?;
    ops.extend(
        read_ahead(sync, node)?
            .into_iter()
            .filter(|op| sequence(op).ok().is_some_and(|seq| seq > floor)),
    );
    ops.sort_by_key(|op| sequence(op).unwrap_or(0));
    ops.dedup_by(|a, b| a.op == b.op);
    Ok(ops)
}

pub(super) fn node_names(sync: &Path) -> Result<Vec<String>> {
    let root = journal_root(sync);
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && !name.starts_with('.')
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// Delete sealed segments whose last sequence is `<= through`. The active file stays.
pub(super) fn drop_sealed(sync: &Path, node: &str, through: u64) -> Result<()> {
    let dir = node_dir(sync, node);
    let files = segments_of(&dir)?;
    if files.len() <= 1 {
        return Ok(());
    }
    let mut index = load_index(sync)?;
    let list = index.nodes.entry(node.to_owned()).or_default();
    for path in &files[..files.len() - 1] {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        let last = list
            .iter()
            .find(|s| s.file == name)
            .map(|s| s.last)
            .or_else(|| {
                last_complete_line(path)
                    .ok()
                    .flatten()
                    .and_then(|line| line_seq(&line).ok())
            })
            .unwrap_or(0);
        if last > 0 && last <= through {
            fs::remove_file(path)?;
            list.retain(|s| s.file != name);
        }
    }
    store_index(sync, &index)?;
    fs::File::open(&dir)?.sync_all()?;
    Ok(())
}

pub(super) fn rebuild_index(sync: &Path) -> Result<()> {
    let mut index = Index::default();
    for node in node_names(sync)? {
        let mut infos = Vec::new();
        let files = segments_of(&node_dir(sync, &node))?;
        // The active segment is recorded too so readers can seek it.
        for path in files {
            if fs::metadata(&path)?.len() == 0 {
                continue;
            }
            infos.push(scan_segment(&path)?);
        }
        if !infos.is_empty() {
            index.nodes.insert(node, infos);
        }
    }
    store_index(sync, &index)
}

fn stamp(path: &Path, hash: &mut u64) {
    mix(hash, path.to_string_lossy().as_bytes());
    let Ok(meta) = fs::metadata(path) else {
        mix(hash, b"absent");
        return;
    };
    let modified = meta
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    mix(hash, &meta.len().to_le_bytes());
    mix(hash, &modified.to_le_bytes());
}

/// Sequential reader of contiguous operations with `seq > after`.
/// One node's HLC grows with its sequence, so a caller can stop after a page.
pub(super) struct Cursor {
    pending: Vec<(PathBuf, u64)>,
    at: usize,
    reader: Option<BufReader<fs::File>>,
    after: u64,
}

impl Cursor {
    pub(super) fn open(sync: &Path, node: &str, after: u64) -> Result<Self> {
        let dir = node_dir(sync, node);
        let files = segments_of(&dir)?;
        let index = load_index(sync).unwrap_or_default();
        let infos = index.nodes.get(node);
        let active = files.last().cloned();
        if let Some(path) = &active {
            repair_tail(path)?;
        }
        let mut pending = Vec::new();
        for path in files {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let first: u64 = name.trim_end_matches(".jsonl").parse().unwrap_or(1);
            let info = infos.and_then(|list| list.iter().find(|s| s.file == name));
            if Some(&path) != active.as_ref() && info.is_some_and(|s| s.last <= after) {
                continue;
            }
            let seek = if first > after {
                0
            } else {
                info.and_then(|segment| {
                    segment
                        .marks
                        .iter()
                        .rev()
                        .find(|mark| mark.seq <= after.saturating_add(1))
                        .map(|mark| mark.offset)
                })
                .unwrap_or(0)
            };
            pending.push((path, seek));
        }
        Ok(Self {
            pending,
            at: 0,
            reader: None,
            after,
        })
    }

    pub(super) fn next_op(&mut self) -> Result<Option<Operation>> {
        loop {
            if self.reader.is_none() {
                if self.at >= self.pending.len() {
                    return Ok(None);
                }
                let seek = self.pending[self.at].1;
                let path = self.pending[self.at].0.clone();
                self.at += 1;
                let mut file = fs::File::open(path)?;
                file.seek(SeekFrom::Start(seek))?;
                self.reader = Some(BufReader::new(file));
            }
            let reader = self.reader.as_mut().expect("open segment");
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                self.reader = None;
                continue;
            }
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                continue;
            }
            let op: Operation = serde_json::from_str(line).map_err(|e| invalid(&e.to_string()))?;
            if sequence(&op)? > self.after {
                return Ok(Some(op));
            }
        }
    }
}

fn mix(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

/// Change token of the files a poller must notice. `None` when sync is absent.
pub fn change_token(store: &Path) -> Option<u64> {
    let sync = store.join("sync");
    if !sync.is_dir() {
        return None;
    }
    let mut hash = 0xcbf29ce484222325u64;
    if let Ok(nodes) = node_names(&sync) {
        for node in nodes {
            let dir = node_dir(&sync, &node);
            if let Ok(files) = segments_of(&dir)
                && let Some(active) = files.last()
            {
                stamp(active, &mut hash);
            }
            stamp(&dir.join("ahead.jsonl"), &mut hash);
        }
    }
    for name in [
        "meta/projection.json",
        "meta/projection/head.json",
        "meta/uids.json",
        "meta/conflicts.jsonl",
        "meta/conflicts-archive.jsonl",
    ] {
        stamp(&sync.join(name), &mut hash);
    }
    Some(hash)
}
