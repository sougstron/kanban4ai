//! Operation exchange between paired nodes (section 10). Every paired peer
//! gets a background worker: catch-up over `/sync/vector` + `/sync/ops`,
//! then a live SSE stream plus a push after each local write. Store access
//! stays in `core::sync`; this module only moves pages over HTTP.
use super::discovery::{self, Config, Credential, PeerCard, Presence};
use super::*;
use crate::core::sync::{Journal, Operation, Page, Snapshot, VersionVector, install_snapshot};
use axum::Extension;
use axum::extract::DefaultBodyLimit;
use axum::http::Uri;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::LazyLock;

const PAGE: usize = 200;
const KEEPALIVE: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Local journal/state writes from any process on this machine.
const POLL: Duration = Duration::from_millis(500);

static CHANGED: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);
static KICK: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);
static STOPPING: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::channel(false).0);

/// Wake idle workers: a peer said hello or reappeared in mDNS.
pub(super) fn kick() {
    KICK.send_modify(|n| *n = n.wrapping_add(1));
}
fn changed() {
    CHANGED.send_modify(|n| *n = n.wrapping_add(1));
}

#[derive(Serialize, Deserialize)]
struct VectorReply {
    node_id: String,
    store_id: String,
    /// No registered projects: a bootstrap target rather than a store mismatch.
    empty: bool,
    vector: VersionVector,
    /// Project UID -> unused task IDs leased to the replying node.
    remaining: BTreeMap<String, u64>,
}
#[derive(Serialize, Deserialize)]
struct Push {
    operations: Vec<Operation>,
}
#[derive(Serialize, Deserialize)]
struct Ack {
    vector: VersionVector,
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| invalid(e.to_string()))?
}

/// `id:seq,id:seq`: node IDs are UUIDs, so the cursor needs no URL encoding.
fn encode_vector(vector: &VersionVector) -> String {
    vector
        .iter()
        .filter(|(_, seq)| **seq > 0)
        .map(|(node, seq)| format!("{node}:{seq}"))
        .collect::<Vec<_>>()
        .join(",")
}
fn decode_vector(text: &str) -> Result<VersionVector> {
    let mut vector = VersionVector::new();
    for item in text.split(',').filter(|item| !item.is_empty()) {
        let (node, seq) = item
            .rsplit_once(':')
            .ok_or_else(|| invalid("invalid version vector"))?;
        if node.is_empty() || !node.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(invalid("invalid version vector node"));
        }
        let seq = seq
            .parse()
            .map_err(|_| invalid("invalid version vector sequence"))?;
        vector.insert(node.to_owned(), seq);
    }
    Ok(vector)
}
fn query<'a>(uri: &'a Uri, name: &str) -> Option<&'a str> {
    uri.query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}
/// `a` has seen everything `b` has seen.
fn covers(a: &VersionVector, b: &VersionVector) -> bool {
    b.iter()
        .all(|(node, seq)| a.get(node).copied().unwrap_or(0) >= *seq)
}

fn max_skew_ms(root: &Path) -> Result<u64> {
    Ok(discovery::config(root)?.max_skew.saturating_mul(1000))
}

/// Apply remote operations, skipping what the contiguous vector already holds.
async fn apply(root: &Path, operations: Vec<Operation>) -> Result<VersionVector> {
    let root = root.to_path_buf();
    let (vector, applied) = blocking(move || {
        let journal = Journal::open(&root)?;
        let known = journal.vector()?;
        let fresh: Vec<_> = operations
            .into_iter()
            .filter(|op| {
                op.op
                    .rsplit_once(':')
                    .and_then(|(_, seq)| seq.parse::<u64>().ok())
                    .is_none_or(|seq| seq > known.get(&op.hlc.2).copied().unwrap_or(0))
            })
            .collect();
        if fresh.is_empty() {
            return Ok((known, false));
        }
        journal.apply_remote(&fresh, max_skew_ms(&root)?)?;
        Ok((journal.vector()?, true))
    })
    .await?;
    if applied {
        changed();
    }
    Ok(vector)
}

