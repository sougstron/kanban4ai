//! Stat-first drift detection. Capture appends one index record; scans compact it.
use super::*;
use sha2::{Digest, Sha256};
use std::time::UNIX_EPOCH;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Entry {
    mtime: u128,
    size: u64,
    hash: String,
}

pub(super) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_hash(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(invalid("invalid blob hash"));
    }
    Ok(())
}

impl Journal {
    /// Content-addressed bytes, verified on both ingress and egress.
    pub fn put_blob(&self, expected: &str, bytes: &[u8]) -> Result<()> {
        valid_hash(expected)?;
        if hash(bytes) != expected {
            return Err(invalid("blob hash mismatch"));
        }
        let path = self.root.join("blobs").join(expected);
        if !path.exists() {
            fs::create_dir_all(path.parent().expect("blob parent"))?;
            transport::write_bytes(&path, bytes)?;
        }
        Ok(())
    }

    pub fn blob(&self, hash: &str) -> Result<Vec<u8>> {
        valid_hash(hash)?;
        let bytes = fs::read(self.root.join("blobs").join(hash))?;
        if self::hash(&bytes) != hash {
            return Err(invalid("blob hash mismatch"));
        }
        Ok(bytes)
    }

    /// Scan only snapshot-approved board trees, never agent artifacts/context.
    /// The scan is not part of task writes. Locks follow store -> board -> sync.
    pub fn reconcile(&self) -> Result<usize> {
        let store = self.root.parent().expect("sync parent");
        let _store = lock_file_reentrant(&store.join(".lock"))?;
        let boards = transport::boards(store)?;
        let _boards = boards
            .iter()
            .map(|p| super::super::storage::Storage::new(p).lock())
            .collect::<Result<Vec<_>>>()?;
        let _sync = lock_file_reentrant(&self.root.join(".lock"))?;
        self.recover()?;
        let mut index = index(self)?;
        let mut paths = BTreeSet::new();
        if store.join("config.yaml").is_file() {
            paths.insert("config.yaml".to_owned());
        }
        for board in boards {
            for path in transport::board_files(&board)? {
                let relative = path
                    .strip_prefix(store)
                    .map_err(|_| invalid("invalid drift path"))?
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF8 drift path"))?
                    .to_owned();
                transport::allowed_path(&relative)?;
                paths.insert(relative);
            }
        }
        let present = paths.clone();
        let task_location = |path: &str| -> Option<String> {
            let (board, rest) = path.split_once("/.kanban/tasks/")?;
            Some(format!("{board}/{}", rest.rsplit('/').next()?))
        };
        let task_paths: BTreeMap<_, _> = present
            .iter()
            .filter_map(|p| Some((task_location(p)?, p)))
            .collect();
        paths.extend(index.keys().cloned());
        let mut changed = 0;
        // Project metadata is captured before dependent task/thread files.
        let mut paths: Vec<_> = paths.into_iter().collect();
        paths.sort_by_key(|p| {
            (
                !present.contains(p),
                !p.ends_with("/project.yaml"),
                p.clone(),
            )
        });
        for relative in paths {
            let path = store.join(&relative);
            let identity = task_location(&relative);
            if !present.contains(&relative)
                && identity
                    .as_ref()
                    .is_some_and(|id| task_paths.contains_key(id))
            {
                index.remove(&relative);
                continue;
            }
            let old = index.get(&relative).or_else(|| {
                identity.as_ref().and_then(|id| {
                    index
                        .iter()
                        .find(|(p, _)| {
                            !present.contains(*p) && task_location(p).as_ref() == Some(id)
                        })
                        .map(|(_, entry)| entry)
                })
            });
            let current = if present.contains(&relative) {
                let meta = fs::symlink_metadata(&path)?;
                if !meta.is_file() {
                    return Err(invalid("drift path is not a regular file"));
                }
                let mtime = meta
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| invalid("mtime before epoch"))?
                    .as_nanos();
                if let Some(old) = old.filter(|old| old.mtime == mtime && old.size == meta.len()) {
                    let old = old.clone();
                    index.entry(relative).or_insert(old);
                    continue;
                }
                let bytes = fs::read(&path)?;
                let hash = hash(&bytes);
                let entry = Entry {
                    mtime,
                    size: meta.len(),
                    hash,
                };
                if old.is_some_and(|old| old.hash == entry.hash) {
                    index.insert(relative, entry);
                    continue;
                }
                self.put_blob(&entry.hash, &bytes)?;
                Some((entry, bytes))
            } else {
                None
            };
            let Some(target) = target(&path) else {
                continue;
            };
            let before_bytes = old.map(|old| self.blob(&old.hash)).transpose()?;
            let file_value = |hash: &str, size: u64| {
                json(&serde_json::json!({"path": target.id, "blob": hash, "size": size}))
            };
            let before = if target.entity == "file" {
                old.map(|old| file_value(&old.hash, old.size)).transpose()?
            } else {
                before_bytes
                    .map(String::from_utf8)
                    .transpose()
                    .map_err(|_| invalid("replicated text is not UTF-8"))?
            };
            let after = if target.entity == "file" {
                current
                    .as_ref()
                    .map(|(entry, _)| file_value(&entry.hash, entry.size))
                    .transpose()?
            } else {
                current
                    .as_ref()
                    .map(|(_, bytes)| std::str::from_utf8(bytes).map(str::to_owned))
                    .transpose()
                    .map_err(|_| invalid("replicated text is not UTF-8"))?
            };
            capture_inner(
                &path,
                after.as_deref(),
                Some(before.as_deref()),
                current.as_ref().map(|(entry, _)| entry.clone()),
            )?;
            changed += 1;
            if let Some((entry, _)) = current {
                index.insert(relative, entry);
            } else {
                index.remove(&relative);
            }
        }
        let text = index
            .iter()
            .map(|(path, entry)| json(&(path, Some(entry))))
            .collect::<Result<Vec<_>>>()?
            .join("\n");
        atomic_write_text_raw(&self.root.join("drift.jsonl"), &(text + "\n"))?;
        // The index now matches disk, including writes that bypassed capture.
        format::mark_armed(store)?;
        Ok(changed)
    }
}

