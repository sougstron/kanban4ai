//! One outbound server link: push durable outbox, then consume hub sequence pages.
use super::discovery::{Config, ServerCard};
use super::hub::{Hello, HelloReply, Push, PushReply};
use super::*;
use crate::core::sync::{
    Journal, MIN_SYNC_PROTOCOL, Operation, SYNC_PROTOCOL, VersionVector, change_token,
};
use axum::extract::{DefaultBodyLimit, Path as Params};
use std::collections::BTreeMap;
use std::sync::LazyLock;
use tokio::io::AsyncWriteExt;

const PAGE: usize = 200;
const KEEPALIVE: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(500);
static CHANGED: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);
static KICK: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);
static STOPPING: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::channel(false).0);
pub(super) fn kick() {
    KICK.send_modify(|n| *n = n.wrapping_add(1));
}
pub(super) fn changed() {
    CHANGED.send_modify(|n| *n = n.wrapping_add(1));
}
pub(super) fn subscribe() -> watch::Receiver<u64> {
    CHANGED.subscribe()
}
pub(super) fn stopping() -> watch::Receiver<bool> {
    STOPPING.subscribe()
}
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| invalid(e.to_string()))?
}
pub(super) fn failure(e: KanbanError) -> Response {
    let text = e.to_string();
    let code = [
        "SYNC_STORE_MISMATCH",
        "SYNC_CLOCK_SKEW",
        "SYNC_STORE_TOO_NEW",
        "SYNC_OP_FORMAT_TOO_NEW",
        "SYNC_PROTOCOL_MISMATCH",
        "SYNC_SNAPSHOT_REQUIRED",
        "SYNC_NOT_A_SERVER",
        "SYNC_SERVER_BEHIND",
        "SYNC_MIGRATION_GAP",
        "NODE_AUTH_FAILED",
    ]
    .into_iter()
    .find(|c| text.contains(c));
    if let Some(code) = code {
        let status = match code {
            "SYNC_PROTOCOL_MISMATCH" => StatusCode::UPGRADE_REQUIRED,
            "NODE_AUTH_FAILED" => StatusCode::UNAUTHORIZED,
            _ => StatusCode::CONFLICT,
        };
        return error(status, code, &text);
    }
    let status = if matches!(e, KanbanError::Invalid(_) | KanbanError::Yaml(_)) {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    error(status, "INVALID_REQUEST", &text)
}
pub(super) fn routes(root: PathBuf) -> Router {
    let legacy = || async {
        error(
            StatusCode::UPGRADE_REQUIRED,
            "SYNC_PROTOCOL_MISMATCH",
            "обновите kanban4ai на всех узлах",
        )
    };
    Router::new()
        .route("/sync/vector", get(legacy))
        .route("/sync/ops", get(legacy).post(legacy))
        .route("/sync/ack", axum::routing::post(legacy))
        .route("/sync/stream", get(legacy))
        .route("/sync/snapshot", get(legacy))
        .route("/sync/hello", axum::routing::post(legacy))
        .route("/sync/bye", axum::routing::post(legacy))
        .route(
            "/sync/blobs/{hash}",
            get(get_blob)
                .put(put_blob)
                .layer(DefaultBodyLimit::max(256 << 20)),
        )
        .with_state(root)
}
async fn get_blob(State(root): State<PathBuf>, Params(hash): Params<String>) -> Response {
    match blocking(move || Journal::open(&root)?.blob(&hash)).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Err(e) => failure(e),
    }
}
async fn put_blob(
    State(root): State<PathBuf>,
    Params(hash): Params<String>,
    bytes: axum::body::Bytes,
) -> Response {
    match blocking(move || Journal::open(&root)?.put_blob(&hash, &bytes)).await {
        Ok(()) => Json(json!({})).into_response(),
        Err(e) => failure(e),
    }
}
#[derive(Default, Clone, Serialize, Deserialize)]
struct Cursor {
    #[serde(default)]
    server_node_id: String,
    #[serde(default)]
    store_id: String,
    #[serde(default)]
    hub_epoch: String,
    #[serde(default)]
    last_seq: u64,
    #[serde(default)]
    pushed: u64,
    #[serde(default)]
    migrating: bool,
}
impl Cursor {
    fn load(root: &Path) -> Result<Self> {
        match fs::read_to_string(root.join("sync/cursor.json")) {
            Ok(t) => serde_json::from_str(&t).map_err(|e| invalid(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    fn save(&self, root: &Path) -> Result<()> {
        let _lock = lock_file_reentrant(&root.join("sync/.lock"))?;
        atomic_write_text_raw(
            &root.join("sync/cursor.json"),
            &serde_json::to_string(self).map_err(|e| invalid(e.to_string()))?,
        )
    }
}
#[derive(Deserialize)]
struct HubOp {
    seq: u64,
    op: Operation,
}
#[derive(Deserialize)]
struct HubPage {
    ops: Vec<HubOp>,
    next: u64,
    head: u64,
    #[serde(default)]
    more: bool,
}
async fn apply(root: &Path, operations: Vec<Operation>) -> Result<()> {
    let root = root.to_path_buf();
    let applied = blocking(move || {
        let journal = Journal::open(&root)?;
        let known = journal.vector()?;
        let fresh: Vec<_> = operations
            .into_iter()
            .filter(|op| {
                op.op
                    .rsplit_once(':')
                    .and_then(|(_, s)| s.parse::<u64>().ok())
                    .is_none_or(|seq| seq > known.get(&op.hlc.2).copied().unwrap_or(0))
            })
            .collect();
        if fresh.is_empty() {
            return Ok(false);
        }
        journal.apply_remote(
            &fresh,
            discovery::config(&root)?.max_skew.saturating_mul(1000),
        )?;
        Ok(true)
    })
    .await?;
    if applied {
        changed();
    }
    Ok(())
}
struct ServerLink {
    root: PathBuf,
    peer: String,
    name: String,
    base: String,
    token: String,
    http: reqwest::Client,
    streaming: reqwest::Client,
}
impl ServerLink {
    async fn connect(root: &Path, config: &Config) -> Result<(Self, HelloReply)> {
        let peer = config
            .target
            .as_ref()
            .ok_or_else(|| invalid("No sync.server configured"))?;
        let secrets = discovery::server_credential(root)?;
        let secret = secrets
            .iter()
            .find(|s| s.node_id == peer.node.node_id)
            .ok_or_else(|| invalid("NODE_AUTH_FAILED: server is not paired"))?;
        let states: BTreeMap<String, discovery::Presence> =
            discovery::load(&root.join("sync/presence.yaml"))?;
        let state = states.get(&secret.node_id).cloned().unwrap_or_default();
        let mut last = invalid("NODE_UNREACHABLE: no server address answered");
        for address in discovery::server_addresses(root, peer, Some(secret), &state, config)? {
            let connect = Duration::from_secs(config.probe_timeout);
            let http = discovery::server_client(root, &address, connect, Some(REQUEST_TIMEOUT))?;
            let link = Self {
                root: root.to_path_buf(),
                peer: secret.node_id.clone(),
                name: peer.node.name.clone(),
                base: address.clone(),
                token: secret.token.clone(),
                http,
                streaming: discovery::server_client(root, &address, connect, None)?,
            };
            match link.hello().await {
                Ok(reply)
                    if reply.server.node_id == link.peer
                        && reply.server.role == crate::core::sync::NodeRole::Server =>
                {
                    if reply.store_id != Journal::open(root)?.node.store_id
                        && !ProjectStore::at(root).list()?.is_empty()
                    {
                        last = invalid("SYNC_STORE_MISMATCH: use join --fresh");
                        continue;
                    }
                    discovery::update_presence(root, &link.peer, |p| {
                        p.last_seen = Some(chrono::Utc::now());
                        p.bye = None;
                    })?;
                    link.status("ONLINE", "", reply.head)?;
                    return Ok((link, reply));
                }
                Ok(_) => last = invalid("SYNC_NOT_A_SERVER: another node answered"),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
    fn status(&self, code: &str, message: &str, head: u64) -> Result<()> {
        let route = if self.base.starts_with("https:") {
            "HTTPS"
        } else if crate::core::net::is_tailnet_address(&self.base) {
            "tailnet"
        } else {
            "LAN"
        };
        atomic_write_text_raw(&self.root.join("sync/server-status.json"),&json!({"code":code,"message":message,"head":head,"route":route,"at":chrono::Local::now().format("%d.%m %H:%M").to_string()}).to_string())
    }
    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .bearer_auth(&self.token)
            .header("x-kanban-sync-protocol", "3")
    }
    async fn response(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let reply = self
            .auth(request)
            .send()
            .await
            .map_err(|e| invalid(format!("NODE_UNREACHABLE: {}: {e}", self.name)))?;
        if !reply.status().is_success() {
            let status = reply.status();
            let body: serde_json::Value = reply.json().await.unwrap_or_default();
            return Err(invalid(format!(
                "{} ({status}): {}",
                body["error"]["code"].as_str().unwrap_or("HTTP_ERROR"),
                body["error"]["message"].as_str().unwrap_or("")
            )));
        }
        Ok(reply)
    }
    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T> {
        self.response(request)
            .await?
            .json()
            .await
            .map_err(|e| invalid(e.to_string()))
    }
    async fn hello(&self) -> Result<HelloReply> {
        let journal = Journal::open(&self.root)?;
        let cursor = Cursor::load(&self.root)?;
        self.send(
            self.http
                .post(format!("{}/sync/v3/hello", self.base))
                .json(&Hello {
                    node_card: journal.node,
                    cursor: Some(serde_json::to_value(cursor).map_err(|e| invalid(e.to_string()))?),
                    protocol: SYNC_PROTOCOL,
                    min_protocol: MIN_SYNC_PROTOCOL,
                }),
        )
        .await
    }
    async fn post(&self, ops: Vec<Operation>) -> Result<PushReply> {
        for hash in ops
            .iter()
            .filter_map(Operation::blob_hash)
            .collect::<BTreeSet<_>>()
        {
            let bytes = Journal::open(&self.root)?.blob(hash)?;
            let _: serde_json::Value = self
                .send(
                    self.http
                        .put(format!("{}/sync/blobs/{hash}", self.base))
                        .body(bytes),
                )
                .await?;
        }
        self.send(
            self.http
                .post(format!("{}/sync/v3/ops", self.base))
                .json(&Push { ops }),
        )
        .await
    }
    async fn fetch_blob(&self, hash: &str) -> Result<()> {
        if crate::core::sync::transport::has_blob(&self.root, hash)? {
            return Ok(());
        }
        let mut response = self
            .response(self.http.get(format!("{}/sync/blobs/{hash}", self.base)))
            .await?;
        let spool = tempfile::NamedTempFile::new()?;
        let mut file = tokio::fs::File::create(spool.path()).await?;
        while let Some(chunk) = response.chunk().await.map_err(|e| invalid(e.to_string()))? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        let root = self.root.clone();
        let hash = hash.to_owned();
        blocking(move || crate::core::sync::transport::put_blob_file(&root, &hash, spool.path()))
            .await
    }
    async fn fetch_blobs(&self, ops: &[Operation]) -> Result<()> {
        for hash in ops
            .iter()
            .filter_map(Operation::blob_hash)
            .collect::<BTreeSet<_>>()
        {
            self.fetch_blob(hash).await?;
        }
        Ok(())
    }
    async fn push(&self, cursor: &mut Cursor) -> Result<()> {
        let journal = Journal::open(&self.root)?;
        let own = journal.node.node_id.clone();
        let mut after = journal.vector()?;
        after.insert(own.clone(), cursor.pushed);
        loop {
            let root = self.root.clone();
            let offset = after.clone();
            let page = blocking(move || Journal::open(&root)?.operations_after(&offset, PAGE))
                .await
                .map_err(|e| {
                    if e.to_string().contains("SYNC_SNAPSHOT_REQUIRED") {
                        invalid("SYNC_SERVER_BEHIND: retained outbox cannot repair server rollback")
                    } else {
                        e
                    }
                })?;
            if page.operations.is_empty() {
                return Ok(());
            }
            let max = page
                .operations
                .iter()
                .filter(|op| op.hlc.2 == own)
                .filter_map(|op| op.op.rsplit_once(':')?.1.parse::<u64>().ok())
                .max()
                .unwrap_or(cursor.pushed);
            let reply = self.post(page.operations).await?;
            let accepted = reply
                .heads
                .get(&own)
                .copied()
                .unwrap_or(cursor.pushed)
                .min(max);
            if accepted <= cursor.pushed {
                return Err(invalid(
                    "SYNC_SERVER_BEHIND: server has a gap before the retained outbox",
                ));
            }
            cursor.pushed = accepted;
            cursor.save(&self.root)?;
            after.insert(own.clone(), accepted);
            if !page.more {
                return Ok(());
            }
        }
    }
    async fn migrate(&self, cursor: &mut Cursor) -> Result<()> {
        let journal = Journal::open(&self.root)?;
        let mut after = journal.state()?.compacted.clone();
        after.insert(
            self.peer.clone(),
            journal.vector()?.get(&self.peer).copied().unwrap_or(0),
        );
        loop {
            let page = journal.operations_after(&after, PAGE)?;
            if page.operations.is_empty() {
                break;
            }
            let sent: VersionVector = page
                .operations
                .iter()
                .filter_map(|op| {
                    Some((
                        op.hlc.2.clone(),
                        op.op.rsplit_once(':')?.1.parse::<u64>().ok()?,
                    ))
                })
                .fold(BTreeMap::new(), |mut v, (n, s)| {
                    let e = v.entry(n).or_default();
                    *e = (*e).max(s);
                    v
                });
            let reply = self.post(page.operations).await?;
            for (origin, seq) in sent {
                if reply.heads.get(&origin).copied().unwrap_or(0) < seq {
                    eprintln!(
                        "SYNC_MIGRATION_GAP: origin {origin}, retained prefix begins after server head"
                    );
                }
            }
            cursor.pushed = reply
                .heads
                .get(&journal.node.node_id)
                .copied()
                .unwrap_or(cursor.pushed);
            cursor.save(&self.root)?;
            after = page.next;
            if !page.more {
                break;
            }
        }
        cursor.migrating = false;
        cursor.save(&self.root)
    }
    async fn consume(&self, page: HubPage, cursor: &mut Cursor) -> Result<bool> {
        if page.next < cursor.last_seq || page.next > page.head {
            return Err(invalid("Invalid server sequence page"));
        }
        let mut previous = cursor.last_seq;
        for entry in &page.ops {
            if entry.seq != previous + 1 {
                return Err(invalid("SYNC_SNAPSHOT_REQUIRED: noncontiguous hub page"));
            }
            previous = entry.seq;
        }
        if previous != page.next {
            return Err(invalid("Invalid server page cursor"));
        }
        let ops: Vec<_> = page.ops.into_iter().map(|e| e.op).collect();
        self.fetch_blobs(&ops).await?;
        apply(&self.root, ops).await?;
        cursor.last_seq = page.next;
        cursor.save(&self.root)?;
        self.status("ONLINE", "", page.head)?;
        Ok(page.more)
    }
    async fn pull(&self, cursor: &mut Cursor, wait: u64) -> Result<()> {
        loop {
            let result = self
                .send::<HubPage>(self.http.get(format!(
                    "{}/sync/v3/ops?after={}&limit=500&wait={wait}",
                    self.base, cursor.last_seq
                )))
                .await;
            let page = match result {
                Err(e) if e.to_string().contains("SYNC_SNAPSHOT_REQUIRED") => {
                    let hello = self.hello().await?;
                    let journal = Journal::open(&self.root)?;
                    let own = journal
                        .vector()?
                        .get(&journal.node.node_id)
                        .copied()
                        .unwrap_or(0);
                    if own > hello.your_head {
                        return Err(invalid(
                            "SYNC_SERVER_BEHIND: refusing snapshot with unsent local operations",
                        ));
                    }
                    self.bootstrap(&hello, cursor).await?;
                    continue;
                }
                other => other?,
            };
            if !self.consume(page, cursor).await? {
                return Ok(());
            }
        }
    }
    async fn bootstrap(&self, hello: &HelloReply, cursor: &mut Cursor) -> Result<()> {
        let mut response = self
            .response(
                self.streaming
                    .get(format!("{}/sync/v3/snapshot", self.base)),
            )
            .await?;
        let spool = tempfile::NamedTempFile::new()?;
        let mut file = tokio::fs::File::create(spool.path()).await?;
        while let Some(chunk) = response.chunk().await.map_err(|e| invalid(e.to_string()))? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        let root = self.root.clone();
        let staged =
            blocking(move || crate::core::sync::transport::stage_snapshot(&root, spool.path()))
                .await?;
        if staged.header.store_id != hello.store_id || staged.header.node_id != self.peer {
            return Err(invalid("SYNC_STORE_MISMATCH: snapshot identity differs"));
        }
        for hash in &staged.blobs {
            self.fetch_blob(hash).await?;
        }
        let root = self.root.clone();
        blocking(move || staged.install(&root)).await?;
        *cursor = Cursor::load(&self.root)?;
        changed();
        Ok(())
    }
    async fn cycle(&self, full: bool) -> Result<Cursor> {
        let root = self.root.clone();
        blocking(move || Journal::open(&root)?.reconcile()).await?;
        let mut hello = self.hello().await?;
        let journal = Journal::open(&self.root)?;
        let mut cursor = Cursor::load(&self.root)?;
        if !cursor.server_node_id.is_empty() && cursor.server_node_id != self.peer {
            cursor = Cursor::default();
        }
        if hello.store_id != journal.node.store_id {
            if !ProjectStore::at(&self.root).list()?.is_empty() {
                return Err(invalid("SYNC_STORE_MISMATCH: use join --fresh"));
            }
            self.bootstrap(&hello, &mut cursor).await?;
            hello = self.hello().await?;
        }
        // Our own operations are journaled before they are pushed, so the
        // server can never be ahead of us for our node id. If it is, another
        // store (a copy of this one) writes under the same id: the server
        // would drop our operations as duplicates and mix the two histories.
        let own = journal
            .vector()?
            .get(&journal.node.node_id)
            .copied()
            .unwrap_or(0);
        if hello.your_head > own {
            return Err(invalid(format!(
                "SYNC_NODE_ID_IN_USE: the server holds operations {}..{} of node {} that this store never wrote; another store (a copy of this one) uses the same node id. Stop that copy, or re-pair this store with join --fresh",
                own + 1,
                hello.your_head,
                journal.node.node_id
            )));
        }
        if cursor.hub_epoch != hello.hub_epoch || cursor.last_seq > hello.head {
            cursor.last_seq = 0;
        }
        cursor.server_node_id = self.peer.clone();
        cursor.store_id = hello.store_id.clone();
        cursor.hub_epoch = hello.hub_epoch.clone();
        cursor.pushed = hello.your_head;
        cursor.save(&self.root)?;
        if cursor.migrating {
            self.migrate(&mut cursor).await?;
        }
        self.push(&mut cursor).await?;
        if full {
            self.pull(&mut cursor, 0).await?;
        }

        // Stable = pushed for our own origin and received by every active
        // client (hello.stable); later concurrent edits still meet our ops here.
        let mut ack = Journal::open(&self.root)?.vector()?;
        ack.insert(journal.node.node_id, cursor.pushed);
        for (origin, seq) in ack.iter_mut() {
            *seq = (*seq).min(hello.stable.get(origin).copied().unwrap_or(0));
        }
        ack.retain(|_, seq| *seq > 0);
        Journal::open(&self.root)?.prune(&BTreeMap::from([(self.peer.clone(), ack)]))?;
        Ok(cursor)
    }
    async fn upload_logs(&self) -> Result<()> {
        let journal = Journal::open(&self.root)?;
        for project in ProjectStore::at(&self.root).list()? {
            let board = self.root.join("projects").join(&project.id);
            for task in crate::core::storage::Storage::new(&board).list_tasks(None)? {
                let Ok(snapshot) = crate::core::runner_log::local_tail(&board, &task.id) else {
                    continue;
                };
                if snapshot.node_id != journal.node.node_id {
                    continue;
                }
                let _: serde_json::Value = self
                    .send(
                        self.http
                            .put(format!(
                                "{}/sync/v3/logs/{}/{}",
                                self.base, snapshot.project, snapshot.task
                            ))
                            .json(&snapshot),
                    )
                    .await?;
            }
        }
        Ok(())
    }
    async fn live(&self, mut cursor: Cursor, interval: Duration) -> Result<()> {
        let mut response = self
            .response(self.streaming.get(format!(
                "{}/sync/v3/stream?after={}",
                self.base, cursor.last_seq
            )))
            .await?;
        let mut changes = CHANGED.subscribe();
        let mut stopping = STOPPING.subscribe();
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        let mut buffer = Vec::new();
        loop {
            tokio::select! {
                result=tokio::time::timeout(KEEPALIVE*3,response.chunk())=>{
                    let chunk=result.map_err(|_|invalid("SSE idle"))?.map_err(|e|invalid(e.to_string()))?.ok_or_else(||invalid("SSE disconnected"))?;
                    buffer.extend_from_slice(&chunk);
                    if buffer.len()>16<<20{return Err(invalid("Oversized SSE event"));}
                    while let Some(end)=buffer.windows(2).position(|w|w==b"\n\n"){
                        let event:Vec<_>=buffer.drain(..end+2).collect();
                        let text=std::str::from_utf8(&event).map_err(|e|invalid(e.to_string()))?;
                        for data in text.lines().filter_map(|line|line.strip_prefix("data: ")) {
                            let page:HubPage=serde_json::from_str(data).map_err(|e|invalid(e.to_string()))?;
                            self.consume(page,&mut cursor).await?;
                        }
                        // Pull acknowledges only after the event was durably applied.
                    }
                }
                _=changes.changed()=>{self.push(&mut cursor).await?;}
                _=tick.tick()=>{return Ok(());}
                _=stopping.changed()=>return Ok(()),
            }
        }
    }
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
pub(super) fn start(root: &Path) -> Running {
    STOPPING.send_replace(false);
    let mut tasks = JoinSet::new();
    let watched = root.to_path_buf();
    tasks.spawn(async move {
        let mut last = change_token(&watched);
        loop {
            tokio::time::sleep(POLL).await;
            let now = change_token(&watched);
            if now != last {
                last = now;
                if Journal::open(&watched)
                    .is_ok_and(|j| j.node.role == crate::core::sync::NodeRole::Server)
                {
                    let r = watched.clone();
                    let _ = blocking(move || crate::core::sync::hub::Hub::open(&r)?.ingest()).await;
                }
                changed();
            }
        }
    });
    let monitor = root.to_path_buf();
    tasks.spawn(async move {
        loop {
            let r = monitor.clone();
            let interval = discovery::config(&monitor).map_or(60, |c| c.run_heartbeat);
            if let Err(e) = blocking(move || {
                let journal = Journal::open(&r)?;
                journal.reconcile()?;
                if journal.node.role == crate::core::sync::NodeRole::Server {
                    crate::core::sync::hub::Hub::open(&r)?.maintenance()?;
                }
                crate::core::code::reconcile(&r)?;
                crate::core::wip::publish(&r, interval, false)?;
                Ok(())
            })
            .await
            {
                eprintln!("sync reconcile: {e}");
            }
            super::release_memory();
            let interval = discovery::config(&monitor).map_or(30, |c| c.interval);
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
    if Journal::open(root).is_ok_and(|j| j.node.role != crate::core::sync::NodeRole::Server) {
        let logs = root.to_path_buf();
        tasks.spawn(async move {
            let mut reported = None;
            loop {
                let config = match discovery::config(&logs) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                if let Ok((link, _)) = ServerLink::connect(&logs, &config).await {
                    match link.upload_logs().await {
                        Ok(()) => reported = None,
                        Err(e) => {
                            let text = e.to_string();
                            if reported.as_ref() != Some(&text) {
                                eprintln!("runner log upload: {text}");
                                reported = Some(text);
                            }
                        }
                    }
                }
                let interval = match crate::core::runner_log::settings(&logs) {
                    Ok(s) => s.log_interval,
                    Err(e) => {
                        eprintln!("runner log settings: {e}");
                        return;
                    }
                };
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        });
        let root = root.to_path_buf();
        tasks.spawn(async move{
            let mut failures=0usize;let mut reported=None;let mut kicks=KICK.subscribe();let mut writes=CHANGED.subscribe();
            loop {
                let config=match discovery::config(&root){Ok(c)=>c,Err(e)=>{eprintln!("sync config: {e}");return}};
                let result=async{
                    let (link,_)=ServerLink::connect(&root,&config).await?;
                    let mut cursor=link.cycle(true).await?;
                    if failures>=2 {
                        // Long-poll fallback after two broken SSE sessions; retry SSE now and then.
                        failures=if failures>=12 {0} else {failures+1};
                        let mut local=CHANGED.subscribe();
                        tokio::select!{result=link.pull(&mut cursor,25)=>result,_=local.changed()=>Ok(())}
                    }else{
                        let result=link.live(cursor,Duration::from_secs(config.interval)).await;
                        if result.is_err(){failures=failures.saturating_add(1);}else{failures=0;}
                        result
                    }
                }.await;
                if *STOPPING.borrow(){return;}
                match result {
                    // A finished live session (safety tick, local write in
                    // long-poll mode) reconnects at once: no gap without updates.
                    Ok(())=>{reported=None;continue;}
                    Err(e)=>{let text=e.to_string();if reported.as_ref()!=Some(&text){eprintln!("sync server: {text}");reported=Some(text.clone());}
                        let _=atomic_write_text_raw(&root.join("sync/server-status.json"),&json!({"code":"NODE_UNREACHABLE","message":text}).to_string());}
                }
                tokio::select!{_=kicks.changed()=>(),_=writes.changed()=>(),_=tokio::time::sleep(Duration::from_secs(config.interval))=>()}
            }
        });
    }
    Running { tasks }
}
pub(super) async fn once(root: &Path, full: bool) -> Result<Vec<(String, Result<()>)>> {
    if !crate::core::sync::enabled(root) {
        return Ok(Vec::new());
    }
    if Journal::open(root)?.node.role == crate::core::sync::NodeRole::Server {
        let root = root.to_path_buf();
        blocking(move || crate::core::sync::hub::Hub::open(&root)?.maintenance()).await?;
        return Ok(Vec::new());
    }
    let config = discovery::config(root)?;
    let Some(server) = config.server.as_ref() else {
        return Ok(Vec::new());
    };
    let result = async {
        let (link, _) = ServerLink::connect(root, &config).await?;
        link.cycle(full).await?;
        link.upload_logs().await?;
        Ok(())
    }
    .await;
    Ok(vec![(server.name.clone(), result)])
}
pub(super) async fn catch_up(root: &Path, peer: &ServerCard) -> Result<()> {
    let config = discovery::config(root)?;
    if config
        .server
        .as_ref()
        .is_none_or(|s| s.node_id != peer.node.node_id)
    {
        return Err(invalid("SYNC_NOT_A_SERVER"));
    }
    let (link, _) = ServerLink::connect(root, &config).await?;
    link.cycle(true).await?;
    Ok(())
}
pub(super) async fn server_nodes(root: &Path) -> Result<Vec<discovery::Status>> {
    let config = discovery::config(root)?;
    let (link, _) = ServerLink::connect(root, &config).await?;
    let nodes: Vec<discovery::Status> = link
        .send(link.http.get(format!("{}/nodes", link.base)))
        .await?;
    atomic_write_text_raw(
        &root.join("sync/nodes.json"),
        &serde_json::to_string(&nodes).map_err(|e| invalid(e.to_string()))?,
    )?;
    Ok(nodes)
}
pub(super) async fn notify_server(root: &Path, reason: Option<&str>) -> Result<()> {
    let config = discovery::config(root)?;
    if config.server.is_none() {
        return Ok(());
    }
    let (link, _) = ServerLink::connect(root, &config).await?;
    if let Some(reason) = reason {
        link.cycle(false).await?;
        link.upload_logs().await?;
        let _: serde_json::Value = link
            .send(
                link.http
                    .post(format!("{}/sync/v3/bye", link.base))
                    .json(&discovery::Bye {
                        reason: reason.into(),
                        at: chrono::Utc::now(),
                    }),
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::operations::{Operations, TaskPatch};
    use crate::core::storage::{NewTask, Storage};
    use std::future::IntoFuture;

    static NETWORK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    fn node(name: &str, role: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            format!("node:\n  name: {name}\n  role: {role}\nsync:\n  enabled: true\n"),
        )
        .unwrap();
        Journal::open(dir.path()).unwrap();
        dir
    }
    async fn connect(client: &Path, server: &Path, base: &str) -> ServerLink {
        let node = Journal::open(client).unwrap().node;
        let server_node = Journal::open(server).unwrap().node;
        let token = issue_token(server, TokenKind::Node).unwrap();
        hub::register(server, &node, &token_hash(&token)).unwrap();
        let credential = discovery::Credential {
            node_id: server_node.node_id.clone(),
            token,
            urls: vec![base.to_owned()],
        };
        atomic_write_text_raw(
            &client.join("sync/server.yaml"),
            &serde_yaml_ng::to_string(&credential).unwrap(),
        )
        .unwrap();
        let store = ProjectStore::at(client);
        let mut config = store.load_global_config().unwrap();
        config.extras.get_mut("sync").unwrap()["server"] =
            serde_yaml_ng::to_value(discovery::ServerConfig {
                node_id: server_node.node_id,
                name: server_node.name,
                urls: vec![base.to_owned()],
            })
            .unwrap();
        store.save_global_config(&config).unwrap();
        ServerLink::connect(client, &discovery::config(client).unwrap())
            .await
            .unwrap()
            .0
    }
    async fn http(root: &Path) -> (tokio::task::JoinHandle<std::io::Result<()>>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(
            axum::serve(listener, router(root.join("sync/tokens.yaml"))).into_future(),
        );
        (server, base)
    }
    fn project(root: &Path, work: &Path) -> crate::core::project::Project {
        fs::create_dir_all(work).unwrap();
        let p = ProjectStore::at(root).add(work, None).unwrap().project;
        Storage::new(&p.data_root).init_board().unwrap();
        p
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_clients_offline_edits_push_before_pull_and_converge() {
        let _serial = NETWORK.lock().await;
        let server = node("server", "server");
        let a = node("laptop", "laptop");
        let b = node("phone", "mobile");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        let initial = Storage::new(&p.data_root)
            .create_task(NewTask::titled("shared"))
            .unwrap();
        let (http, base) = http(server.path()).await;
        let la = connect(a.path(), server.path(), &base).await;
        let lb = connect(b.path(), server.path(), &base).await;
        la.cycle(true).await.unwrap();
        lb.cycle(true).await.unwrap();
        let pa = ProjectStore::at(a.path()).get(&p.id).unwrap().unwrap();
        let pb = ProjectStore::at(b.path()).get(&p.id).unwrap().unwrap();
        Operations::for_project(&pa)
            .update_task(
                &initial.id,
                TaskPatch {
                    title: Some("from laptop".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        Operations::for_project(&pb)
            .update_task(
                &initial.id,
                TaskPatch {
                    title: Some("from phone".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        Storage::new(&pa.data_root)
            .create_task(NewTask::titled("offline a"))
            .unwrap();
        Storage::new(&pb.data_root)
            .create_task(NewTask::titled("offline b"))
            .unwrap();
        la.cycle(true).await.unwrap();
        lb.cycle(true).await.unwrap();
        la.cycle(true).await.unwrap();
        let expected = Storage::new(&p.data_root)
            .load_task(&initial.id)
            .unwrap()
            .unwrap()
            .title;
        assert_eq!(
            Storage::new(&pa.data_root)
                .load_task(&initial.id)
                .unwrap()
                .unwrap()
                .title,
            expected
        );
        assert_eq!(
            Storage::new(&pb.data_root)
                .load_task(&initial.id)
                .unwrap()
                .unwrap()
                .title,
            expected
        );
        for root in [a.path(), b.path(), server.path()] {
            assert_eq!(
                Storage::new(root.join("projects").join(&p.id))
                    .list_tasks(None)
                    .unwrap()
                    .len(),
                3
            );
        }
        let cursor = Cursor::load(a.path()).unwrap();
        assert!(cursor.last_seq > 0 && cursor.pushed > 0);
        assert_eq!(
            Journal::open(a.path()).unwrap().vector().unwrap(),
            Journal::open(b.path()).unwrap().vector().unwrap()
        );
        // The same LWW winner and the same title conflict record on all three replicas.
        let conflicts: Vec<_> = [a.path(), b.path(), server.path()]
            .iter()
            .map(|root| {
                let mut found: Vec<_> = Journal::open(root)
                    .unwrap()
                    .conflicts()
                    .unwrap()
                    .into_iter()
                    .filter(|c| c.entity == "task" && c.field == "title")
                    .map(|c| (c.winner, c.loser, c.winner_value))
                    .collect();
                found.sort_by(|x, y| (&x.0, &x.1).cmp(&(&y.0, &y.1)));
                found
            })
            .collect();
        assert_eq!(conflicts[0].len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0], conflicts[1]);
        assert_eq!(conflicts[0], conflicts[2]);
        assert_eq!(conflicts[0][0].2, json!(expected));
        http.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_copied_store_refuses_to_sync_under_the_same_node_id() {
        let _serial = NETWORK.lock().await;
        let server = node("server", "server");
        let a = node("laptop", "laptop");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        Storage::new(&p.data_root)
            .create_task(NewTask::titled("shared"))
            .unwrap();
        let (http, base) = http(server.path()).await;
        let la = connect(a.path(), server.path(), &base).await;
        la.cycle(true).await.unwrap();
        // A copy of the laptop store keeps its node id; the original then
        // pushes more of its own operations than the copy will ever have.
        let copy = tempfile::tempdir().unwrap();
        copy_tree(a.path(), copy.path());
        let pa = ProjectStore::at(a.path()).get(&p.id).unwrap().unwrap();
        for title in ["one", "two", "three"] {
            Storage::new(&pa.data_root)
                .create_task(NewTask::titled(title))
                .unwrap();
        }
        la.cycle(true).await.unwrap();
        let pc = ProjectStore::at(copy.path()).get(&p.id).unwrap().unwrap();
        Storage::new(&pc.data_root)
            .create_task(NewTask::titled("only in the copy"))
            .unwrap();
        let head = la.hello().await.unwrap().head;
        let lc = ServerLink::connect(copy.path(), &discovery::config(copy.path()).unwrap())
            .await
            .unwrap()
            .0;
        let Err(err) = lc.cycle(true).await else {
            panic!("a copy with the same node id must not sync");
        };
        assert!(err.to_string().contains("SYNC_NODE_ID_IN_USE"), "{err}");
        assert_eq!(la.hello().await.unwrap().head, head, "copy pushed nothing");
        http.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_unavailable_keeps_everything_local_then_delivers() {
        let _serial = NETWORK.lock().await;
        let server = node("server", "server");
        let client = node("laptop", "laptop");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        let shared = Storage::new(&p.data_root)
            .create_task(NewTask::titled("shared"))
            .unwrap();
        let (running, base) = http(server.path()).await;
        let mut link = connect(client.path(), server.path(), &base).await;
        link.cycle(true).await.unwrap();
        running.abort();
        let _ = running.await;
        let local = ProjectStore::at(client.path()).get(&p.id).unwrap().unwrap();
        let started = std::time::Instant::now();
        let ops = Operations::for_project(&local);
        let created = ops
            .create_task(NewTask::titled("written while the server is down"))
            .unwrap();
        ops.update_task(
            &shared.id,
            TaskPatch {
                title: Some("edited offline".into()),
                ..Default::default()
            },
        )
        .unwrap();
        ops.move_task(&created.id, "in_progress", false).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "local writes waited for the network"
        );
        let results = once(client.path(), true).await.unwrap();
        assert!(
            results[0]
                .1
                .as_ref()
                .is_err_and(|e| e.to_string().contains("NODE_UNREACHABLE")),
            "{:?}",
            results[0].1.as_ref().err()
        );
        assert!(
            own_outbox(client.path()).len() >= 3,
            "outbox holds the offline writes"
        );
        let (restored, new_base) = http(server.path()).await;
        link.base = new_base;
        link.cycle(true).await.unwrap();
        assert!(own_outbox(client.path()).is_empty());
        let board = Storage::new(&p.data_root);
        assert_eq!(
            board.load_task(&shared.id).unwrap().unwrap().title,
            "edited offline"
        );
        assert!(board.task_exists(&created.id));
        restored.abort();
    }
    #[test]
    fn a_client_that_never_pulled_holds_history_only_through_the_bootstrap_grace() {
        let server = node("server", "server");
        let phone = node("phone", "mobile");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        Storage::new(&p.data_root)
            .create_task(NewTask::titled("initial"))
            .unwrap();
        Journal::open(server.path()).unwrap().reconcile().unwrap();
        hub::initialize(server.path()).unwrap();
        let hub = crate::core::sync::hub::Hub::open(server.path()).unwrap();
        let head = hub.vector_at(hub.ingest().unwrap()).unwrap();
        assert!(!head.is_empty());
        let phone_id = Journal::open(phone.path()).unwrap().node.node_id;
        let token = issue_token(server.path(), TokenKind::Node).unwrap();
        hub::register(
            server.path(),
            &Journal::open(phone.path()).unwrap().node,
            &token_hash(&token),
        )
        .unwrap();
        let clients = server.path().join("sync/hub/clients.json");
        let quiet_since = |seconds: i64, cursor: u64| {
            let mut all: BTreeMap<String, serde_json::Value> =
                serde_json::from_str(&fs::read_to_string(&clients).unwrap()).unwrap();
            all.get_mut(&phone_id).unwrap()["last_seen"] = serde_json::Value::from(
                (chrono::Utc::now() - chrono::Duration::seconds(seconds)).to_rfc3339(),
            );
            all.get_mut(&phone_id).unwrap()["cursor"] = serde_json::Value::from(cursor);
            fs::write(&clients, serde_json::to_string(&all).unwrap()).unwrap();
        };
        // Just paired: its bootstrap may still pull from the start.
        assert_ne!(hub.stable_vector().unwrap(), head);
        // Quiet past the grace without ever pulling: it will need a snapshot
        // anyway, so it no longer holds the history.
        quiet_since(2 * 3600, 0);
        assert_eq!(hub.stable_vector().unwrap(), head);
        // A client that did pull keeps its position for the whole TTL.
        quiet_since(2 * 3600, 1);
        assert_eq!(hub.stable_vector().unwrap(), hub.vector_at(1).unwrap());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn protocol_auth_duplicate_push_and_server_only_routes() {
        let _serial = NETWORK.lock().await;
        let server = node("server", "server");
        let client = node("laptop", "laptop");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        Storage::new(&p.data_root)
            .create_task(NewTask::titled("initial"))
            .unwrap();
        let (http, base) = http(server.path()).await;
        let link = connect(client.path(), server.path(), &base).await;
        link.cycle(true).await.unwrap();
        let no_auth = reqwest::Client::new()
            .get(format!("{base}/sync/v3/ops"))
            .header("x-kanban-sync-protocol", "3")
            .send()
            .await
            .unwrap();
        assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);
        let old = link
            .http
            .get(format!("{base}/sync/v3/ops"))
            .bearer_auth(&link.token)
            .header("x-kanban-sync-protocol", "2")
            .send()
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::UPGRADE_REQUIRED);
        let old = link
            .http
            .get(format!("{base}/sync/vector"))
            .send()
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::UPGRADE_REQUIRED);
        let journal = Journal::open(server.path()).unwrap();
        let ops = journal
            .operations_after(&VersionVector::new(), 200)
            .unwrap()
            .operations;
        let head = link.post(ops.clone()).await.unwrap().head;
        let again = link.post(ops.clone()).await.unwrap().head;
        assert_eq!(head, again);
        let mut future = ops.clone();
        future[0].v = 2;
        let error = link.post(future).await.err().unwrap().to_string();
        assert!(error.contains("SYNC_OP_FORMAT_TOO_NEW"), "{error}");
        let raw = link
            .http
            .get(format!("{base}/sync/v3/ops?after=0&limit=1"))
            .bearer_auth(&link.token)
            .header("x-kanban-sync-protocol", "3")
            .send()
            .await
            .unwrap();
        let page: serde_json::Value = raw.json().await.unwrap();
        assert_eq!(page["ops"].as_array().unwrap().len(), 1);
        assert_eq!(page["next"], 1);
        let token = issue_token(client.path(), TokenKind::Node).unwrap();
        let (other, client_base) = http_client(client.path()).await;
        let response = link
            .http
            .get(format!("{client_base}/sync/v3/ops"))
            .header("x-kanban-sync-protocol", "3")
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["error"]["code"],
            "SYNC_NOT_A_SERVER"
        );
        other.abort();
        http.abort();
    }
    async fn http_client(root: &Path) -> (tokio::task::JoinHandle<std::io::Result<()>>, String) {
        http(root).await
    }
    #[test]
    fn cursor_roundtrip_keeps_migration_and_outbox_receipts() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sync")).unwrap();
        let cursor = Cursor {
            server_node_id: "server".into(),
            store_id: "store".into(),
            hub_epoch: "epoch".into(),
            last_seq: 9,
            pushed: 4,
            migrating: true,
        };
        cursor.save(dir.path()).unwrap();
        let loaded = Cursor::load(dir.path()).unwrap();
        assert_eq!(
            (loaded.last_seq, loaded.pushed, loaded.migrating),
            (9, 4, true)
        );
    }
    #[test]
    fn server_url_accepts_tls_and_rejects_credentials_and_redirect_paths() {
        assert_eq!(
            discovery::base_url("https://sync.example.test")
                .unwrap()
                .scheme(),
            "https"
        );
        assert!(discovery::base_url("http://user:password@example.test").is_err());
        assert!(discovery::base_url("https://example.test/store").is_err());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn longpoll_sse_and_streaming_snapshot_are_server_mediated() {
        let _serial = NETWORK.lock().await;
        STOPPING.send_replace(false);
        let server = node("server", "server");
        let client = node("laptop", "laptop");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        let storage = Storage::new(&p.data_root);
        storage.create_task(NewTask::titled("initial")).unwrap();
        fs::create_dir_all(p.data_root.join(".kanban/assets/images")).unwrap();
        fs::write(
            p.data_root.join(".kanban/assets/images/test.png"),
            [0, 255, 1, 128],
        )
        .unwrap();
        fs::create_dir_all(p.data_root.join(".kanban/context")).unwrap();
        fs::write(p.data_root.join(".kanban/context/local.txt"), "private").unwrap();
        let (http, base) = http(server.path()).await;
        let link = connect(client.path(), server.path(), &base).await;
        let cursor = link.cycle(true).await.unwrap();
        let image = client
            .path()
            .join("projects")
            .join(&p.id)
            .join(".kanban/assets/images/test.png");
        assert_eq!(fs::read(image).unwrap(), [0, 255, 1, 128]);
        let response = link
            .response(link.http.get(format!("{base}/sync/v3/snapshot")))
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/x-ndjson"
        );
        let data = response.bytes().await.unwrap();
        let text = std::str::from_utf8(&data).unwrap();
        let records: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.first().unwrap()["kind"], "header");
        assert_eq!(records.last().unwrap()["kind"], "end");
        assert!(!text.contains("context/local.txt"));
        assert!(
            records.iter().any(|r| r["kind"] == "blob"
                && r["path"].as_str().is_some_and(|p| p.ends_with("test.png")))
        );
        let incomplete = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            incomplete.path(),
            text.lines()
                .take(records.len() - 1)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        assert!(
            crate::core::sync::transport::stage_snapshot(client.path(), incomplete.path()).is_err()
        );
        let request = link.auth(link.http.get(format!(
            "{base}/sync/v3/ops?after={}&wait=5",
            cursor.last_seq
        )));
        let pending = tokio::spawn(async move { request.send().await.unwrap() });
        tokio::time::sleep(Duration::from_millis(100)).await;
        storage
            .create_task(NewTask::titled("longpoll write"))
            .unwrap();
        changed();
        let page: serde_json::Value = tokio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(!page["ops"].as_array().unwrap().is_empty());
        let after = page["next"].as_u64().unwrap();
        let mut stream = link
            .response(
                link.streaming
                    .get(format!("{base}/sync/v3/stream?after={after}")),
            )
            .await
            .unwrap();
        assert_eq!(stream.headers()["x-accel-buffering"], "no");
        storage.create_task(NewTask::titled("SSE write")).unwrap();
        changed();
        let chunk = tokio::time::timeout(Duration::from_secs(3), stream.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(std::str::from_utf8(&chunk).unwrap().contains("event: ops"));
        // Delivery alone is not a durable acknowledgement.
        let clients: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(server.path().join("sync/hub/clients.json")).unwrap(),
        )
        .unwrap();
        let id = Journal::open(client.path()).unwrap().node.node_id;
        assert_eq!(clients[&id]["cursor"].as_u64().unwrap(), after);
        drop(stream);
        http.abort();
    }

    fn own_outbox(root: &Path) -> Vec<Operation> {
        let journal = Journal::open(root).unwrap();
        let pushed = Cursor::load(root).unwrap().pushed;
        journal
            .operations(Some(&journal.node.node_id))
            .unwrap()
            .into_iter()
            .filter(|op| {
                op.op
                    .rsplit_once(':')
                    .and_then(|(_, seq)| seq.parse::<u64>().ok())
                    .is_some_and(|seq| seq > pushed)
            })
            .collect()
    }
    fn copy_tree(source: &Path, target: &Path) {
        fs::create_dir_all(target).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let path = entry.unwrap().path();
            let dest = target.join(path.file_name().unwrap());
            if path.is_dir() {
                copy_tree(&path, &dest);
            } else {
                fs::copy(path, dest).unwrap();
            }
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn restart_repairs_torn_hub_tail_and_apply_before_ingest() {
        use std::io::Write;
        let _serial = NETWORK.lock().await;
        let server = node("server", "server");
        let client = node("laptop", "laptop");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        Storage::new(&p.data_root)
            .create_task(NewTask::titled("initial"))
            .unwrap();
        let (running, base) = http(server.path()).await;
        let mut link = connect(client.path(), server.path(), &base).await;
        let cursor = link.cycle(true).await.unwrap();
        let epoch = crate::core::sync::hub::Hub::open(server.path())
            .unwrap()
            .epoch()
            .unwrap();
        let board = Storage::new(client.path().join("projects").join(&p.id));
        let task = board
            .create_task(NewTask::titled("apply committed before hub append"))
            .unwrap();
        let operations = own_outbox(client.path());
        assert!(!operations.is_empty());
        Journal::open(server.path())
            .unwrap()
            .apply_remote(&operations, 300_000)
            .unwrap();
        running.abort();
        let _ = running.await;
        let mut segments: Vec<_> = fs::read_dir(server.path().join("sync/hub/log"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        segments.sort();
        fs::OpenOptions::new()
            .append(true)
            .open(segments.last().unwrap())
            .unwrap()
            .write_all(b"{\"seq\":")
            .unwrap();
        hub::initialize(server.path()).unwrap();
        let state: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(server.path().join("sync/hub/clients.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            state[&Journal::open(client.path()).unwrap().node.node_id]["cursor"],
            cursor.last_seq
        );
        let (restarted, new_base) = http(server.path()).await;
        link.base = new_base;
        let resumed = link.cycle(true).await.unwrap();
        assert_eq!(resumed.hub_epoch, epoch);
        assert!(resumed.last_seq > cursor.last_seq);
        assert!(Storage::new(&p.data_root).task_exists(&task.id));
        assert_eq!(
            Journal::open(client.path()).unwrap().vector().unwrap(),
            Journal::open(server.path()).unwrap().vector().unwrap()
        );
        restarted.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_rollback_rewinds_receipt_and_repushes_retained_outbox() {
        let _serial = NETWORK.lock().await;
        let server = node("server", "server");
        let client = node("laptop", "laptop");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        Storage::new(&p.data_root)
            .create_task(NewTask::titled("initial"))
            .unwrap();
        let (running, base) = http(server.path()).await;
        let mut link = connect(client.path(), server.path(), &base).await;
        let mut cursor = link.cycle(true).await.unwrap();
        let backup = tempfile::tempdir().unwrap();
        copy_tree(server.path(), backup.path());
        let copy = Storage::new(client.path().join("projects").join(&p.id));
        let task = copy
            .create_task(NewTask::titled("must survive server rollback"))
            .unwrap();
        // Preserve the retained prefix to exercise successful resend, without pruning.
        link.push(&mut cursor).await.unwrap();
        link.pull(&mut cursor, 0).await.unwrap();
        let pushed = cursor.pushed;
        assert!(pushed > 0);
        running.abort();
        let _ = running.await;
        fs::remove_dir_all(server.path()).unwrap();
        copy_tree(backup.path(), server.path());
        fs::remove_file(server.path().join("sync/hub/epoch")).unwrap();
        hub::initialize(server.path()).unwrap();
        let (restored, new_base) = http(server.path()).await;
        link.base = new_base;
        let hello = link.hello().await.unwrap();
        assert!(hello.your_head < pushed);
        let resumed = link.cycle(true).await.unwrap();
        assert!(resumed.pushed >= pushed);
        assert_eq!(resumed.hub_epoch, hello.hub_epoch);
        assert!(Storage::new(&p.data_root).task_exists(&task.id));
        assert!(copy.task_exists(&task.id));
        restored.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lagging_client_pages_then_pushes_outbox_before_compacted_snapshot() {
        let _serial = NETWORK.lock().await;
        let server = node("server", "server");
        let lagging = node("phone", "mobile");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        let store = ProjectStore::at(server.path());
        let mut config = store.load_global_config().unwrap();
        config.extras.get_mut("sync").unwrap()["journal"] =
            serde_yaml_ng::from_str("segment_bytes: 256").unwrap();
        config.extras.get_mut("sync").unwrap()["hub"] =
            serde_yaml_ng::from_str("client_ttl_days: 0").unwrap();
        store.save_global_config(&config).unwrap();
        let source = Storage::new(&p.data_root);
        source.create_task(NewTask::titled("initial")).unwrap();
        let (running, base) = http(server.path()).await;
        let link = connect(lagging.path(), server.path(), &base).await;
        let initial = link.cycle(true).await.unwrap();
        let copy = Storage::new(lagging.path().join("projects").join(&p.id));
        let offline = copy
            .create_task(NewTask::titled("offline outbox retained across snapshot"))
            .unwrap();
        for n in 0..300 {
            source
                .create_task(NewTask::titled(format!("server task {n}")))
                .unwrap();
        }
        let mut after = initial.last_seq;
        let mut pages = 0;
        loop {
            let page: HubPage = link
                .send(
                    link.http
                        .get(format!("{base}/sync/v3/ops?after={after}&limit=5")),
                )
                .await
                .unwrap();
            assert!(page.ops.len() <= 5);
            after = page.next;
            pages += 1;
            if !page.more {
                break;
            }
        }
        assert!(pages > 50, "only {pages} pages");
        let hub = crate::core::sync::hub::Hub::open(server.path()).unwrap();
        hub.maintenance().unwrap();
        assert!(hub.floor().unwrap() > initial.last_seq);
        let error = link
            .send::<HubPage>(
                link.http
                    .get(format!("{base}/sync/v3/ops?after={}", initial.last_seq)),
            )
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("SYNC_SNAPSHOT_REQUIRED"), "{error}");
        let resumed = link.cycle(true).await.unwrap();
        assert!(resumed.last_seq > initial.last_seq && resumed.pushed > 0);
        assert!(
            source
                .list_tasks(None)
                .unwrap()
                .iter()
                .any(|task| task.title == offline.title)
        );
        assert!(
            copy.list_tasks(None)
                .unwrap()
                .iter()
                .any(|task| task.title == offline.title)
        );
        assert_eq!(
            source.list_tasks(None).unwrap().len(),
            copy.list_tasks(None).unwrap().len()
        );
        assert_eq!(
            Journal::open(server.path()).unwrap().vector().unwrap(),
            Journal::open(lagging.path()).unwrap().vector().unwrap()
        );
        running.abort();
    }
    /// Rewrites a star store into the 0.8 (format 3) peer layout.
    fn legacy_v3(root: &Path, peer: serde_json::Value, secret: serde_json::Value) {
        for path in ["sync/hub", "sync/server.yaml", "sync/cursor.json"] {
            let path = root.join(path);
            if path.is_dir() {
                fs::remove_dir_all(&path).unwrap();
            } else {
                let _ = fs::remove_file(&path);
            }
        }
        let mut config: serde_json::Value =
            serde_yaml_ng::from_str(&fs::read_to_string(root.join("config.yaml")).unwrap())
                .unwrap();
        let sync = config["sync"].as_object_mut().unwrap();
        sync.remove("server");
        sync.insert("peers".into(), json!([peer]));
        sync.insert("relay_remote".into(), json!("origin"));
        sync.insert("relay_interval".into(), json!(60));
        sync.insert("relay_encrypt".into(), json!(false));
        fs::write(
            root.join("config.yaml"),
            serde_yaml_ng::to_string(&config).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("sync/peers.yaml"),
            serde_yaml_ng::to_string(&json!([secret])).unwrap(),
        )
        .unwrap();
        fs::write(root.join("sync/exchanges.yaml"), "{}\n").unwrap();
        fs::write(root.join("sync/relay-ack.json"), "{}\n").unwrap();
        fs::write(root.join("sync/relay-age.key"), "AGE-SECRET-KEY-1TEST\n").unwrap();
        fs::create_dir_all(root.join("sync/relay/refs")).unwrap();
        fs::write(root.join("sync/FORMAT"), "3\n").unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn format3_peers_and_relay_migrate_to_the_star_without_repairing() {
        let _serial = NETWORK.lock().await;
        let server = node("server", "server");
        let laptop = node("laptop", "laptop");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        Storage::new(&p.data_root)
            .create_task(NewTask::titled("before migration"))
            .unwrap();
        let (running, base) = http(server.path()).await;
        connect(laptop.path(), server.path(), &base)
            .await
            .cycle(true)
            .await
            .unwrap();
        running.abort();
        let _ = running.await;
        // A seek cache from a mismatched binary: its offsets must not
        // survive into format 4.
        for root in [server.path(), laptop.path()] {
            fs::create_dir_all(root.join("sync/meta")).unwrap();
            fs::write(
                root.join("sync/meta/index.json"),
                json!({"nodes":{"stale-node":[{"file":"00000000000000000001.jsonl",
                    "marks":[{"seq":9,"offset":31}]}]}})
                .to_string(),
            )
            .unwrap();
        }
        let (s, l) = (
            Journal::open(server.path()).unwrap().node,
            Journal::open(laptop.path()).unwrap().node,
        );
        let credential: discovery::Credential =
            discovery::load(&laptop.path().join("sync/server.yaml")).unwrap();
        let address = base.trim_start_matches("http://").to_owned();
        legacy_v3(
            server.path(),
            json!({"node_id":l.node_id,"name":l.name,"role":"laptop","store_id":l.store_id,"address":"127.0.0.1:9"}),
            json!({"node_id":l.node_id,"token":"server-to-laptop","inbound_sha256":token_hash(&credential.token)}),
        );
        legacy_v3(
            laptop.path(),
            json!({"node_id":s.node_id,"name":s.name,"role":"server","store_id":s.store_id,"address":address}),
            json!({"node_id":s.node_id,"token":credential.token,"inbound_sha256":token_hash("server-to-laptop")}),
        );
        // Accumulated while still on 0.8: must reach the server through migration.
        let offline = Storage::new(laptop.path().join("projects").join(&p.id))
            .create_task(NewTask::titled("written on 0.8"))
            .unwrap();

        for root in [server.path(), laptop.path()] {
            Journal::open(root).unwrap();
            assert_eq!(
                fs::read_to_string(root.join("sync/FORMAT")).unwrap().trim(),
                crate::core::sync::STORE_FORMAT.to_string()
            );
            for gone in [
                "sync/peers.yaml",
                "sync/exchanges.yaml",
                "sync/relay-ack.json",
                "sync/relay-age.key",
                "sync/relay",
            ] {
                assert!(!root.join(gone).exists(), "{gone}");
            }
            // The stale seek cache must not survive: the server rebuilds a
            // fresh one while ingesting, the client is left without one.
            if let Ok(text) = fs::read_to_string(root.join("sync/meta/index.json")) {
                assert!(!text.contains("stale-node"), "{text}");
            }
            assert!(
                root.join("sync/legacy-v3/peers.yaml").is_file()
                    && root.join("sync/legacy-v3/credentials.yaml").is_file()
            );
            let config = fs::read_to_string(root.join("config.yaml")).unwrap();
            assert!(
                !config.contains("peers") && !config.contains("relay_"),
                "{config}"
            );
        }
        let clients: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(server.path().join("sync/hub/clients.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            clients[&l.node_id]["token_sha256"],
            json!(token_hash(&credential.token))
        );
        let config = discovery::config(laptop.path()).unwrap();
        let target = config.server.clone().unwrap();
        assert_eq!(
            (target.node_id.as_str(), target.urls.clone()),
            (s.node_id.as_str(), vec![base.clone()])
        );
        assert!(Cursor::load(laptop.path()).unwrap().migrating);
        let before = fs::read_to_string(laptop.path().join("config.yaml")).unwrap();
        Journal::open(laptop.path()).unwrap();
        assert_eq!(
            fs::read_to_string(laptop.path().join("config.yaml")).unwrap(),
            before,
            "a second open is a no-op"
        );

        // Crash after the config cutover but before cleanup and FORMAT: the rerun finishes from legacy-v3.
        fs::write(laptop.path().join("sync/FORMAT"), "3\n").unwrap();
        fs::copy(
            laptop.path().join("sync/legacy-v3/credentials.yaml"),
            laptop.path().join("sync/peers.yaml"),
        )
        .unwrap();
        fs::create_dir_all(laptop.path().join("sync/relay")).unwrap();
        Journal::open(laptop.path()).unwrap();
        assert_eq!(
            fs::read_to_string(laptop.path().join("sync/FORMAT"))
                .unwrap()
                .trim(),
            crate::core::sync::STORE_FORMAT.to_string()
        );
        assert!(
            !laptop.path().join("sync/peers.yaml").exists()
                && !laptop.path().join("sync/relay").exists()
        );
        assert_eq!(
            discovery::server_credential(laptop.path())
                .unwrap()
                .unwrap()
                .token,
            credential.token
        );

        // The old token still authenticates; the retained 0.8 outbox lands on the server.
        let listener = TcpListener::bind(&address).await.unwrap();
        let restored = tokio::spawn(
            axum::serve(listener, router(server.path().join("sync/tokens.yaml"))).into_future(),
        );
        let results = once(laptop.path(), true).await.unwrap();
        assert!(results[0].1.is_ok(), "{:?}", results[0].1.as_ref().err());
        assert!(!Cursor::load(laptop.path()).unwrap().migrating);
        assert!(Storage::new(&p.data_root).task_exists(&offline.id));
        restored.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn migrating_client_forwards_retained_foreign_ops_with_unknown_fields() {
        let _serial = NETWORK.lock().await;
        let server = node("server", "server");
        let laptop = node("laptop", "laptop");
        let phone = node("phone", "mobile");
        let work = tempfile::tempdir().unwrap();
        let p = project(server.path(), &work.path().join("demo"));
        Storage::new(&p.data_root)
            .create_task(NewTask::titled("initial"))
            .unwrap();
        let (running, base) = http(server.path()).await;
        let laptop_link = connect(laptop.path(), server.path(), &base).await;
        laptop_link.cycle(true).await.unwrap();
        let phone_link = connect(phone.path(), server.path(), &base).await;
        phone_link.cycle(true).await.unwrap();
        let phone_board = Storage::new(phone.path().join("projects").join(&p.id));
        let task = phone_board
            .create_task(NewTask::titled(
                "previously received from legacy phone peer",
            ))
            .unwrap();
        let mut foreign = own_outbox(phone.path());
        assert!(!foreign.is_empty());
        foreign[0]
            .extra
            .insert("future_field".into(), json!({"preserve":[1,2,3]}));
        // Simulate format-3 retained journal history; there is no client HTTP route.
        Journal::open(laptop.path())
            .unwrap()
            .apply_remote(&foreign, 300_000)
            .unwrap();
        let mut cursor = Cursor::load(laptop.path()).unwrap();
        cursor.migrating = true;
        cursor.save(laptop.path()).unwrap();
        laptop_link.cycle(true).await.unwrap();
        assert!(!Cursor::load(laptop.path()).unwrap().migrating);
        assert!(Storage::new(&p.data_root).task_exists(&task.id));
        let phone_id = Journal::open(phone.path()).unwrap().node.node_id;
        let retained = Journal::open(server.path())
            .unwrap()
            .operations(Some(&phone_id))
            .unwrap();
        assert!(
            retained
                .iter()
                .any(|op| op.extra.get("future_field") == Some(&json!({"preserve":[1,2,3]})))
        );
        let hub = crate::core::sync::hub::Hub::open(server.path()).unwrap();
        let mut after = 0;
        let mut found = false;
        loop {
            let page = hub.page(after, 5, 8 << 20).unwrap();
            found |= page
                .ops
                .iter()
                .any(|entry| entry.op.extra.contains_key("future_field"));
            after = page.next;
            if !page.more {
                break;
            }
        }
        assert!(found);
        let mut future = foreign;
        future[0].v = 2;
        let before = hub.ingest().unwrap();
        assert!(
            laptop_link
                .post(future)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("SYNC_OP_FORMAT_TOO_NEW")
        );
        assert_eq!(hub.ingest().unwrap(), before);
        running.abort();
    }
}