fn failure(e: KanbanError) -> Response {
    let text = e.to_string();
    for code in [
        "SYNC_STORE_MISMATCH",
        "SYNC_CLOCK_SKEW",
        "SYNC_LEASE_CONFLICT",
    ] {
        if text.contains(code) {
            return error(StatusCode::CONFLICT, code, &text);
        }
    }
    match e {
        KanbanError::Invalid(_) | KanbanError::Yaml(_) => {
            error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", &text)
        }
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &text),
    }
}

pub(super) fn routes(root: PathBuf) -> Router {
    Router::new()
        .route("/sync/vector", get(vector))
        .route(
            "/sync/ops",
            get(pull)
                .post(receive)
                .layer(DefaultBodyLimit::max(256 << 20)),
        )
        .route("/sync/stream", get(stream))
        .route("/sync/snapshot", get(snapshot))
        .with_state(root)
}

async fn vector(State(root): State<PathBuf>, Extension(hash): Extension<String>) -> Response {
    let result = blocking(move || {
        // Every authenticated sync request is a fresh contact (hello).
        if let Ok(id) = discovery::caller(&root, &hash) {
            discovery::update_presence(&root, &id, |p| {
                p.last_seen = Some(chrono::Utc::now());
                p.bye = None;
            })?;
        }
        let journal = Journal::open(&root)?;
        let mut remaining = BTreeMap::new();
        for (alias, uid) in journal.project_uids()? {
            remaining.insert(uid, journal.remaining_primary_task_ids(&alias)?);
        }
        Ok(VectorReply {
            node_id: journal.node.node_id.clone(),
            store_id: journal.node.store_id.clone(),
            empty: ProjectStore::at(&root).list()?.is_empty(),
            vector: journal.vector()?,
            remaining,
        })
    })
    .await;
    match result {
        Ok(reply) => Json(reply).into_response(),
        Err(e) => failure(e),
    }
}

async fn pull(State(root): State<PathBuf>, uri: Uri) -> Response {
    let result = async {
        let after = decode_vector(query(&uri, "after").unwrap_or(""))?;
        let limit = match query(&uri, "limit") {
            Some(limit) => limit
                .parse::<usize>()
                .ok()
                .filter(|limit| (1..=1000).contains(limit))
                .ok_or_else(|| invalid("limit must be 1..=1000"))?,
            None => PAGE,
        };
        blocking(move || Journal::open(&root)?.operations_after(&after, limit)).await
    }
    .await;
    match result {
        Ok(page) => Json(page).into_response(),
        Err(e) => failure(e),
    }
}

async fn receive(State(root): State<PathBuf>, Json(push): Json<Push>) -> Response {
    match apply(&root, push.operations).await {
        Ok(vector) => Json(Ack { vector }).into_response(),
        Err(e) => failure(e),
    }
}

async fn snapshot(State(root): State<PathBuf>) -> Response {
    match blocking(move || Journal::open(&root)?.snapshot()).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(e) => failure(e),
    }
}

/// SSE: every operation after `after`, then live pages as the store changes.
async fn stream(State(root): State<PathBuf>, uri: Uri) -> Response {
    let after = match decode_vector(query(&uri, "after").unwrap_or("")) {
        Ok(after) => after,
        Err(e) => return failure(e),
    };
    // Subscribe before the first read so no change can slip between them.
    let changes = CHANGED.subscribe();
    let stopping = STOPPING.subscribe();
    let body = futures_util::stream::unfold(
        (root, after, changes, stopping),
        |(root, mut cursor, mut changes, mut stopping)| async move {
            loop {
                if *stopping.borrow() {
                    return None;
                }
                let read = root.clone();
                let after = cursor.clone();
                let page: Page =
                    match blocking(move || Journal::open(&read)?.operations_after(&after, PAGE))
                        .await
                    {
                        Ok(page) => page,
                        Err(e) => {
                            eprintln!("sync stream: {e}");
                            return None;
                        }
                    };
                if !page.operations.is_empty() {
                    cursor = page.next;
                    let data = serde_json::to_string(&page.operations).ok()?;
                    let event = format!("event: ops\ndata: {data}\n\n");
                    return Some((
                        Ok::<_, Infallible>(event),
                        (root, cursor, changes, stopping),
                    ));
                }
                tokio::select! {
                    result = tokio::time::timeout(KEEPALIVE, changes.changed()) => match result {
                        Ok(Ok(())) => continue,
                        Ok(Err(_)) => return None,
                        Err(_) => return Some((
                            Ok(": keepalive\n\n".to_owned()),
                            (root, cursor, changes, stopping),
                        )),
                    },
                    _ = stopping.changed() => return None,
                }
            }
        },
    );
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        axum::body::Body::from_stream(body),
    )
        .into_response()
}

