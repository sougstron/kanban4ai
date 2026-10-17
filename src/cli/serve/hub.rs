//! Authenticated protocol 3 server endpoints. Only the hub serves replicas.
use super::*;
use crate::core::sync::hub::Hub;
use crate::core::sync::{Journal, NodeIdentity, NodeRole, Operation, SYNC_PROTOCOL};
use axum::Extension;
use axum::extract::{DefaultBodyLimit, Path as Params, Query};
use std::collections::BTreeMap;
use std::convert::Infallible;

pub(super) fn not_server() -> Response {
    error(
        StatusCode::CONFLICT,
        "SYNC_NOT_A_SERVER",
        "Connect to a node with role server",
    )
}
pub(super) fn require_server(root: &Path) -> Result<()> {
    if Journal::open(root)?.node.role != NodeRole::Server {
        return Err(invalid("SYNC_NOT_A_SERVER"));
    }
    Ok(())
}
pub(super) fn initialize(root: &Path) -> Result<()> {
    require_server(root)?;
    let spool = root.join("sync/hub/spool");
    if spool.exists() {
        fs::remove_dir_all(&spool)?;
    }
    Hub::open(root)?.ingest()?;
    Ok(())
}
// The core hub owns the registry format; unknown client properties survive updates.
fn clients(root: &Path) -> Result<BTreeMap<String, serde_json::Value>> {
    let path = root.join("sync/hub/clients.json");
    match fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| invalid(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}
fn save_clients(root: &Path, clients: &BTreeMap<String, serde_json::Value>) -> Result<()> {
    atomic_write_text_raw(
        &root.join("sync/hub/clients.json"),
        &serde_json::to_string(clients).map_err(|e| invalid(e.to_string()))?,
    )
}
pub(super) fn register(root: &Path, node: &NodeIdentity, hash: &str) -> Result<()> {
    let _lock = lock_file_reentrant(&root.join("sync/.lock"))?;
    let mut all = clients(root)?;
    if all.iter().any(|(id, client)| {
        id != &node.node_id && client["name"].as_str() == Some(node.name.as_str())
    }) {
        return Err(invalid("Client node names must be unique"));
    }
    let old = all
        .entry(node.node_id.clone())
        .or_insert_with(|| json!({"cursor":0}));
    if let Some(previous) = old["token_sha256"]
        .as_str()
        .filter(|previous| *previous != hash)
    {
        let path = root.join("sync/tokens.yaml");
        let mut tokens: Tokens = discovery::load(&path)?;
        tokens.tokens.retain(|token| token.sha256 != previous);
        atomic_write_text_raw(&path, &serde_yaml_ng::to_string(&tokens)?)?;
    }
    old["name"] = json!(node.name);
    old["role"] = json!(node.role);
    old["token_sha256"] = json!(hash);
    old["last_seen"] = json!(chrono::Utc::now());
    save_clients(root, &all)
}
pub(super) fn revoke(root: &Path, hash: &str) -> Result<()> {
    let _lock = lock_file_reentrant(&root.join("sync/.lock"))?;
    let mut all = clients(root)?;
    all.retain(|_, c| c["token_sha256"].as_str() != Some(hash));
    save_clients(root, &all)
}
pub(super) fn caller(root: &Path, hash: &str) -> Result<String> {
    clients(root)?
        .into_iter()
        .find(|(_, c)| {
            c["token_sha256"]
                .as_str()
                .is_some_and(|h| bool::from(h.as_bytes().ct_eq(hash.as_bytes())))
        })
        .map(|(id, _)| id)
        .ok_or_else(|| invalid("NODE_AUTH_FAILED: Token is not bound to a node"))
}
fn touch(root: &Path, id: &str) -> Result<()> {
    let _lock = lock_file_reentrant(&root.join("sync/.lock"))?;
    let mut all = clients(root)?;
    let c = all.get_mut(id).ok_or_else(|| invalid("NODE_AUTH_FAILED"))?;
    c["last_seen"] = json!(chrono::Utc::now());
    save_clients(root, &all)?;
    discovery::update_presence(root, id, |p| {
        p.last_seen = Some(chrono::Utc::now());
        p.bye = None;
    })
}
pub(super) fn node_statuses(root: &Path) -> Result<Vec<discovery::Status>> {
    require_server(root)?;
    let journal = Journal::open(root)?;
    let head = Hub::open(root)?.ingest()?;
    let interval = discovery::config(root)?.interval;
    let presence: BTreeMap<String, discovery::Presence> =
        discovery::load(&root.join("sync/presence.yaml"))?;
    let mut result = vec![discovery::Status {
        node: journal.node.node_id.clone(),
        name: journal.node.name.clone(),
        role: NodeRole::Server,
        status: "online".into(),
        code: "ONLINE".into(),
        path: "local".into(),
        message: String::new(),
        since: None,
        last_seen: Some(chrono::Utc::now()),
        cursor: head,
        lag: 0,
    }];
    for (id, c) in clients(root)? {
        let seen = c["last_seen"]
            .as_str()
            .and_then(|t| t.parse::<chrono::DateTime<chrono::Utc>>().ok());
        let online = seen.is_some_and(|at| {
            (chrono::Utc::now() - at).num_seconds() < interval.saturating_mul(3) as i64
        }) && presence.get(&id).is_none_or(|p| p.bye.is_none());
        let cursor = c["cursor"].as_u64().unwrap_or(0);
        result.push(discovery::Status {
            node: id,
            name: c["name"].as_str().unwrap_or("client").into(),
            role: serde_json::from_value(c["role"].clone()).unwrap_or(NodeRole::Laptop),
            status: if online { "online" } else { "offline" }.into(),
            code: if online { "ONLINE" } else { "OFFLINE" }.into(),
            path: "server".into(),
            message: String::new(),
            since: None,
            last_seen: seen,
            cursor,
            lag: head.saturating_sub(cursor),
        });
    }
    Ok(result)
}
#[derive(Serialize, Deserialize)]
pub(super) struct Hello {
    pub node_card: NodeIdentity,
    #[serde(default)]
    pub cursor: Option<serde_json::Value>,
    pub protocol: u32,
    pub min_protocol: u32,
}
#[derive(Serialize, Deserialize)]
pub(super) struct HelloReply {
    pub server: NodeIdentity,
    pub store_id: String,
    pub hub_epoch: String,
    pub head: u64,
    pub floor: u64,
    pub your_head: u64,
    pub empty: bool,
    /// Received by every active client; bounds client pruning (optional field).
    #[serde(default)]
    pub stable: crate::core::sync::VersionVector,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Push {
    pub ops: Vec<Operation>,
}
#[derive(Serialize, Deserialize)]
pub(super) struct PushReply {
    pub heads: BTreeMap<String, u64>,
    pub head: u64,
}
#[derive(Default, Deserialize)]
struct Pull {
    #[serde(default)]
    after: u64,
    limit: Option<usize>,
    wait: Option<u64>,
}

pub(super) fn routes(root: PathBuf) -> Router {
    Router::new()
        .route("/sync/v3/hello", axum::routing::post(hello))
        .route(
            "/sync/v3/ops",
            get(pull).post(push).layer(DefaultBodyLimit::max(256 << 20)),
        )
        .route("/sync/v3/stream", get(stream))
        .route("/sync/v3/snapshot", get(snapshot))
        .route("/sync/v3/bye", axum::routing::post(bye))
        .route(
            "/sync/v3/logs/{project}/{task}",
            get(get_log)
                .put(put_log)
                .layer(DefaultBodyLimit::max(8 << 20)),
        )
        .with_state(root)
}
async fn hello(
    State(root): State<PathBuf>,
    Extension(hash): Extension<String>,
    Json(request): Json<Hello>,
) -> Response {
    match sync::blocking(move || {
        require_server(&root)?;
        crate::core::sync::accept_store_protocol(request.protocol, request.min_protocol)?;
        let id = caller(&root, &hash)?;
        if id != request.node_card.node_id {
            return Err(invalid(
                "NODE_AUTH_FAILED: node identity differs from token",
            ));
        }
        let journal = Journal::open(&root)?;
        journal.reconcile()?;
        touch(&root, &id)?;
        let hub = Hub::open(&root)?;
        let head = hub.ingest()?;
        Ok(HelloReply {
            server: journal.node.clone(),
            store_id: journal.node.store_id.clone(),
            hub_epoch: hub.epoch()?,
            head,
            floor: hub.floor()?,
            your_head: journal.vector()?.get(&id).copied().unwrap_or(0),
            empty: ProjectStore::at(&root).list()?.is_empty(),
            stable: hub.stable_vector()?,
        })
    })
    .await
    {
        Ok(reply) => Json(reply).into_response(),
        Err(e) => sync::failure(e),
    }
}
async fn push(
    State(root): State<PathBuf>,
    Extension(hash): Extension<String>,
    Json(push): Json<Push>,
) -> Response {
    if push.ops.len() > 1000 {
        return error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "At most 1000 operations per batch",
        );
    }
    match sync::blocking(move || {
        require_server(&root)?;
        let id = caller(&root, &hash)?;
        let journal = Journal::open(&root)?;
        for op in &push.ops {
            if op.v > crate::core::sync::max_op_version(SYNC_PROTOCOL) {
                return Err(invalid("SYNC_OP_FORMAT_TOO_NEW"));
            }
            if op.store != journal.node.store_id {
                return Err(invalid("SYNC_STORE_MISMATCH"));
            }
            if let Some(hash) = op.blob_hash() {
                journal.blob(hash)?;
            }
        }
        journal.apply_remote(
            &push.ops,
            discovery::config(&root)?.max_skew.saturating_mul(1000),
        )?;
        let vector = journal.vector()?;
        let heads = push
            .ops
            .iter()
            .map(|op| {
                (
                    op.hlc.2.clone(),
                    vector.get(&op.hlc.2).copied().unwrap_or(0),
                )
            })
            .collect();
        let head = Hub::open(&root)?.ingest()?;
        touch(&root, &id)?;
        Ok(PushReply { heads, head })
    })
    .await
    {
        Ok(reply) => {
            sync::changed();
            Json(reply).into_response()
        }
        Err(e) => sync::failure(e),
    }
}
async fn pull(
    State(root): State<PathBuf>,
    Extension(hash): Extension<String>,
    Query(query): Query<Pull>,
) -> Response {
    let limit = query.limit.unwrap_or(500);
    if !(1..=1000).contains(&limit) || query.wait.unwrap_or(0) > 30 {
        return error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "limit=1..1000; wait=0..30",
        );
    }
    let mut changes = sync::subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(query.wait.unwrap_or(0));
    loop {
        let (root, hash) = (root.clone(), hash.clone());
        let page = sync::blocking(move || {
            require_server(&root)?;
            let id = caller(&root, &hash)?;
            let hub = Hub::open(&root)?;
            hub.ingest()?;
            let page = hub.page(query.after, limit, 8 << 20)?;
            hub.record_cursor(&id, query.after)?;
            touch(&root, &id)?;
            Ok(page)
        })
        .await;
        match page {
            Ok(page) if page.ops.is_empty() && tokio::time::Instant::now() < deadline => {
                tokio::select! {_=changes.changed()=>(),_=tokio::time::sleep_until(deadline)=>()}
            }
            Ok(page) => return Json(page).into_response(),
            Err(e) => return sync::failure(e),
        }
    }
}
async fn stream(
    State(root): State<PathBuf>,
    Extension(hash): Extension<String>,
    Query(query): Query<Pull>,
) -> Response {
    let id = match caller(&root, &hash).and_then(|id| {
        require_server(&root)?;
        let hub = Hub::open(&root)?;
        hub.ingest()?;
        hub.page(query.after, 1, 8 << 20)?;
        hub.record_cursor(&id, query.after)?;
        Ok(id)
    }) {
        Ok(id) => id,
        Err(e) => return sync::failure(e),
    };
    let body = futures_util::stream::unfold(
        (root, id, query.after, sync::subscribe(), sync::stopping()),
        |(root, id, mut after, mut changes, mut stopping)| async move {
            loop {
                if *stopping.borrow() {
                    return None;
                }
                let (r, c) = (root.clone(), id.clone());
                let page = match sync::blocking(move || {
                    let hub = Hub::open(&r)?;
                    hub.ingest()?;
                    let page = hub.page(after, 500, 8 << 20)?;
                    touch(&r, &c)?;
                    Ok(page)
                })
                .await
                {
                    Ok(p) => p,
                    Err(_) => return None,
                };
                if !page.ops.is_empty() {
                    after = page.next;
                    let data = serde_json::to_string(&page).ok()?;
                    return Some((
                        Ok::<_, Infallible>(format!("event: ops\nid: {after}\ndata: {data}\n\n")),
                        (root, id, after, changes, stopping),
                    ));
                }
                tokio::select! {
                    r=tokio::time::timeout(Duration::from_secs(15),changes.changed())=>match r{
                        Ok(Ok(()))=>(),Ok(Err(_))=>return None,Err(_)=>return Some((Ok(": keepalive\n\n".to_string()),(root,id,after,changes,stopping))),
                    },_=stopping.changed()=>return None,
                }
            }
        },
    );
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (
                axum::http::HeaderName::from_static("x-accel-buffering"),
                "no",
            ),
        ],
        axum::body::Body::from_stream(body),
    )
        .into_response()
}
async fn snapshot(State(root): State<PathBuf>) -> Response {
    let spool = sync::blocking(move || {
        require_server(&root)?;
        crate::core::sync::transport::snapshot_spool(&root)
    })
    .await;
    match spool {
        Ok(spool) => match git::bundle_stream(spool).await {
            Ok(body) => (
                [(header::CONTENT_TYPE, "application/x-ndjson")],
                axum::body::Body::from_stream(body),
            )
                .into_response(),
            Err(e) => sync::failure(e),
        },
        Err(e) => sync::failure(e),
    }
}
async fn bye(
    State(root): State<PathBuf>,
    Extension(hash): Extension<String>,
    Json(bye): Json<discovery::Bye>,
) -> Response {
    match (|| {
        require_server(&root)?;
        let id = caller(&root, &hash)?;
        if !matches!(bye.reason.as_str(), "shutdown" | "sleep") {
            return Err(invalid("reason must be shutdown or sleep"));
        }
        discovery::update_presence(&root, &id, |p| {
            p.last_seen = Some(bye.at);
            p.bye = Some(bye);
        })
    })() {
        Ok(()) => Json(json!({})).into_response(),
        Err(e) => sync::failure(e),
    }
}
async fn put_log(
    State(root): State<PathBuf>,
    Extension(hash): Extension<String>,
    Params((uid, task)): Params<(String, String)>,
    Json(snapshot): Json<crate::core::runner_log::Snapshot>,
) -> Response {
    match sync::blocking(move || {
        require_server(&root)?;
        let id = caller(&root, &hash)?;
        if snapshot.node_id != id || snapshot.project != uid || snapshot.task != task {
            return Err(invalid("NODE_AUTH_FAILED: Runner log identity differs"));
        }
        crate::core::runner_log::validate_snapshot(&root, &snapshot)?;
        crate::core::runner_log::cache_snapshot(&root, &snapshot)
    })
    .await
    {
        Ok(()) => Json(json!({})).into_response(),
        Err(e) => sync::failure(e),
    }
}
async fn get_log(
    State(root): State<PathBuf>,
    Params((uid, task)): Params<(String, String)>,
) -> Response {
    match sync::blocking(move || {
        require_server(&root)?;
        let journal = Journal::open(&root)?;
        let alias = journal
            .project_uids()?
            .into_iter()
            .find(|(_, u)| u == &uid)
            .map(|(a, _)| a)
            .ok_or_else(|| invalid("Unknown project UID"))?;
        let board = root.join("projects").join(alias);
        let run = crate::core::runner_log::latest_run(&board, &task)?;
        if run.node_id == journal.node.node_id {
            crate::core::runner_log::local_tail(&board, &task)
        } else {
            crate::core::runner_log::cached_tail(&board, &task, &run.node_id)
        }
    })
    .await
    {
        Ok(log) => Json(log).into_response(),
        Err(e) => sync::failure(e),
    }
}
