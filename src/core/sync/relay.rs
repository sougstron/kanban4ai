//! Single-writer git refs for append-only sync batches and bounded runner tails.
use super::*;
use crate::core::project::ProjectStore;
use sha2::{Digest, Sha256};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PREFIX: &str = "refs/kanban/sync/";
const FETCHED: &str = "refs/remotes/relay/";
const LOG_PREFIX: &str = "refs/kanban/logs/";
const LOG_FETCHED: &str = "refs/remotes/relay-logs/";

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Explicit URL/path of a dedicated store remote; otherwise project remotes.
    pub relay_remote: Option<String>,
    pub relay_interval: u64,
    pub log_interval: u64,
    pub log_tail_bytes: usize,
    pub relay_encrypt: bool,
    pub max_skew: u64,
    pub peers: Vec<RelayPeer>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            relay_remote: None,
            relay_interval: 60,
            log_interval: 60,
            log_tail_bytes: 64 * 1024,
            relay_encrypt: false,
            max_skew: 300,
            peers: Vec::new(),
        }
    }
}
#[derive(Clone, Deserialize)]
pub struct RelayPeer {
    #[serde(flatten)]
    pub node: NodeIdentity,
}
#[derive(Default, Deserialize)]
struct Recipient {
    node_id: String,
    #[serde(default)]
    relay_recipient: Option<String>,
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub node_id: String,
    pub store_id: String,
    pub at_ms: u64,
    pub vector: VersionVector,
}
#[derive(Default, Serialize, Deserialize)]
struct Cache {
    /// Ref tips are persisted only after the whole fetched chain was applied.
    tips: BTreeMap<String, String>,
}

pub fn settings(store: &Path) -> Result<Settings> {
    let config = ProjectStore::at(store).load_global_config()?;
    let settings: Settings = config
        .extras
        .get("sync")
        .cloned()
        .map(serde_yaml_ng::from_value)
        .transpose()?
        .unwrap_or_default();
    if settings.relay_interval == 0 {
        return Err(invalid("sync.relay_interval must be positive"));
    }
    if settings.log_interval == 0 || !(1..=1024 * 1024).contains(&settings.log_tail_bytes) {
        return Err(invalid(
            "sync.log_interval must be positive and sync.log_tail_bytes must be 1..=1048576",
        ));
    }
    if settings
        .relay_remote
        .as_ref()
        .is_some_and(|s| s.is_empty() || s.starts_with('-'))
    {
        return Err(invalid("invalid sync.relay_remote"));
    }
    Ok(settings)
}

/// Fresh relay contact is availability, not an HTTP launch endpoint. Consumers
/// resolving `auto` can combine this with a successful direct ONLINE probe.
pub fn available(store: &Path, node: &str, now: u64) -> Result<bool> {
    let config = settings(store)?;
    let state: BTreeMap<String, Heartbeat> = load(&store.join("sync/relay-presence.json"))?;
    let local = Journal::open(store)?.node;
    Ok(config
        .peers
        .iter()
        .any(|p| p.node.node_id == node && p.node.store_id == local.store_id)
        && state.get(node).is_some_and(|h| {
            h.store_id == local.store_id
                && h.at_ms <= now
                && now - h.at_ms <= config.relay_interval.saturating_mul(2000)
        }))
}

