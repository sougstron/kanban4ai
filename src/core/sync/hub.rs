//! Server-local sequence index over the retained per-origin journal.
use super::*;
use std::io::{Seek, SeekFrom};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubOp { pub seq: u64, pub op: Operation }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubPage { pub ops: Vec<HubOp>, pub next: u64, pub head: u64, pub more: bool }
#[derive(Clone, Default, Serialize, Deserialize)]
struct Segment {
    first: u64, last: u64, heads_before: VersionVector,
    #[serde(default)] bytes: u64,
    #[serde(default)] offsets: Vec<(u64, u64)>,
}
#[derive(Default, Serialize, Deserialize)]
struct Heads {
    through_seq: u64, heads: VersionVector, segments: Vec<Segment>,
    #[serde(default)] base: VersionVector,
}
pub struct Hub { store: PathBuf, root: PathBuf }
impl Hub {
    pub fn open(store: &Path) -> Result<Self> {
        Journal::open(store)?;
        let hub = Self { store: store.to_path_buf(), root: store.join("sync/hub") };
        let _guard = lock_file_reentrant(&store.join("sync/.lock"))?;
        hub.initialize()?;
        hub.recover()?;
        Ok(hub)
    }
    // Migration calls this without recursively opening Journal.
    pub(super) fn init_if_missing(store: &Path) -> Result<Self> {
        let hub = Self { store: store.to_path_buf(), root: store.join("sync/hub") };
        hub.initialize()?;
        hub.recover()?;
        Ok(hub)
    }
    fn initialize(&self) -> Result<()> {
        fs::create_dir_all(self.root.join("log"))?;
        if !self.root.join("epoch").exists() { atomic_write_text_raw(&self.root.join("epoch"), &uuid()?)?; }
        if !self.root.join("floor").exists() { atomic_write_text_raw(&self.root.join("floor"), "0\n")?; }
        Ok(())
    }
    fn segment_path(&self, first: u64) -> PathBuf { self.root.join("log").join(format!("{first:020}.jsonl")) }
    pub fn epoch(&self) -> Result<String> { Ok(fs::read_to_string(self.root.join("epoch"))?.trim().into()) }
    pub fn floor(&self) -> Result<u64> {
        fs::read_to_string(self.root.join("floor"))?.trim().parse().map_err(|_| invalid("invalid hub floor"))
    }
    fn save(&self, heads: &Heads) -> Result<()> { atomic_write_text_raw(&self.root.join("heads.json"), &json(heads)?) }
    /// The log is authoritative. A torn final line is discarded; cached heads
    /// advance only after complete, synced records. Unchanged segments are skipped.
    fn recover(&self) -> Result<Heads> {
        let floor = self.floor()?;
        let mut heads: Heads = load(&self.root.join("heads.json"))?;
        let mut paths: Vec<PathBuf> = fs::read_dir(self.root.join("log"))?.map(|e| e.map(|e| e.path())).collect::<std::io::Result<_>>()?;
        paths.retain(|p| p.extension().is_some_and(|s| s == "jsonl")); paths.sort();
        let mut changed = false;
        for path in paths {
            let first: u64 = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()).ok_or_else(|| invalid("invalid hub segment"))?;
            if first <= floor && heads.segments.iter().all(|s| s.first != first) { fs::remove_file(path)?; continue; }
            let slot = heads.segments.iter().position(|s| s.first == first);
            let index = slot.unwrap_or_else(|| {
                heads.segments.push(Segment { first, last: first - 1, heads_before: heads.heads.clone(), ..Default::default() });
                heads.segments.len() - 1
            });
            let size = fs::metadata(&path)?.len();
            let offset = heads.segments[index].bytes;
            if size == offset { continue; }
            if size < offset { return Err(invalid("hub segment truncated before durable head")); }
            let mut reader = BufReader::new(fs::File::open(&path)?); reader.seek(SeekFrom::Start(offset))?;
            let mut position = offset;
            loop {
                let mut line = String::new(); let len = reader.read_line(&mut line)?;
                if len == 0 { break; }
                if !line.ends_with('\n') {
                    OpenOptions::new().write(true).open(&path)?.set_len(position)?; break;
                }
                let item: HubOp = serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
                if item.seq != heads.through_seq + 1 { return Err(invalid("noncontiguous hub sequence")); }
                if (item.seq - first).is_multiple_of(128) { heads.segments[index].offsets.push((item.seq, position)); }
                heads.heads.insert(item.op.hlc.2.clone(), sequence(&item.op)?);
                heads.through_seq = item.seq; heads.segments[index].last = item.seq;
                position += len as u64; heads.segments[index].bytes = position; changed = true;
            }
        }
        if changed || !self.root.join("heads.json").exists() { self.save(&heads)?; }
        Ok(heads)
    }
    pub fn ingest(&self) -> Result<u64> {
        let journal = Journal::open(&self.store)?;
        let _guard = lock_file_reentrant(&self.store.join("sync/.lock"))?;
        self.ingest_journal(&journal)
    }
    pub(super) fn ingest_journal(&self, journal: &Journal) -> Result<u64> {
        let mut heads = self.recover()?;
        let compacted = journal.state()?.compacted;
        // A newly created hub starts at the retained frontier, not at an
        // unavailable prefix. This baseline survives subsequent compaction.
        if heads.through_seq == 0 && heads.segments.is_empty() {
            heads.base = compacted.clone(); heads.heads = compacted; self.save(&heads)?;
        }
        let limits = format::JournalLimits::load(&self.store)?;
        loop {
            let page = journal.operations_after(&heads.heads, 500)?;
            if page.operations.is_empty() { break; }
            let mut ops = page.operations;
            ops.sort_by(|a,b| (&a.hlc,&a.op).cmp(&(&b.hlc,&b.op)));
            for op in ops {
                let seq = heads.through_seq.checked_add(1).ok_or_else(|| invalid("hub sequence exhausted"))?;
                let roll = heads.segments.last().is_none_or(|s| s.bytes >= limits.segment_bytes);
                if roll { heads.segments.push(Segment { first: seq, last: seq - 1, heads_before: heads.heads.clone(), ..Default::default() }); }
                let segment = heads.segments.last_mut().expect("active segment");
                let mut bytes = json(&HubOp { seq, op: op.clone() })?; bytes.push('\n');
                let mut file = OpenOptions::new().create(true).append(true).open(self.segment_path(segment.first))?;
                file.write_all(bytes.as_bytes())?; file.sync_data()?;
                if (seq - segment.first).is_multiple_of(128) { segment.offsets.push((seq, segment.bytes)); }
                segment.bytes += bytes.len() as u64; segment.last = seq;
                heads.heads.insert(op.hlc.2.clone(), sequence(&op)?); heads.through_seq = seq;
            }
            self.save(&heads)?;
        }
        Ok(heads.through_seq)
    }
    fn read_segment(&self, segment: &Segment, after: u64, mut visit: impl FnMut(HubOp) -> Result<bool>) -> Result<()> {
        let offset = segment.offsets.iter().rev().find(|(seq,_)| *seq <= after.saturating_add(1)).map(|(_,off)| *off).unwrap_or(0);
        let mut reader = BufReader::new(fs::File::open(self.segment_path(segment.first))?); reader.seek(SeekFrom::Start(offset))?;
        for line in reader.lines() {
            let item: HubOp = serde_json::from_str(&line?).map_err(|e| invalid(&e.to_string()))?;
            if item.seq > after && !visit(item)? { break; }
        }
        Ok(())
    }
    pub fn page(&self, after: u64, limit: usize, max_bytes: usize) -> Result<HubPage> {
        let _guard = lock_file_reentrant(&self.store.join("sync/.lock"))?;
        let heads = self.recover()?;
        if after < self.floor()? { return Err(invalid("SYNC_SNAPSHOT_REQUIRED")); }
        if after > heads.through_seq { return Err(invalid("SYNC_SNAPSHOT_REQUIRED: cursor beyond hub head")); }
        let mut page = HubPage { ops: Vec::new(), next: after, head: heads.through_seq, more: false };
        let mut bytes = 0;
        for segment in heads.segments.iter().filter(|s| s.last > after) {
            self.read_segment(segment, after, |item| {
                let size = json(&item)?.len();
                if page.ops.len() >= limit.max(1) || bytes + size > max_bytes {
                    if page.ops.is_empty() { return Err(invalid("hub operation exceeds page byte budget")); }
                    return Ok(false);
                }
                bytes += size; page.next = item.seq; page.ops.push(item); Ok(true)
            })?;
            if page.ops.len() >= limit.max(1) || page.next < segment.last { break; }
        }
        page.more = page.next < page.head; Ok(page)
    }
    pub fn vector_at(&self, seq: u64) -> Result<VersionVector> {
        let _guard = lock_file_reentrant(&self.store.join("sync/.lock"))?;
        let heads = self.recover()?;
        if seq < self.floor()? || seq > heads.through_seq { return Err(invalid("SYNC_SNAPSHOT_REQUIRED")); }
        let Some(segment) = heads.segments.iter().find(|s| s.last >= seq) else { return Ok(heads.heads); };
        let mut vector = segment.heads_before.clone();
        // Start at the segment baseline; sparse seeking would skip its vector deltas.
        let mut scan = segment.clone(); scan.offsets.clear();
        self.read_segment(&scan, segment.first - 1, |item| {
            if item.seq > seq { return Ok(false); }
            vector.insert(item.op.hlc.2.clone(), sequence(&item.op)?); Ok(true)
        })?;
        Ok(vector)
    }
    pub fn record_cursor(&self, client: &str, after: u64) -> Result<()> {
        let _guard = lock_file_reentrant(&self.store.join("sync/.lock"))?;
        if after > self.recover()?.through_seq { return Err(invalid("invalid hub cursor")); }
        let mut clients: BTreeMap<String, Value> = load(&self.root.join("clients.json"))?;
        let record = clients.get_mut(client).ok_or_else(|| invalid("NODE_AUTH_FAILED"))?;
        record["cursor"] = Value::from(after); record["last_seen"] = Value::from(chrono::Utc::now().to_rfc3339());
        atomic_write_text_raw(&self.root.join("clients.json"), &json(&clients)?)
    }
    pub fn touch(&self, client: &str) -> Result<()> {
        let _guard = lock_file_reentrant(&self.store.join("sync/.lock"))?;
        let mut clients: BTreeMap<String, Value> = load(&self.root.join("clients.json"))?;
        let record = clients.get_mut(client).ok_or_else(|| invalid("NODE_AUTH_FAILED"))?;
        record["last_seen"] = Value::from(chrono::Utc::now().to_rfc3339());
        atomic_write_text_raw(&self.root.join("clients.json"), &json(&clients)?)
    }
    fn active_cursors(&self, ttl_days: u64) -> Result<BTreeMap<String,u64>> {
        let clients: BTreeMap<String, Value> = load(&self.root.join("clients.json"))?;
        let now = chrono::Utc::now();
        Ok(clients.into_iter().filter_map(|(id,c)| {
            let seen = c["last_seen"].as_str()?.parse::<chrono::DateTime<chrono::Utc>>().ok()?;
            ((now-seen).num_seconds() <= ttl_days.saturating_mul(86400).min(i64::MAX as u64) as i64).then(|| (id,c["cursor"].as_u64().unwrap_or(0)))
        }).collect())
    }
    pub fn compact(&self, ttl_days: u64) -> Result<()> {
        let _guard = lock_file_reentrant(&self.store.join("sync/.lock"))?;
        let mut heads = self.recover()?;
        let stable = self.active_cursors(ttl_days)?.values().copied().min().unwrap_or(heads.through_seq);
        let count = heads.segments.iter().take(heads.segments.len().saturating_sub(1)).take_while(|s| s.last <= stable).count();
        if count == 0 { return Ok(()); }
        let removed: Vec<_> = heads.segments.drain(..count).collect();
        let floor = removed.last().expect("removed segment").last;
        heads.base = heads.segments.first().map(|s| s.heads_before.clone()).unwrap_or_else(|| heads.heads.clone());
        // Cache first, then floor, then unlink. Recovery ignores leftover segments
        // below the floor, including a crash between any two unlinks.
        self.save(&heads)?;
        atomic_write_text_raw(&self.root.join("floor"), &format!("{floor}\n"))?;
        for segment in removed { fs::remove_file(self.segment_path(segment.first))?; }
        Ok(())
    }
    pub fn maintenance(&self) -> Result<()> {
        let journal = Journal::open(&self.store)?;
        let _guard = lock_file_reentrant(&self.store.join("sync/.lock"))?;
        self.ingest_journal(&journal)?;
        let config: Value = match fs::read_to_string(self.store.join("config.yaml")) { Ok(text) => serde_yaml_ng::from_str(&text)?, Err(e) if e.kind()==std::io::ErrorKind::NotFound => Value::Null, Err(e) => return Err(e.into()) };
        let ttl = config.pointer("/sync/hub/client_ttl_days").and_then(Value::as_u64).unwrap_or(30);
        let mut acks = BTreeMap::new();
        for (id,cursor) in self.active_cursors(ttl)? { acks.insert(id, self.vector_at(cursor)?); }
        journal.prune(&acks)?; self.compact(ttl)
    }
}