fn index(journal: &Journal) -> Result<BTreeMap<String, Entry>> {
    let mut entries = BTreeMap::new();
    let path = journal.root.join("drift.jsonl");
    if !path.exists() {
        return Ok(entries);
    }
    repair_tail(&path)?;
    for line in BufReader::new(fs::File::open(path)?).lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let (path, entry): (String, Option<Entry>) =
            serde_json::from_str(&line).map_err(|e| invalid(&e.to_string()))?;
        transport::allowed_path(&path)?;
        if let Some(entry) = entry {
            entries.insert(path, entry);
        } else {
            entries.remove(&path);
        }
    }
    Ok(entries)
}

fn entry_for(journal: &Journal, path: &Path, bytes: &[u8]) -> Result<Entry> {
    let meta = fs::metadata(path)?;
    let hash = hash(bytes);
    journal.put_blob(&hash, bytes)?;
    Ok(Entry {
        mtime: meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid("mtime before epoch"))?
            .as_nanos(),
        size: meta.len(),
        hash,
    })
}

/// Called in the same redo transaction as the file write, including remote writes.
pub(super) fn record(journal: &Journal, path: &Path) -> Result<()> {
    let store = journal.root.parent().expect("sync parent");
    let Ok(relative) = path.strip_prefix(store) else {
        return Ok(());
    };
    let Some(relative) = relative.to_str() else {
        return Ok(());
    };
    if relative.starts_with("sync/") || transport::allowed_path(relative).is_err() {
        return Ok(());
    }
    let entry = match fs::read(path) {
        Ok(bytes) => Some(entry_for(journal, path, &bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    record_observed(journal, path, entry.as_ref())
}

pub(super) fn record_observed(journal: &Journal, path: &Path, entry: Option<&Entry>) -> Result<()> {
    let store = journal.root.parent().expect("sync parent");
    let relative = path
        .strip_prefix(store)
        .map_err(|_| invalid("invalid drift path"))?
        .to_str()
        .ok_or_else(|| invalid("non-UTF8 drift path"))?;
    let index = journal.root.join("drift.jsonl");
    // A crash can tear only the final append. Remove that suffix before replay.
    repair_tail(&index)?;
    let mut file = OpenOptions::new().create(true).append(true).open(index)?;
    writeln!(file, "{}", json(&(relative, entry))?)?;
    file.sync_all()?;
    Ok(())
}

pub(super) fn validate_file(id: &str, values: &Fields) -> Result<()> {
    let path = values.get("path").and_then(Value::as_str).unwrap_or(id);
    if path != id
        || !(path.starts_with(".kanban/instructions/")
            || path.starts_with(".kanban/assets/images/"))
    {
        return Err(invalid("invalid replicated file path"));
    }
    transport::allowed_path(&format!("projects/project/{path}"))?;
    if let Some(hash) = values.get("blob").filter(|v| !v.is_null()) {
        valid_hash(hash.as_str().ok_or_else(|| invalid("invalid blob hash"))?)?;
    }
    if let Some(size) = values.get("size").filter(|v| !v.is_null()) {
        if !size.is_number() {
            return Err(invalid("invalid blob size"));
        }
    }
    Ok(())
}

/// One path, relative to the drift base. Does not take the store lock: recovery
/// runs it while the board lock is already held.
pub(super) fn reconcile_path(journal: &Journal, path: &Path) -> Result<()> {
    let store = journal.root.parent().expect("sync parent");
    let relative = match path.strip_prefix(store) {
        Ok(relative) => relative
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 drift path"))?
            .to_owned(),
        Err(_) => return Ok(()),
    };
    if relative.starts_with("sync/") || transport::allowed_path(&relative).is_err() {
        return Ok(());
    }
    let index = index(journal)?;
    let old = index.get(&relative);
    let present = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => true,
        Ok(_) => return Err(invalid("drift path is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    if !present && old.is_none() {
        return Ok(());
    }
    let current = if present {
        let meta = fs::symlink_metadata(path)?;
        let mtime = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid("mtime before epoch"))?
            .as_nanos();
        if old.is_some_and(|old| old.mtime == mtime && old.size == meta.len()) {
            return Ok(());
        }
        let bytes = fs::read(path)?;
        let digest = hash(&bytes);
        if old.is_some_and(|old| old.hash == digest) {
            return Ok(());
        }
        journal.put_blob(&digest, &bytes)?;
        Some((
            Entry {
                mtime,
                size: meta.len(),
                hash: digest,
            },
            bytes,
        ))
    } else {
        None
    };
    let Some(target) = target(path) else {
        return Ok(());
    };
    let before_bytes = old.map(|old| journal.blob(&old.hash)).transpose()?;
    let file_value = |hash: &str, size: u64| {
        json(&serde_json::json!({"path": target.id, "blob": hash, "size": size}))
    };
    let before = if target.entity == "file" {
        old.map(|old| file_value(&old.hash, old.size)).transpose()?
    } else {
        before_bytes
            .map(String::from_utf8)
            .transpose()
            .map_err(|_| invalid("replicated text is not UTF-8"))?
    };
    let after = if target.entity == "file" {
        current
            .as_ref()
            .map(|(entry, _)| file_value(&entry.hash, entry.size))
            .transpose()?
    } else {
        current
            .as_ref()
            .map(|(_, bytes)| std::str::from_utf8(bytes).map(str::to_owned))
            .transpose()
            .map_err(|_| invalid("replicated text is not UTF-8"))?
    };
    capture_inner(
        path,
        after.as_deref(),
        Some(before.as_deref()),
        current.as_ref().map(|(entry, _)| entry.clone()),
    )?;
    Ok(())
}

pub(super) fn repair_tail(path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if file.metadata()?.len() == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    if byte[0] != b'\n' {
        file.rewind()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let len = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        file.set_len(len as u64)?;
        file.sync_all()?;
    }
    Ok(())
}