// File-backed stdio avoids pipe deadlocks and lets us kill a stalled git/age
// process group, including ssh children, before the worker can get stuck.
fn execute(mut command: Command, input: &[u8]) -> Result<Vec<u8>> {
    let mut stdin = tempfile::tempfile()?;
    stdin.write_all(input)?;
    use std::io::{Seek, SeekFrom};
    stdin.seek(SeekFrom::Start(0))?;
    let mut stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid("SYNC_RELAY_TIMEOUT"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if !status.success() {
        // Remote URLs can embed credentials; never report subprocess stderr.
        return Err(invalid(&format!(
            "SYNC_RELAY_FAILED: subprocess exited {status}"
        )));
    }
    stdout.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    stdout.read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn git(repo: &Path, args: &[&str], input: &[u8]) -> Result<String> {
    String::from_utf8(git_bytes(repo, args, input)?)
        .map_err(|_| invalid("invalid relay git output"))
}

fn git_bytes(repo: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.ext.allow=never",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "kanban4ai relay")
        .env("GIT_AUTHOR_EMAIL", "relay@kanban4ai.local")
        .env("GIT_COMMITTER_NAME", "kanban4ai relay")
        .env("GIT_COMMITTER_EMAIL", "relay@kanban4ai.local");
    execute(command, input)
}
fn tip(repo: &Path, reference: &str) -> Result<Option<String>> {
    let refs = git(
        repo,
        &["for-each-ref", "--format=%(objectname)", reference],
        b"",
    )?;
    Ok(refs.lines().next().map(str::to_owned))
}
fn node_component(node: &str) -> Result<()> {
    if node.is_empty() || !node.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(invalid("invalid relay node ID"));
    }
    Ok(())
}

/// Existing key or key generated when encryption is enabled. Enabling before
/// pairing lets the authenticated pairing exchange distribute public keys.
pub fn recipient(store: &Path) -> Result<Option<String>> {
    let key = store.join("sync/relay-age.key");
    if !key.exists() && !settings(store)?.relay_encrypt {
        return Ok(None);
    }
    let _lock = lock_file_reentrant(&store.join("sync/relay-key.lock"))?;
    if !key.exists() {
        let bytes = execute(Command::new("age-keygen"), b"")?;
        atomic_write_text_raw(
            &key,
            std::str::from_utf8(&bytes).map_err(|_| invalid("invalid age key"))?,
        )?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600))?;
    }
    let mut command = Command::new("age-keygen");
    command.arg("-y").arg(&key);
    Ok(Some(
        String::from_utf8(execute(command, b"")?)
            .map_err(|_| invalid("invalid age recipient"))?
            .trim()
            .to_owned(),
    ))
}
fn encrypt(store: &Path, config: &Settings, plain: &[u8]) -> Result<Vec<u8>> {
    let own = recipient(store)?.ok_or_else(|| invalid("missing relay age key"))?;
    let peers: Vec<Recipient> = match fs::read_to_string(store.join("sync/peers.yaml")) {
        Ok(text) => serde_yaml_ng::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let mut command = Command::new("age");
    command.args(["--armor", "--encrypt", "--recipient", &own]);
    for peer in &config.peers {
        let key = peers
            .iter()
            .find(|p| p.node_id == peer.node.node_id)
            .and_then(|p| p.relay_recipient.as_ref())
            .ok_or_else(|| {
                invalid("SYNC_RELAY_KEY_MISSING: re-pair after enabling relay encryption")
            })?;
        command.arg("--recipient").arg(key);
    }
    execute(command, plain)
}
fn read_batch(store: &Path, repo: &Path, commit: &str) -> Result<(Heartbeat, Vec<Operation>)> {
    let files = git(repo, &["ls-tree", "--name-only", commit], b"")?;
    let batch = if files.lines().any(|name| name == "batch.age") {
        "batch.age"
    } else {
        "batch.jsonl"
    };
    let plain = match batch {
        "batch.jsonl" => git(repo, &["show", &format!("{commit}:batch.jsonl")], b"")?.into_bytes(),
        "batch.age" => {
            let data = git(repo, &["show", &format!("{commit}:batch.age")], b"")?;
            let mut command = Command::new("age");
            command
                .args(["--decrypt", "--identity"])
                .arg(store.join("sync/relay-age.key"));
            execute(command, data.as_bytes())?
        }
        _ => return Err(invalid("invalid relay tree")),
    };
    let text = std::str::from_utf8(&plain).map_err(|_| invalid("invalid relay batch"))?;
    let mut lines = text.lines();
    let heartbeat = serde_json::from_str(lines.next().ok_or_else(|| invalid("empty relay batch"))?)
        .map_err(|e| invalid(&e.to_string()))?;
    let operations = lines
        .map(|line| serde_json::from_str(line).map_err(|e| invalid(&e.to_string())))
        .collect::<Result<Vec<Operation>>>()?;
    let journal = Journal::open(store)?;
    for hash in operations
        .iter()
        .filter_map(Operation::blob_hash)
        .collect::<BTreeSet<_>>()
    {
        let name = if batch == "batch.age" {
            format!("blob-{hash}.age")
        } else {
            format!("blob-{hash}")
        };
        let bytes = git_bytes(repo, &["show", &format!("{commit}:{name}")], b"")?;
        let bytes = if batch == "batch.age" {
            let mut command = Command::new("age");
            command
                .args(["--decrypt", "--identity"])
                .arg(store.join("sync/relay-age.key"));
            execute(command, &bytes)?
        } else {
            bytes
        };
        journal.put_blob(hash, &bytes)?;
    }
    Ok((heartbeat, operations))
}
fn remotes(store: &Path, config: &Settings) -> Result<BTreeSet<String>> {
    if let Some(remote) = &config.relay_remote {
        return Ok(BTreeSet::from([remote.clone()]));
    }
    let mut remotes = BTreeSet::new();
    for project in ProjectStore::at(store).list()? {
        if !project.work_path.is_dir() {
            continue;
        }
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&project.work_path)
            .args(["remote", "get-url", "origin"]);
        // Projects without git/origin are not relay endpoints.
        if let Ok(output) = execute(command, b"") {
            let remote = String::from_utf8(output).map_err(|_| invalid("invalid git remote"))?;
            let remote = remote.trim();
            if !remote.is_empty() && !remote.starts_with('-') {
                // Relative local remotes are relative to the project's work dir.
                let path = project.work_path.join(remote);
                remotes.insert(if path.exists() {
                    path.to_string_lossy().into_owned()
                } else {
                    remote.into()
                });
            }
        }
    }
    Ok(remotes)
}

