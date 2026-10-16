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
        self.store_blob(expected, bytes, true)
    }

    /// Page-cache copy of a blob. Drift rebuilds it from the file when the
    /// hash still matches; callers that replicate the blob use `put_blob`.
    fn put_blob_cached(&self, expected: &str, bytes: &[u8]) -> Result<()> {
        self.store_blob(expected, bytes, false)
    }

    fn store_blob(&self, expected: &str, bytes: &[u8], durable: bool) -> Result<()> {
        valid_hash(expected)?;
        if hash(bytes) != expected {
            return Err(invalid("blob hash mismatch"));
        }
        let path = self.root.join("blobs").join(expected);
        if !path.exists() {
            fs::create_dir_all(path.parent().expect("blob parent"))?;
            if durable {
                transport::write_bytes(&path, bytes)?;
            } else {
                transport::write_bytes_cached(&path, bytes)?;
            }
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
        // The common pass finds nothing; answer it without the board locks so
        // the background scan and every CLI start never stall a board write.
        if self.quiet(store) {
            return Ok(0);
        }
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
        let compact = if text.is_empty() {
            String::new()
        } else {
            format!("{text}\n")
        };
        let path = self.root.join("drift.jsonl");
        let same = changed == 0
            && fs::read_to_string(&path)
                .ok()
                .is_some_and(|current| current == compact);
        if !same {
            if compact.is_empty() {
                if path.exists() {
                    fs::remove_file(&path)?;
                }
            } else {
                atomic_write_text_raw(&path, &compact)?;
            }
        }
        // The index now matches disk, including writes that bypassed capture.
        format::mark_armed(store)?;
        Ok(changed)
    }

    /// Lock-free read of what the locked pass would conclude: true only when
    /// the store is armed, no redo is pending, the index is compact, and every
    /// board file matches it by mtime and size. Any doubt — a torn index line,
    /// an odd path, a missing file — answers false and leaves the decision to
    /// the locked pass, which also compacts the appended capture records.
    fn quiet(&self, store: &Path) -> bool {
        let check = || -> Result<bool> {
            if !format::armed(store) || self.root.join("intent.json").exists() {
                return Ok(false);
            }
            let Ok(text) = fs::read_to_string(self.root.join("drift.jsonl")) else {
                return Ok(false);
            };
            let mut index = BTreeMap::new();
            for line in text.lines() {
                let Ok((path, Some(entry))) = serde_json::from_str::<(String, Option<Entry>)>(line)
                else {
                    return Ok(false);
                };
                if index.insert(path, entry).is_some() {
                    return Ok(false);
                }
            }
            let mut paths = Vec::new();
            if store.join("config.yaml").is_file() {
                paths.push(store.join("config.yaml"));
            }
            for board in transport::boards(store)? {
                paths.extend(transport::board_files(&board)?);
            }
            if paths.len() != index.len() {
                return Ok(false);
            }
            for path in paths {
                let Some(entry) = path
                    .strip_prefix(store)
                    .ok()
                    .and_then(Path::to_str)
                    .and_then(|relative| index.get(relative))
                else {
                    return Ok(false);
                };
                let meta = fs::symlink_metadata(&path)?;
                let mtime = meta
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos());
                if !meta.is_file() || entry.mtime != mtime || entry.size != meta.len() {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        check().unwrap_or(false)
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
    journal.put_blob_cached(&hash, bytes)?;
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
    let present = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => true,
        Ok(_) => return Err(invalid("drift path is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    // A task moved between columns outside capture keeps its identity, as in
    // the full scan: the present copy inherits the vanished path's entry, and
    // the vanished path is left for that copy to claim.
    let moved_from = if present && !index.contains_key(&relative) {
        task_location(&relative).and_then(|id| {
            index.keys().find(|p| {
                task_location(p).as_ref() == Some(&id) && !store.join(p.as_str()).exists()
            })
        })
    } else {
        None
    };
    if !present && task_location(&relative).is_some() && moved_copy(path).is_some() {
        return Ok(());
    }
    let old = index
        .get(&relative)
        .or_else(|| moved_from.and_then(|p| index.get(p)));
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
    if let Some(vanished) = moved_from {
        record_observed(journal, &store.join(vanished), None)?;
    }
    Ok(())
}

/// Fold bypass writes into the journal before capture overwrites `path`.
///
/// Capture reads "before" from disk, so an edit made outside capture would be
/// absorbed into the next captured write and never replicated. Checking the
/// written path — plus the board metadata it depends on and, for a task, its
/// copy in another column — replaces a full store scan on every CLI start.
pub(super) fn reconcile_for_write(journal: &Journal, path: &Path) -> Result<()> {
    if let Some(board) = board_dir(path) {
        let project = board.join("project.yaml");
        if project != path {
            reconcile_path(journal, &project)?;
        }
    }
    if let Some(other) = moved_copy(path) {
        reconcile_path(journal, &other)?;
    }
    reconcile_path(journal, path)
}

fn task_location(path: &str) -> Option<String> {
    let (board, rest) = path.split_once("/.kanban/tasks/")?;
    Some(format!("{board}/{}", rest.rsplit('/').next()?))
}

/// `projects/<name>` for a path inside a store board.
fn board_dir(path: &Path) -> Option<&Path> {
    path.ancestors().find(|a| {
        a.parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "projects")
    })
}

/// The same task file in another column, when `path` is a task file.
fn moved_copy(path: &Path) -> Option<std::path::PathBuf> {
    let column = path.parent()?;
    let tasks = column.parent()?;
    if tasks.file_name()? != "tasks" || tasks.parent()?.file_name()? != ".kanban" {
        return None;
    }
    let name = path.file_name()?;
    fs::read_dir(tasks)
        .ok()?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|dir| dir != column)
        .map(|dir| dir.join(name))
        .find(|other| other.is_file())
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