/// An authenticated route to one peer at the first address that answered.
struct Link {
    root: PathBuf,
    peer: String,
    name: String,
    base: String,
    token: String,
    http: reqwest::Client,
    streaming: reqwest::Client,
}

impl Link {
    async fn connect(root: &Path, config: &Config, peer: &PeerCard) -> Result<(Self, VectorReply)> {
        let credentials: Vec<Credential> = discovery::load(&root.join("sync/peers.yaml"))?;
        let secret = credentials
            .iter()
            .find(|c| c.node_id == peer.node.node_id)
            .ok_or_else(|| invalid("NODE_AUTH_FAILED: peer is not paired"))?;
        let presence: BTreeMap<String, Presence> =
            discovery::load(&root.join("sync/presence.yaml"))?;
        let presence = presence
            .get(&peer.node.node_id)
            .cloned()
            .unwrap_or_default();
        let connect = Duration::from_secs(config.probe_timeout);
        let mut last = invalid("NODE_UNREACHABLE: no address answered");
        for address in discovery::peer_addresses(root, peer, Some(secret), &presence, config)? {
            let http = match discovery::peer_client(root, &address, connect, Some(REQUEST_TIMEOUT))
            {
                Ok(http) => http,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let link = Link {
                root: root.to_path_buf(),
                peer: peer.node.node_id.clone(),
                name: peer.node.name.clone(),
                base: format!("http://{address}"),
                token: secret.token.clone(),
                http,
                streaming: discovery::peer_client(root, &address, connect, None)?,
            };
            match link.vector().await {
                Ok(reply) if reply.node_id == link.peer => {
                    discovery::update_presence(root, &link.peer, |p| {
                        p.last_seen = Some(chrono::Utc::now());
                        p.bye = None;
                    })?;
                    return Ok((link, reply));
                }
                Ok(_) => last = invalid("NODE_AUTH_FAILED: another node answered"),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T> {
        let reply = request
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| invalid(format!("{}: {e}", self.name)))?;
        if !reply.status().is_success() {
            let status = reply.status();
            let body: serde_json::Value = reply.json().await.unwrap_or_default();
            let code = body["error"]["code"].as_str().unwrap_or("HTTP_ERROR");
            let message = body["error"]["message"].as_str().unwrap_or("");
            return Err(invalid(format!(
                "{}: {code} ({status}) {message}",
                self.name
            )));
        }
        reply
            .json()
            .await
            .map_err(|e| invalid(format!("{}: {e}", self.name)))
    }

    async fn vector(&self) -> Result<VectorReply> {
        self.send(self.http.get(format!("{}/sync/vector", self.base)))
            .await
    }

    async fn post(&self, operations: Vec<Operation>) -> Result<VersionVector> {
        let ack: Ack = self
            .send(
                self.http
                    .post(format!("{}/sync/ops", self.base))
                    .json(&Push { operations }),
            )
            .await?;
        Ok(ack.vector)
    }

    /// Steps 2 of 10.2: page through everything the peer has and we lack.
    async fn pull(&self) -> Result<()> {
        let root = self.root.clone();
        let mut cursor = blocking(move || Journal::open(&root)?.vector()).await?;
        loop {
            let page: Page = self
                .send(self.http.get(format!(
                    "{}/sync/ops?after={}&limit={PAGE}",
                    self.base,
                    encode_vector(&cursor)
                )))
                .await?;
            if !page.operations.is_empty() {
                apply(&self.root, page.operations).await?;
            }
            if !page.more || page.next == cursor {
                return Ok(());
            }
            cursor = page.next;
        }
    }

    /// Step 3: send what the peer lacks; returns the peer's acknowledged vector.
    async fn push(&self, acknowledged: VersionVector) -> Result<VersionVector> {
        let mut acknowledged = acknowledged;
        let mut cursor = acknowledged.clone();
        loop {
            let root = self.root.clone();
            let after = cursor.clone();
            let page =
                blocking(move || Journal::open(&root)?.operations_after(&after, PAGE)).await?;
            if page.operations.is_empty() {
                break;
            }
            acknowledged = self.post(page.operations).await?;
            cursor = page.next;
            if !page.more {
                break;
            }
        }
        let (root, peer, ack) = (self.root.clone(), self.peer.clone(), acknowledged.clone());
        blocking(move || Journal::open(&root)?.confirm_id_leases(&peer, &ack)).await?;
        Ok(acknowledged)
    }

    /// Issue or renew task-ID grants once both replicas hold the same vector.
    async fn leases(
        &self,
        mut acknowledged: VersionVector,
        remaining: &BTreeMap<String, u64>,
    ) -> Result<()> {
        let root = self.root.clone();
        let (local, projects) = blocking(move || {
            let journal = Journal::open(&root)?;
            Ok((journal.vector()?, journal.project_uids()?))
        })
        .await?;
        if !covers(&acknowledged, &local) || !covers(&local, &acknowledged) {
            return Ok(());
        }
        for (alias, uid) in projects {
            let (root, peer, ack) = (self.root.clone(), self.peer.clone(), acknowledged.clone());
            let left = remaining.get(&uid).copied().unwrap_or(0);
            let grants = blocking(move || {
                let journal = Journal::open(&root)?;
                let tasks = root.join("projects").join(&alias).join(".kanban/tasks");
                if !tasks.is_dir() || !journal.issues_id_leases(&uid)? {
                    return Ok(Vec::new());
                }
                journal.prepare_id_leases(&alias, &peer, &ack, left)
            })
            .await;
            match grants {
                Ok(grants) if !grants.is_empty() => {
                    acknowledged = self.post(grants).await?;
                    let (root, peer, ack) =
                        (self.root.clone(), self.peer.clone(), acknowledged.clone());
                    blocking(move || Journal::open(&root)?.confirm_id_leases(&peer, &ack)).await?;
                }
                Ok(_) => (),
                // A write raced the handshake; the next cycle retries.
                Err(e) => eprintln!("sync {}: task ID lease: {e}", self.name),
            }
        }
        Ok(())
    }

    /// One catch-up cycle. `None` means the peer is waiting to bootstrap.
    async fn cycle(&self) -> Result<Option<VersionVector>> {
        loop {
            let reply = self.vector().await?;
            let root = self.root.clone();
            let (node, empty) = blocking(move || {
                Ok((
                    Journal::open(&root)?.node,
                    ProjectStore::at(&root).list()?.is_empty(),
                ))
            })
            .await?;
            if reply.store_id != node.store_id {
                // Two empty stores: the larger node_id adopts the other's UUID.
                if empty && (!reply.empty || node.node_id > reply.node_id) {
                    self.bootstrap(&reply.store_id).await?;
                    continue;
                }
                if reply.empty {
                    return Ok(None);
                }
                return Err(invalid(format!(
                    "{}: SYNC_STORE_MISMATCH: the peer holds another kanban4ai store",
                    self.name
                )));
            }
            self.pull().await?;
            let acknowledged = self.push(reply.vector).await?;
            self.leases(acknowledged, &reply.remaining).await?;
            // Leases may have advanced both vectors; report the fresh one.
            return Ok(Some(self.vector().await?.vector));
        }
    }

    async fn bootstrap(&self, store_id: &str) -> Result<()> {
        let snapshot: Snapshot = self
            .send(self.http.get(format!("{}/sync/snapshot", self.base)))
            .await?;
        if snapshot.store_id != store_id || snapshot.node_id != self.peer {
            return Err(invalid("snapshot does not match the peer"));
        }
        let root = self.root.clone();
        blocking(move || install_snapshot(&root, &snapshot)).await?;
        changed();
        println!("sync: replica bootstrapped from {}", self.name);
        Ok(())
    }

    /// Step 4: live operations both ways until the stream breaks.
    async fn live(&self, acknowledged: VersionVector, interval: Duration) -> Result<()> {
        let root = self.root.clone();
        let local = blocking(move || Journal::open(&root)?.vector()).await?;
        let mut response = self
            .streaming
            .get(format!(
                "{}/sync/stream?after={}",
                self.base,
                encode_vector(&local)
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| invalid(format!("{}: {e}", self.name)))?;
        if !response.status().is_success() {
            return Err(invalid(format!(
                "{}: stream {}",
                self.name,
                response.status()
            )));
        }
        let mut changes = CHANGED.subscribe();
        changes.borrow_and_update();
        let mut stopping = STOPPING.subscribe();
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        let mut acknowledged = acknowledged;
        let mut buffer = String::new();
        loop {
            tokio::select! {
                chunk = tokio::time::timeout(KEEPALIVE * 3, response.chunk()) => {
                    let chunk = match chunk {
                        Err(_) => return Err(invalid(format!("{}: stream idle", self.name))),
                        Ok(chunk) => chunk.map_err(|e| invalid(format!("{}: {e}", self.name)))?,
                    };
                    let Some(chunk) = chunk else { return Ok(()) };
                    buffer.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(end) = buffer.find("\n\n") {
                        let event: String = buffer.drain(..end + 2).collect();
                        for data in event.lines().filter_map(|l| l.strip_prefix("data: ")) {
                            let operations: Vec<Operation> = serde_json::from_str(data)
                                .map_err(|e| invalid(e.to_string()))?;
                            // The sender holds what it streams: skip echoing it back.
                            for op in &operations {
                                if let Some((node, seq)) = op.op.rsplit_once(':')
                                    && let Ok(seq) = seq.parse::<u64>()
                                {
                                    let entry = acknowledged.entry(node.to_owned()).or_default();
                                    *entry = (*entry).max(seq);
                                }
                            }
                            apply(&self.root, operations).await?;
                        }
                    }
                }
                result = changes.changed() => {
                    if result.is_err() { return Ok(()); }
                    acknowledged = self.push(acknowledged).await?;
                }
                _ = tick.tick() => {
                    match self.cycle().await? {
                        Some(vector) => acknowledged = vector,
                        None => return Ok(()),
                    }
                }
                _ = stopping.changed() => return Ok(()),
            }
        }
    }
}

/// Connect, catch up and stay live. `Ok(false)` once the peer is unpaired.
async fn session(root: &Path, peer_id: &str) -> Result<bool> {
    let config = discovery::config(root)?;
    let Some(peer) = config.peers.iter().find(|p| p.node.node_id == peer_id) else {
        return Ok(false);
    };
    // Writes wait in the journal until either LAN or tailnet is available.
    if !discovery::network_available(root, &config)? {
        return Ok(true);
    }
    let presence: BTreeMap<String, Presence> = discovery::load(&root.join("sync/presence.yaml"))?;
    if presence.get(peer_id).is_some_and(|p| {
        p.bye
            .as_ref()
            .is_some_and(|bye| p.last_seen.is_none_or(|seen| seen <= bye.at))
    }) {
        // NODE_POWERED_OFF until its hello or an mDNS re-announcement.
        return Ok(true);
    }
    let (link, _) = Link::connect(root, &config, peer).await?;
    let Some(acknowledged) = link.cycle().await? else {
        return Ok(true);
    };
    link.live(acknowledged, Duration::from_secs(config.interval))
        .await?;
    Ok(true)
}

async fn worker(root: PathBuf, peer: String) {
    let mut kicks = KICK.subscribe();
    let mut reported = None;
    loop {
        match session(&root, &peer).await {
            Ok(false) => return,
            Ok(true) => reported = None,
            Err(e) => {
                // Log transitions only; a powered-off peer would flood journald.
                let text = e.to_string();
                if reported.as_ref() != Some(&text) {
                    eprintln!("sync {peer}: {text}");
                    reported = Some(text);
                }
            }
        }
        let interval = discovery::config(&root).map_or(30, |c| c.interval);
        kicks.borrow_and_update();
        tokio::select! {
            _ = kicks.changed() => (),
            _ = tokio::time::sleep(Duration::from_secs(interval)) => (),
        }
    }
}

fn fingerprint(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

pub(super) struct Running {
    tasks: JoinSet<()>,
}
impl Running {
    pub(super) async fn stop(mut self) {
        STOPPING.send_replace(true);
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }
}

/// Start the poller for local writes and one worker per paired peer.
/// Peers paired while serve runs are picked up on the next interval or kick.
pub(super) fn start(root: &Path) -> Running {
    STOPPING.send_replace(false);
    let mut tasks = JoinSet::new();
    let state = root.join("sync/meta/state.json");
    tasks.spawn(async move {
        // Writers are other processes (TUI, agents' $KANBAN_CMD), so watch
        // the replicated state rather than an in-process event.
        let mut last = fingerprint(&state);
        loop {
            tokio::time::sleep(POLL).await;
            let now = fingerprint(&state);
            if now != last {
                last = now;
                changed();
            }
        }
    });
    let root = root.to_path_buf();
    tasks.spawn(async move {
        let mut workers: BTreeMap<String, tokio::task::JoinHandle<()>> = BTreeMap::new();
        let mut kicks = KICK.subscribe();
        loop {
            let config = discovery::config(&root);
            if let Ok(config) = &config {
                workers.retain(|_, handle| !handle.is_finished());
                for peer in &config.peers {
                    let id = peer.node.node_id.clone();
                    workers
                        .entry(id.clone())
                        .or_insert_with(|| tokio::spawn(worker(root.clone(), id)));
                }
            }
            let interval = config.map_or(30, |c| c.interval);
            tokio::select! {
                _ = kicks.changed() => (),
                _ = tokio::time::sleep(Duration::from_secs(interval)) => (),
            }
        }
        // Unreachable: the JoinSet aborts this task; workers end with it via drop.
    });
    Running { tasks }
}

/// One catch-up cycle with every paired peer (`serve --sync`, and the tail
/// push before bye). With `full == false` only pushes local operations.
pub(super) async fn once(root: &Path, full: bool) -> Result<Vec<(String, Result<()>)>> {
    let config = discovery::config(root)?;
    let mut results = Vec::new();
    if !discovery::network_available(root, &config)? {
        return Ok(results);
    }
    for peer in &config.peers {
        let result = async {
            let (link, reply) = Link::connect(root, &config, peer).await?;
            if full {
                link.cycle().await?;
            } else if reply.store_id == Journal::open(root)?.node.store_id {
                link.push(reply.vector).await?;
            }
            Ok(())
        }
        .await;
        results.push((peer.node.name.clone(), result));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::storage::{NewTask, Storage, atomic_write_text};

    fn node(role: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let id = Journal::open(dir.path()).unwrap().node.node_id;
        fs::write(
            dir.path().join("config.yaml"),
            format!("node:\n  node_id: {id}\n  name: {role}\n  role: {role}\n"),
        )
        .unwrap();
        Journal::open(dir.path()).unwrap();
        (dir, id)
    }

    /// Pair `local` with `remote`: `token` is what local presents to remote,
    /// `inbound` what remote presents to local.
    fn pair(local: &Path, remote: &Path, address: SocketAddr, token: &str, inbound: &str) {
        let peer = Journal::open(remote).unwrap().node;
        let own = Journal::open(local).unwrap().node;
        let role = |r| match r {
            crate::core::sync::NodeRole::Laptop => "laptop",
            crate::core::sync::NodeRole::Server => "server",
            crate::core::sync::NodeRole::Mobile => "mobile",
        };
        fs::write(
            local.join("config.yaml"),
            format!(
                "node:\n  node_id: {}\n  name: {}\n  role: {}\nsync:\n  peers:\n  - node_id: {}\n    name: {}\n    role: {}\n    store_id: {}\n    address: {address}\n",
                own.node_id, own.name, role(own.role), peer.node_id, peer.name, role(peer.role), peer.store_id
            ),
        )
        .unwrap();
        fs::write(
            local.join("sync/peers.yaml"),
            format!(
                "- node_id: {}\n  token: {token}\n  inbound_sha256: {}\n",
                peer.node_id,
                token_hash(inbound)
            ),
        )
        .unwrap();
        fs::write(
            local.join("sync/tokens.yaml"),
            format!("tokens:\n- kind: node\n  sha256: {}\n", token_hash(inbound)),
        )
        .unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn managed_magicdns_uses_local_socks_for_authenticated_http_and_streaming() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (local, _) = node("laptop");
            let (remote, remote_id) = node("server");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let destination = listener.local_addr().unwrap();
            let http_server = tokio::spawn(axum::serve(listener, router(remote.path().join("sync/tokens.yaml"))).into_future());
            let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let unreachable = closed.local_addr().unwrap();
            drop(closed);
            let token = "a".repeat(64);
            pair(local.path(), remote.path(), unreachable, &token, &"b".repeat(64));
            fs::write(remote.path().join("sync/tokens.yaml"),
                format!("tokens:\n- kind: node\n  sha256: {}\n", token_hash(&token))).unwrap();
            let socks = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = socks.local_addr().unwrap().port();
            let requested = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let recorded = requested.clone();
            let proxy = tokio::spawn(async move {
                loop {
                    let (mut incoming, _) = socks.accept().await.unwrap();
                    let recorded = recorded.clone();
                    tokio::spawn(async move {
                        let mut greeting = [0; 2];
                        incoming.read_exact(&mut greeting).await.unwrap();
                        assert_eq!(greeting[0], 5);
                        let mut methods = vec![0; greeting[1] as usize];
                        incoming.read_exact(&mut methods).await.unwrap();
                        assert!(methods.contains(&0));
                        incoming.write_all(&[5, 0]).await.unwrap();
                        let mut connect = [0; 5];
                        incoming.read_exact(&mut connect).await.unwrap();
                        assert_eq!(&connect[..4], &[5, 1, 0, 3], "SOCKS5h must resolve MagicDNS at the proxy");
                        let mut host = vec![0; connect[4] as usize];
                        incoming.read_exact(&mut host).await.unwrap();
                        let mut port = [0; 2];
                        incoming.read_exact(&mut port).await.unwrap();
                        recorded.lock().await.push((String::from_utf8(host).unwrap(), u16::from_be_bytes(port)));
                        let mut outgoing = tokio::net::TcpStream::connect(destination).await.unwrap();
                        incoming.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await.unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
                    });
                }
            });
            let config_text = fs::read_to_string(local.path().join("config.yaml")).unwrap();
            fs::write(local.path().join("config.yaml"), format!("{config_text}    tailscale_address: runner.example.ts.net:4242\nnet:\n  tailscale:\n    mode: managed\n    socks_port: {port}\n")).unwrap();
            let bin = local.path().join("net/bin");
            fs::create_dir_all(&bin).unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
            for (name, script) in [
                ("tailscaled", "#!/bin/sh\nwhile :; do sleep 1; done\n"),
                ("tailscale", "#!/bin/sh\necho '{\"BackendState\":\"Running\"}'\n"),
            ] {
                let path = bin.join(name);
                fs::write(&path, script).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let mut manager = crate::core::net::Manager::new(local.path()).unwrap();
            manager.tick().unwrap();
            let config = discovery::config(local.path()).unwrap();
            let (link, vector) = Link::connect(local.path(), &config, &config.peers[0]).await.unwrap();
            assert_eq!(vector.node_id, remote_id);
            assert_eq!(link.base, "http://runner.example.ts.net:4242");
            let mut response = tokio::time::timeout(KEEPALIVE + Duration::from_secs(3),
                link.streaming.get(format!("{}/sync/stream", link.base)).bearer_auth(&token).send()).await.unwrap().unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-type"], "text/event-stream");
            assert!(tokio::time::timeout(KEEPALIVE + Duration::from_secs(3), response.chunk()).await.unwrap().unwrap().is_some());
            drop(response);
            let rejected = link.http.get(format!("{}/sync/vector", link.base)).bearer_auth("revoked").send().await.unwrap();
            assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
            let routes = requested.lock().await;
            assert!(routes.iter().all(|(host, port)| host == "runner.example.ts.net" && *port == 4242));
            drop(routes);
            drop(link);
            drop(manager);
            proxy.abort();
            http_server.abort();
        });
    }

    async fn wait_for(what: &str, check: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !check() {
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[test]
    fn empty_server_bootstraps_then_leases_and_live_stream_converge() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (laptop, _) = node("laptop");
            let (server, server_id) = node("server");
            let a = laptop.path().to_path_buf();
            let b = server.path().to_path_buf();
            // Laptop data, including machine-local fields and a binary asset.
            let board = Storage::new(a.join("projects/demo"));
            board.init_board().unwrap();
            atomic_write_text(
                &board.project_path.join("project.yaml"),
                "id: demo\nname: Demo\nwork_path: /home/me/demo\ncreated_at: '2026-01-01T00:00:00'\n",
            )
            .unwrap();
            let first = board.create_task(NewTask::titled("first")).unwrap();
            let path = board.tasks_dir.join(format!("todo/{}.md", first.id));
            let text = fs::read_to_string(&path).unwrap();
            fs::write(&path, text.replacen("---\n", "---\nsession: ses-local\n", 1)).unwrap();
            fs::create_dir_all(board.project_path.join(".kanban/assets/images")).unwrap();
            fs::write(board.project_path.join(".kanban/assets/images/a.png"), [0u8, 159, 146, 150]).unwrap();

            let la = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let lb = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (aa, ab) = (la.local_addr().unwrap(), lb.local_addr().unwrap());
            let (t_ab, t_ba) = ("a".repeat(64), "b".repeat(64));
            pair(&a, &b, ab, &t_ab, &t_ba);
            pair(&b, &a, aa, &t_ba, &t_ab);
            let sa = tokio::spawn(axum::serve(la, router(a.join("sync/tokens.yaml"))).into_future());
            let sb = tokio::spawn(axum::serve(lb, router(b.join("sync/tokens.yaml"))).into_future());

            // The empty server adopts the laptop store.
            let config = discovery::config(&b).unwrap();
            let (to_laptop, _) = Link::connect(&b, &config, &config.peers[0]).await.unwrap();
            to_laptop.cycle().await.unwrap().unwrap();
            let (ja, jb) = (Journal::open(&a).unwrap(), Journal::open(&b).unwrap());
            assert_eq!(jb.node.store_id, ja.node.store_id);
            assert_eq!(jb.node.name, "server");
            let copy = Storage::new(b.join("projects/demo"));
            let task = fs::read_to_string(copy.tasks_dir.join(format!("todo/{}.md", first.id))).unwrap();
            assert!(task.contains("first") && !task.contains("ses-local"), "{task}");
            let project = fs::read_to_string(b.join("projects/demo/project.yaml")).unwrap();
            assert!(!project.contains("/home/me/demo"), "{project}");
            assert_eq!(
                fs::read(b.join("projects/demo/.kanban/assets/images/a.png")).unwrap(),
                [0u8, 159, 146, 150]
            );
            assert!(discovery::config(&b).unwrap().peers.len() == 1);
            assert!(install_snapshot(&a, &ja.snapshot().unwrap()).unwrap_err().to_string().contains("SYNC_STORE_MISMATCH"));

            // The server issues grants once both vectors match.
            let config = discovery::config(&a).unwrap();
            let (to_server, _) = Link::connect(&a, &config, &config.peers[0]).await.unwrap();
            let acknowledged = to_server.cycle().await.unwrap().unwrap();
            assert!(jb.remaining_task_ids("demo").unwrap() > 0);
            assert!(ja.remaining_task_ids("demo").unwrap() > 0);
            assert_eq!(ja.vector().unwrap(), jb.vector().unwrap());

            // Live: a server write reaches the laptop through the SSE stream.
            let live = tokio::spawn(async move { to_server.live(acknowledged, Duration::from_secs(3600)).await });
            let remote = copy.create_task(NewTask::titled("from server")).unwrap();
            changed();
            let (root, id) = (a.join("projects/demo"), remote.id.clone());
            wait_for("server task on laptop", move || Storage::new(&root).task_exists(&id)).await;
            assert_ne!(remote.id, board.create_task(NewTask::titled("from laptop")).unwrap().id);
            // ...and a laptop write is pushed to the server on the change signal.
            changed();
            let root = b.join("projects/demo");
            wait_for("laptop task on server", move || Storage::new(&root).list_tasks(None).unwrap().len() == 3).await;
            assert_eq!(ja.vector().unwrap(), jb.vector().unwrap());
            assert!(!server_id.is_empty());
            live.abort();
            sa.abort();
            sb.abort();
        });
    }

    #[test]
    fn vector_cursor_round_trips_and_rejects_injection() {
        let vector = VersionVector::from([
            ("6f1c0000-0000-4000-8000-000000000001".into(), 7),
            ("a1b2".into(), 0),
        ]);
        let text = encode_vector(&vector);
        assert_eq!(text, "6f1c0000-0000-4000-8000-000000000001:7");
        assert_eq!(
            decode_vector(&text).unwrap(),
            VersionVector::from([("6f1c0000-0000-4000-8000-000000000001".into(), 7)])
        );
        assert!(decode_vector("../x:1").is_err());
        assert!(decode_vector("node:x").is_err());
        assert!(decode_vector("").unwrap().is_empty());
    }
}