/// One forced cycle (CLI), or a worker tick respecting relay_interval. Always
/// publishes a heartbeat, even if this node has no new journal operations.
pub fn cycle(store: &Path, force: bool) -> Result<usize> {
    let config = settings(store)?;
    if config.peers.is_empty() {
        return Ok(0);
    }
    let journal = Journal::open(store)?;
    journal.reconcile()?;
    node_component(&journal.node.node_id)?;
    let _lock = lock_file_reentrant(&store.join("sync/relay.lock"))?;
    let endpoints = remotes(store, &config)?;
    let mut completed = 0;
    let mut failure = None;
    for remote in endpoints {
        let result = exchange(store, &journal, &config, &remote, force);
        match result {
            Ok(true) => completed += 1,
            Ok(false) => (),
            Err(e) => failure = Some(e),
        }
    }
    if completed == 0
        && let Some(error) = failure
    {
        return Err(error);
    }
    Ok(completed)
}
fn exchange(
    store: &Path,
    journal: &Journal,
    config: &Settings,
    remote: &str,
    force: bool,
) -> Result<bool> {
    let digest = format!("{:x}", Sha256::digest(remote.as_bytes()));
    let directory = store.join("sync/relay").join(digest);
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(store.join("sync/relay"), fs::Permissions::from_mode(0o700))?;
    }
    let last: u64 = load(&directory.join("last.json"))?;
    let sync_due =
        force || now_ms().saturating_sub(last) >= config.relay_interval.saturating_mul(1000);
    let repo = directory.join("git");
    if !repo.exists() {
        git(&repo, &["init", "--bare", "--quiet"], b"")?;
    }
    let logs_last: u64 = load(&directory.join("logs-last.json"))?;
    let logs_due =
        force || now_ms().saturating_sub(logs_last) >= config.log_interval.saturating_mul(1000);
    if !sync_due {
        if logs_due {
            exchange_logs(store, journal, config, remote, &repo)?;
            atomic_write_text_raw(&directory.join("logs-last.json"), &json(&now_ms())?)?;
        }
        return Ok(logs_due);
    }
    let mut cache: Cache = load(&directory.join("cache.json"))?;
    git(
        &repo,
        &[
            "fetch",
            "--quiet",
            "--prune",
            "--no-tags",
            remote,
            &format!("+{PREFIX}*:{FETCHED}*"),
        ],
        b"",
    )?;
    let own_ref = format!("{PREFIX}{}", journal.node.node_id);
    let remote_ref = format!("{FETCHED}{}", journal.node.node_id);
    let remote_tip = tip(&repo, &remote_ref)?;
    let local_tip = tip(&repo, &own_ref)?;
    let parent = match (&local_tip, &remote_tip) {
        (Some(local), Some(remote)) if local != remote => {
            // A pending local push can be retried, but a forked writer cannot
            // overwrite an existing ref, even if it has the same node UUID.
            git(&repo, &["merge-base", "--is-ancestor", remote, local], b"")?;
            Some(local.clone())
        }
        (Some(local), _) => Some(local.clone()),
        (_, Some(remote)) => Some(remote.clone()),
        _ => None,
    };
    let published = parent
        .as_ref()
        .map(|p| read_batch(store, &repo, p))
        .transpose()?
        .map(|(h, _)| h.vector.get(&journal.node.node_id).copied().unwrap_or(0))
        .unwrap_or(0);
    let state = journal.state()?;
    if state
        .compacted
        .get(&journal.node.node_id)
        .copied()
        .unwrap_or(0)
        > published
    {
        return Err(invalid(
            "SYNC_RELAY_GAP: unpublished journal prefix was compacted",
        ));
    }
    let mut operations: Vec<_> = state
        .operations
        .values()
        .filter(|op| op.hlc.2 == journal.node.node_id)
        .filter_map(|op| match sequence(op) {
            Ok(seq) if seq > published => Some(Ok(op)),
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        })
        .collect::<Result<_>>()?;
    operations.sort_by_key(|op| sequence(op).expect("validated sequence"));
    for operation in &operations {
        apply::validate(operation, &journal.node.store_id)?;
    }
    let heartbeat = Heartbeat {
        node_id: journal.node.node_id.clone(),
        store_id: journal.node.store_id.clone(),
        at_ms: now_ms(),
        vector: state.vector,
    };
    let mut batch = json(&heartbeat)?;
    batch.push('\n');
    let mut tree_entries = String::new();
    for hash in operations
        .iter()
        .filter_map(|op| op.blob_hash())
        .collect::<BTreeSet<_>>()
    {
        let bytes = journal.blob(hash)?;
        let (name, bytes) = if config.relay_encrypt {
            (format!("blob-{hash}.age"), encrypt(store, config, &bytes)?)
        } else {
            (format!("blob-{hash}"), bytes)
        };
        let object = git(&repo, &["hash-object", "-w", "--stdin"], &bytes)?;
        tree_entries.push_str(&format!("100644 blob {}\t{name}\n", object.trim()));
    }
    for operation in operations {
        batch.push_str(&json(operation)?);
        batch.push('\n');
    }
    let (name, content) = if config.relay_encrypt {
        ("batch.age", encrypt(store, config, batch.as_bytes())?)
    } else {
        ("batch.jsonl", batch.into_bytes())
    };
    let blob = git(&repo, &["hash-object", "-w", "--stdin"], &content)?;
    tree_entries.push_str(&format!("100644 blob {}\t{name}\n", blob.trim()));
    let tree = git(&repo, &["mktree"], tree_entries.as_bytes())?;
    let mut args = vec!["commit-tree", tree.trim()];
    if let Some(parent) = &parent {
        args.extend(["-p", parent]);
    }
    let commit = git(&repo, &args, b"kanban sync batch\n")?;
    git(&repo, &["update-ref", &own_ref, commit.trim()], b"")?;
    // No '+' or --force. Only our own ref is ever pushed.
    git(
        &repo,
        &["push", "--quiet", remote, &format!("{own_ref}:{own_ref}")],
        b"",
    )?;
    git(
        &repo,
        &[
            "fetch",
            "--quiet",
            "--prune",
            "--no-tags",
            remote,
            &format!("+{PREFIX}*:{FETCHED}*"),
        ],
        b"",
    )?;
    for peer in &config.peers {
        node_component(&peer.node.node_id)?;
        if peer.node.node_id == journal.node.node_id || peer.node.store_id != journal.node.store_id
        {
            continue;
        }
        let reference = format!("{FETCHED}{}", peer.node.node_id);
        let Some(head) = tip(&repo, &reference)? else {
            continue;
        };
        let old = cache.tips.get(&peer.node.node_id);
        if old == Some(&head) {
            continue;
        }
        if let Some(old) = old {
            git(&repo, &["merge-base", "--is-ancestor", old, &head], b"")?;
        }
        let range = old.map_or(head.clone(), |old| format!("{old}..{head}"));
        // Start at the newest batch. An authenticated HTTP bootstrap may
        // already cover old ciphertext written before this peer was paired.
        // Stop once the contiguous operation prefix is covered; no old private
        // key is needed, and known operations are not replayed needlessly.
        let commits = git(&repo, &["rev-list", "--topo-order", &range], b"")?;
        let known = journal
            .vector()?
            .get(&peer.node.node_id)
            .copied()
            .unwrap_or(0);
        let mut through = known;
        let mut received = BTreeSet::new();
        let mut incoming = Vec::new();
        let mut latest: Option<Heartbeat> = None;
        for commit in commits.lines() {
            let (heartbeat, operations) = read_batch(store, &repo, commit)?;
            if heartbeat.node_id != peer.node.node_id
                || heartbeat.store_id != journal.node.store_id
                || heartbeat.at_ms.saturating_sub(now_ms()) > config.max_skew.saturating_mul(1000)
                || operations.iter().any(|op| op.hlc.2 != peer.node.node_id)
            {
                return Err(invalid("invalid relay batch owner/store/clock"));
            }
            let published = heartbeat
                .vector
                .get(&peer.node.node_id)
                .copied()
                .unwrap_or(0);
            for operation in operations {
                apply::validate(&operation, &journal.node.store_id)?;
                let sequence = sequence(&operation)?;
                if sequence > published {
                    return Err(invalid("invalid relay batch vector"));
                }
                if sequence > known {
                    received.insert(sequence);
                    incoming.push(operation);
                }
            }
            while let Some(next) = through.checked_add(1) {
                if !received.remove(&next) {
                    break;
                }
                through = next;
            }
            let target = latest
                .get_or_insert(heartbeat)
                .vector
                .get(&peer.node.node_id)
                .copied()
                .unwrap_or(0);
            if through >= target {
                break;
            }
        }
        let heartbeat = latest.ok_or_else(|| invalid("empty relay chain"))?;
        if through
            < heartbeat
                .vector
                .get(&peer.node.node_id)
                .copied()
                .unwrap_or(0)
        {
            return Err(invalid("SYNC_RELAY_GAP: missing operations in relay chain"));
        }
        journal.apply_remote(&incoming, config.max_skew.saturating_mul(1000))?;
        {
            let _sync = lock_file_reentrant(&store.join("sync/.lock"))?;
            let path = store.join("sync/relay-presence.json");
            let mut presence: BTreeMap<String, Heartbeat> = load(&path)?;
            if presence
                .get(&heartbeat.node_id)
                .is_none_or(|old| old.at_ms <= heartbeat.at_ms)
            {
                presence.insert(heartbeat.node_id.clone(), heartbeat);
                atomic_write_text_raw(&path, &json(&presence)?)?;
            }
        }
        cache.tips.insert(peer.node.node_id.clone(), head);
        atomic_write_text_raw(&directory.join("cache.json"), &json(&cache)?)?;
    }
    if logs_due {
        exchange_logs(store, journal, config, remote, &repo)?;
        atomic_write_text_raw(&directory.join("logs-last.json"), &json(&now_ms())?)?;
    }
    atomic_write_text_raw(&directory.join("last.json"), &json(&now_ms())?)?;
    Ok(true)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogEnvelope {
    store_id: String,
    snapshot: crate::core::runner_log::Snapshot,
}

fn read_log(store: &Path, repo: &Path, commit: &str, limit: usize) -> Result<LogEnvelope> {
    let files = git(repo, &["ls-tree", "--name-only", commit], b"")?;
    let name = match files.trim() {
        "log.json" => "log.json",
        "log.age" => "log.age",
        _ => return Err(invalid("invalid relay log tree")),
    };
    let object = format!("{commit}:{name}");
    let size: usize = git(repo, &["cat-file", "-s", &object], b"")?
        .trim()
        .parse()
        .map_err(|_| invalid("invalid relay log size"))?;
    // JSON escaping can expand a byte sixfold; armor adds further overhead.
    let maximum = limit.saturating_mul(12).saturating_add(16 * 1024);
    if size > maximum {
        return Err(invalid("relay log exceeds configured tail bound"));
    }
    let data = git(repo, &["show", &object], b"")?;
    let plain = if name == "log.age" {
        let mut command = Command::new("age");
        command
            .args(["--decrypt", "--identity"])
            .arg(store.join("sync/relay-age.key"));
        execute(command, data.as_bytes())?
    } else {
        data.into_bytes()
    };
    if plain.len() > maximum {
        return Err(invalid("relay log exceeds configured tail bound"));
    }
    serde_json::from_slice(&plain).map_err(|e| invalid(&e.to_string()))
}

fn fetch_logs(repo: &Path, remote: &str) -> Result<()> {
    git(
        repo,
        &[
            "fetch",
            "--quiet",
            "--prune",
            "--no-tags",
            remote,
            &format!("+{LOG_PREFIX}*:{LOG_FETCHED}*"),
        ],
        b"",
    )?;
    Ok(())
}

fn exchange_logs(
    store: &Path,
    journal: &Journal,
    config: &Settings,
    remote: &str,
    repo: &Path,
) -> Result<()> {
    use crate::core::runner_log::{cache_snapshot, local_tail, validate_snapshot};
    use crate::core::storage::Storage;

    fetch_logs(repo, remote)?;
    let state = journal.state()?;
    for (alias, project) in &state.project_uids {
        node_component(project)?;
        node_component(alias)?;
        let board = store.join("projects").join(alias);
        if !board.is_dir() {
            continue;
        }
        for task in Storage::new(&board).list_tasks(None)? {
            // A mirrored task has no local session, and therefore cannot publish
            // another runner's log. local_tail also validates the run owner.
            if task.session.is_none() {
                continue;
            }
            let snapshot = match local_tail(&board, &task.id) {
                Ok(snapshot) => snapshot,
                Err(_) => continue,
            };
            if snapshot.node_id != journal.node.node_id || snapshot.project != *project {
                return Err(invalid("invalid local relay log owner/project"));
            }
            let suffix = format!("{}/{}/{}", snapshot.node_id, project, snapshot.task);
            let reference = format!("{LOG_PREFIX}{suffix}");
            let fetched = format!("{LOG_FETCHED}{suffix}");
            let remote_tip = tip(repo, &fetched)?;
            let local_tip = tip(repo, &reference)?;
            if remote_tip.is_some() && local_tip != remote_tip {
                return Err(invalid(
                    "SYNC_RELAY_LOG_WRITER_CONFLICT: node log ref changed",
                ));
            }
            let envelope = LogEnvelope {
                store_id: journal.node.store_id.clone(),
                snapshot,
            };
            let plain = json(&envelope)?.into_bytes();
            let (name, content) = if config.relay_encrypt {
                ("log.age", encrypt(store, config, &plain)?)
            } else {
                ("log.json", plain)
            };
            let blob = git(repo, &["hash-object", "-w", "--stdin"], &content)?;
            let tree = git(
                repo,
                &["mktree"],
                format!("100644 blob {}\t{name}\n", blob.trim()).as_bytes(),
            )?;
            // No parent: only the bounded current snapshot is reachable.
            let commit = git(repo, &["commit-tree", tree.trim()], b"kanban runner tail\n")?;
            let expected = remote_tip.as_deref().unwrap_or("");
            git(
                repo,
                &[
                    "push",
                    "--quiet",
                    &format!("--force-with-lease={reference}:{expected}"),
                    remote,
                    &format!("{}:{reference}", commit.trim()),
                ],
                b"",
            )?;
            git(repo, &["update-ref", &reference, commit.trim()], b"")?;
        }
    }
    fetch_logs(repo, remote)?;
    let refs = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            LOG_FETCHED,
        ],
        b"",
    )?;
    for line in refs.lines() {
        let Some((reference, commit)) = line.split_once(' ') else {
            return Err(invalid("invalid relay log reference"));
        };
        let Some(suffix) = reference.strip_prefix(LOG_FETCHED) else {
            continue;
        };
        let parts: Vec<_> = suffix.split('/').collect();
        if parts.len() != 3 {
            continue;
        }
        let (node, project, task) = (parts[0], parts[1], parts[2]);
        if node == journal.node.node_id
            || !config.peers.iter().any(|peer| {
                peer.node.node_id == node && peer.node.store_id == journal.node.store_id
            })
        {
            continue;
        }
        node_component(node)?;
        node_component(project)?;
        let envelope = read_log(store, repo, commit, config.log_tail_bytes)?;
        if envelope.store_id != journal.node.store_id
            || envelope.snapshot.node_id != node
            || envelope.snapshot.project != project
            || envelope.snapshot.task != task
            || envelope.snapshot.at_ms.saturating_sub(now_ms())
                > config.max_skew.saturating_mul(1000)
        {
            return Err(invalid("invalid relay log owner/store/project/task/clock"));
        }
        // A log ref can outlive its run or arrive before the slower sync stream.
        // Such snapshots must not be cached, but are not transport failures.
        if validate_snapshot(store, &envelope.snapshot).is_err() {
            continue;
        }
        // Performs project/task existence and latest run-owner validation before
        // touching the cache; aliases and task IDs alone are never identities.
        cache_snapshot(store, &envelope.snapshot)?;
    }
    Ok(())
}
