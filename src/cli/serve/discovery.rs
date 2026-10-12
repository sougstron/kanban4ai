//! LAN discovery is advisory. Only paired bearer credentials establish identity.
use super::*;
use crate::core::sync::{Journal, NodeIdentity, NodeRole};
use axum::Extension;
use chrono::{DateTime, Local, Utc};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::BTreeMap;

const SERVICE: &str = "_kanban4ai._tcp.local.";

#[derive(clap::Args, Default)]
#[group(id = "discovery_action", multiple = false)]
pub struct DiscoveryOptions {
    /// Discover LAN services for sync.probe_timeout seconds and print JSON.
    #[arg(long, conflicts_with_all = ["issue_token", "pair_code", "pair", "revoke_token", "phone_qr"])]
    discover: bool,
    /// Probe all paired peers and print explicit availability codes as JSON.
    #[arg(long, conflicts_with_all = ["issue_token", "pair_code", "pair", "revoke_token", "phone_qr"])]
    probe: bool,
    /// Notify peers before shutdown or sleep (for systemd hooks).
    #[arg(long, value_parser = ["shutdown", "sleep"], conflicts_with_all = ["issue_token", "pair_code", "pair", "revoke_token", "phone_qr"])]
    bye: Option<String>,
    /// Notify peers after resume; also sent automatically on serve startup.
    #[arg(long, conflicts_with_all = ["issue_token", "pair_code", "pair", "revoke_token", "phone_qr"])]
    hello: bool,
    /// Run one sync cycle with every paired peer and exit (serve does this
    /// continuously on its own).
    #[arg(long, conflicts_with_all = ["issue_token", "pair_code", "pair", "revoke_token", "phone_qr"])]
    sync: bool,
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub(super) struct Config {
    pub(super) home_networks: Vec<IpNet>,
    pub(super) probe_timeout: u64,
    pub(super) interval: u64,
    pub(super) max_skew: u64,
    pub(super) peers: Vec<PeerCard>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            home_networks: Vec::new(),
            probe_timeout: 3,
            interval: 30,
            max_skew: 300,
            peers: Vec::new(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct PeerCard {
    #[serde(flatten)]
    pub(super) node: NodeIdentity,
    pub(super) address: SocketAddr,
}
#[derive(Deserialize)]
pub(super) struct Credential {
    pub(super) node_id: String,
    pub(super) token: String,
    inbound_sha256: String,
    #[serde(default)]
    address: Option<SocketAddr>,
    #[serde(default)]
    addresses: Vec<SocketAddr>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Bye {
    pub(super) reason: String,
    pub(super) at: DateTime<Utc>,
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub(super) struct Presence {
    #[serde(default)]
    pub(super) last_seen: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(super) bye: Option<Bye>,
    #[serde(default)]
    discovered: Vec<SocketAddr>,
}
#[derive(Serialize)]
struct Status {
    node: String,
    status: &'static str,
    code: &'static str,
    message: String,
    since: Option<DateTime<Utc>>,
    last_seen: Option<DateTime<Utc>>,
}
#[derive(Serialize, Deserialize)]
struct Version {
    proto: u32,
    min_proto: u32,
}
#[derive(Serialize, Deserialize)]
struct NodeCard {
    #[serde(flatten)]
    node: NodeIdentity,
    app_version: String,
    proto: u32,
    min_proto: u32,
    hlc: crate::core::sync::Hlc,
    started_at: DateTime<Utc>,
    wol: Option<serde_json::Value>,
}

pub(super) fn load<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(serde_yaml_ng::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn config(root: &Path) -> Result<Config> {
    let global = ProjectStore::at(root).load_global_config()?;
    let config: Config = global
        .extras
        .get("sync")
        .cloned()
        .map(serde_yaml_ng::from_value)
        .transpose()?
        .unwrap_or_default();
    if config.probe_timeout == 0 || config.interval == 0 {
        return Err(invalid(
            "sync.probe_timeout and sync.interval must be positive",
        ));
    }
    Ok(config)
}
pub(super) fn update_presence(
    root: &Path,
    id: &str,
    update: impl FnOnce(&mut Presence),
) -> Result<()> {
    let _lock = lock_file_reentrant(&root.join("sync/.lock"))?;
    let path = root.join("sync/presence.yaml");
    let mut state: BTreeMap<String, Presence> = load(&path)?;
    update(state.entry(id.into()).or_default());
    atomic_write_text_raw(&path, &serde_yaml_ng::to_string(&state)?)
}
pub(super) fn in_lan(config: &Config) -> Result<bool> {
    Ok(if_addrs::get_if_addrs()?.iter().any(|i| {
        let ip = i.ip();
        !ip.is_loopback() && allowed(ip, &config.home_networks)
    }))
}
pub(super) fn peer_addresses(
    peer: &PeerCard,
    secret: Option<&Credential>,
    presence: &Presence,
    config: &Config,
) -> Vec<SocketAddr> {
    if peer.node.outbound_only() {
        return Vec::new();
    }
    let mut result = presence.discovered.clone();
    if let Some(secret) = secret {
        result.extend(secret.address);
        result.extend(&secret.addresses);
    }
    result.push(peer.address);
    result.retain(|a| a.port() != 0 && allowed(a.ip(), &config.home_networks));
    let mut seen = BTreeSet::new();
    result.retain(|a| seen.insert(*a));
    result
}
fn display_node(node: &NodeIdentity) -> &'static str {
    match node.role {
        NodeRole::Server => "Сервер",
        NodeRole::Laptop => "Ноут",
        NodeRole::Mobile => "Телефон",
    }
}
fn time_text(at: DateTime<Utc>) -> String {
    let at = at.with_timezone(&Local);
    if at.date_naive() == Local::now().date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%d.%m %H:%M").to_string()
    }
}
fn status(
    peer: &PeerCard,
    presence: &Presence,
    code: &'static str,
    version: Option<&Version>,
) -> Status {
    let node = display_node(&peer.node);
    let since = presence.bye.as_ref().map(|bye| bye.at);
    let message = match code {
        "ONLINE" => format!("{node} в сети"),
        "OFFLINE" => presence.last_seen.map_or_else(
            || format!("{node}: ещё не был в сети"),
            |at| format!("{node}: не в сети с {}", time_text(at)),
        ),
        "NODE_OUT_OF_LAN" => format!(
            "Вы вне домашней сети — {node} недоступен. Изменения сохранятся и уйдут при возвращении"
        ),
        "NODE_POWERED_OFF" => {
            let bye = presence.bye.as_ref().expect("powered off requires bye");
            let verb = if bye.reason == "sleep" {
                "спит"
            } else {
                "выключен"
            };
            format!("{node} {verb} с {}", time_text(bye.at))
        }
        "NODE_UNREACHABLE" => {
            let contact = presence
                .last_seen
                .map(|at| {
                    format!(
                        "{} ({} мин назад)",
                        time_text(at),
                        (Utc::now() - at).num_minutes().max(0)
                    )
                })
                .unwrap_or_else(|| "ещё не было".into());
            format!("{node} не отвечает. Последний контакт: {contact}")
        }
        "NODE_SERVICE_DOWN" => format!("{node} включён, но kanban4ai на нём не запущен"),
        "NODE_AUTH_FAILED" => {
            format!("{node} не принимает этот узел: сопряжение отозвано или не выполнено")
        }
        "NODE_VERSION_MISMATCH" => {
            let remote = version.map(|v| v.proto).unwrap_or(0);
            let older = if remote < PROTO {
                node
            } else {
                "этот узел"
            };
            format!(
                "На {node} несовместимая версия kanban4ai ({remote}, здесь {PROTO}). Обновите {older}"
            )
        }
        "SYNC_STORE_MISMATCH" => {
            format!("На {node} другая база kanban4ai. Выберите, какую оставить")
        }
        "SYNC_CLOCK_SKEW" => format!(
            "Часы на {node} расходятся с этим узлом. Синхронизация приостановлена — включите NTP"
        ),
        _ => unreachable!(),
    };
    Status {
        node: peer.node.node_id.clone(),
        status: if code == "ONLINE" {
            "online"
        } else {
            "offline"
        },
        code,
        message,
        since,
        last_seen: presence.last_seen,
    }
}
fn compatible(version: &Version) -> bool {
    version.proto >= MIN_PROTO && version.min_proto <= PROTO && version.min_proto <= version.proto
}
fn client(config: &Config) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(config.probe_timeout))
        .build()
        .map_err(|e| invalid(e.to_string()))
}
async fn probe_peer(
    root: &Path,
    config: &Config,
    peer: &PeerCard,
    secret: Option<&Credential>,
    presence: &Presence,
    lan: bool,
) -> Result<Status> {
    if peer.node.outbound_only() {
        let online = presence.bye.is_none()
            && presence.last_seen.is_some_and(|seen| {
                (Utc::now() - seen).num_seconds() < config.interval.saturating_mul(3) as i64
            });
        return Ok(status(
            peer,
            presence,
            if online { "ONLINE" } else { "OFFLINE" },
            None,
        ));
    }
    if !lan {
        return Ok(status(peer, presence, "NODE_OUT_OF_LAN", None));
    }
    if presence
        .bye
        .as_ref()
        .is_some_and(|bye| presence.last_seen.is_none_or(|seen| seen <= bye.at))
    {
        return Ok(status(peer, presence, "NODE_POWERED_OFF", None));
    }
    let http = client(config)?;
    let local = Journal::open(root)?.node;
    let mut code = "NODE_UNREACHABLE";
    let mut remote_version = None;
    for address in peer_addresses(peer, secret, presence, config) {
        match tokio::time::timeout(
            Duration::from_secs(config.probe_timeout),
            tokio::net::TcpStream::connect(address),
        )
        .await
        {
            Ok(Ok(stream)) => drop(stream),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                if code == "NODE_UNREACHABLE" {
                    code = "NODE_SERVICE_DOWN";
                }
                continue;
            }
            _ => continue,
        }
        let base = format!("http://{address}");
        let Ok(reply) = http.get(format!("{base}/version")).send().await else {
            continue;
        };
        let Ok(version) = reply.json::<Version>().await else {
            continue;
        };
        if !compatible(&version) {
            code = "NODE_VERSION_MISMATCH";
            remote_version = Some(version);
            continue;
        }
        let Some(secret) = secret else {
            code = "NODE_AUTH_FAILED";
            continue;
        };
        let Ok(reply) = http
            .get(format!("{base}/node"))
            .bearer_auth(&secret.token)
            .send()
            .await
        else {
            continue;
        };
        if reply.status() == StatusCode::UNAUTHORIZED || reply.status() == StatusCode::FORBIDDEN {
            code = "NODE_AUTH_FAILED";
            continue;
        }
        if !reply.status().is_success() {
            continue;
        }
        let Ok(card) = reply.json::<NodeCard>().await else {
            continue;
        };
        if card.node.node_id != peer.node.node_id {
            code = "NODE_AUTH_FAILED";
            continue;
        }
        let now = Utc::now();
        update_presence(root, &peer.node.node_id, |p| {
            p.last_seen = Some(now);
            p.bye = None;
        })?;
        let mut contacted = presence.clone();
        contacted.last_seen = Some(now);
        contacted.bye = None;
        if card.node.store_id != local.store_id {
            return Ok(status(peer, &contacted, "SYNC_STORE_MISMATCH", None));
        }
        if now.timestamp_millis().abs_diff(card.hlc.0 as i64) > config.max_skew.saturating_mul(1000)
        {
            return Ok(status(peer, &contacted, "SYNC_CLOCK_SKEW", None));
        }
        return Ok(status(peer, &contacted, "ONLINE", None));
    }
    Ok(status(peer, presence, code, remote_version.as_ref()))
}
async fn probes(root: &Path) -> Result<Vec<Status>> {
    let config = config(root)?;
    let credentials: Vec<Credential> = load(&root.join("sync/peers.yaml"))?;
    let presence: BTreeMap<String, Presence> = load(&root.join("sync/presence.yaml"))?;
    let lan = in_lan(&config)?;
    let mut statuses = Vec::with_capacity(config.peers.len());
    for peer in &config.peers {
        statuses.push(
            probe_peer(
                root,
                &config,
                peer,
                credentials.iter().find(|c| c.node_id == peer.node.node_id),
                presence
                    .get(&peer.node.node_id)
                    .unwrap_or(&Presence::default()),
                lan,
            )
            .await?,
        );
    }
    Ok(statuses)
}

#[derive(Clone)]
struct HttpState {
    root: PathBuf,
    started_at: DateTime<Utc>,
}
pub(super) fn routes(root: PathBuf) -> Router {
    Router::new()
        .route("/node", get(node))
        .route("/nodes", get(nodes))
        .route("/sync/bye", axum::routing::post(bye))
        .route("/sync/hello", axum::routing::post(hello))
        .with_state(HttpState {
            root,
            started_at: Utc::now(),
        })
}
pub(super) fn caller(root: &Path, hash: &str) -> Result<String> {
    let credentials: Vec<Credential> = load(&root.join("sync/peers.yaml"))?;
    credentials
        .into_iter()
        .find(|c| c.inbound_sha256.as_bytes().ct_eq(hash.as_bytes()).into())
        .map(|c| c.node_id)
        .ok_or_else(|| invalid("NODE_AUTH_FAILED"))
}
async fn node(State(state): State<HttpState>, Extension(hash): Extension<String>) -> Response {
    let result = (|| -> Result<NodeCard> {
        let journal = Journal::open(&state.root)?;
        // A paired request is a fresh contact.
        if let Ok(id) = caller(&state.root, &hash) {
            update_presence(&state.root, &id, |p| {
                p.last_seen = Some(Utc::now());
                p.bye = None;
            })?;
        }
        let global = ProjectStore::at(&state.root).load_global_config()?;
        let wol = if journal.node.role == NodeRole::Server {
            global
                .extras
                .get("node")
                .and_then(|n| n.get("wol"))
                .map(serde_json::to_value)
                .transpose()
                .map_err(|e| invalid(e.to_string()))?
        } else {
            None
        };
        let mut hlc: crate::core::sync::Hlc = load(&state.root.join("sync/hlc"))?;
        hlc.0 = hlc.0.max(Utc::now().timestamp_millis() as u64);
        hlc.2 = journal.node.node_id.clone();
        Ok(NodeCard {
            node: journal.node,
            app_version: env!("CARGO_PKG_VERSION").into(),
            proto: PROTO,
            min_proto: MIN_PROTO,
            hlc,
            started_at: state.started_at,
            wol,
        })
    })();
    match result {
        Ok(card) => Json(card).into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            &e.to_string(),
        ),
    }
}
async fn nodes(State(state): State<HttpState>) -> Response {
    match probes(&state.root).await {
        Ok(statuses) => Json(statuses).into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            &e.to_string(),
        ),
    }
}
async fn bye(
    State(state): State<HttpState>,
    Extension(hash): Extension<String>,
    Json(bye): Json<Bye>,
) -> Response {
    if !matches!(bye.reason.as_str(), "sleep" | "shutdown") {
        return error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "reason must be shutdown or sleep",
        );
    }
    let id = match caller(&state.root, &hash) {
        Ok(id) => id,
        Err(_) => {
            return error(
                StatusCode::UNAUTHORIZED,
                "NODE_AUTH_FAILED",
                "Only paired nodes can announce departure",
            );
        }
    };
    match update_presence(&state.root, &id, |p| {
        if p.last_seen.is_none_or(|seen| seen <= bye.at) {
            p.last_seen = Some(bye.at);
            p.bye = Some(bye);
        }
    }) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            &e.to_string(),
        ),
    }
}
async fn hello(State(state): State<HttpState>, Extension(hash): Extension<String>) -> Response {
    let id = match caller(&state.root, &hash) {
        Ok(id) => id,
        Err(_) => {
            return error(
                StatusCode::UNAUTHORIZED,
                "NODE_AUTH_FAILED",
                "Only paired nodes can announce arrival",
            );
        }
    };
    match update_presence(&state.root, &id, |p| {
        p.last_seen = Some(Utc::now());
        p.bye = None;
    }) {
        Ok(()) => {
            // The peer is back: catch up now instead of at the next interval.
            super::sync::kick();
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            &e.to_string(),
        ),
    }
}
pub(super) async fn notify(root: &Path, reason: Option<&str>) -> Result<()> {
    let config = config(root)?;
    if !in_lan(&config)? {
        return Ok(());
    }
    if reason.is_some() {
        // Push the journal tail first; an unreachable peer must not block sleep.
        for (peer, result) in super::sync::once(root, false).await? {
            if let Err(e) = result {
                eprintln!("sync {peer} before bye: {e}");
            }
        }
    }
    let http = client(&config)?;
    let credentials: Vec<Credential> = load(&root.join("sync/peers.yaml"))?;
    let presence: BTreeMap<String, Presence> = load(&root.join("sync/presence.yaml"))?;
    let mut failed = Vec::new();
    for peer in &config.peers {
        if peer.node.outbound_only() {
            continue;
        }
        let Some(secret) = credentials.iter().find(|s| s.node_id == peer.node.node_id) else {
            failed.push(peer.node.name.clone());
            continue;
        };
        let mut sent = false;
        for address in peer_addresses(
            peer,
            Some(secret),
            presence
                .get(&peer.node.node_id)
                .unwrap_or(&Presence::default()),
            &config,
        ) {
            let path = if reason.is_some() { "bye" } else { "hello" };
            let request = http
                .post(format!("http://{address}/sync/{path}"))
                .bearer_auth(&secret.token);
            let request = if let Some(reason) = reason {
                request.json(&Bye {
                    reason: reason.into(),
                    at: Utc::now(),
                })
            } else {
                request
            };
            if request.send().await.is_ok_and(|r| r.status().is_success()) {
                sent = true;
                if reason.is_none() {
                    update_presence(root, &peer.node.node_id, |p| {
                        p.last_seen = Some(Utc::now());
                        p.bye = None;
                    })?;
                }
                break;
            }
        }
        if !sent {
            failed.push(peer.node.name.clone());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "Cannot notify peers: {}",
            failed.join(", ")
        )))
    }
}

async fn rediscovered(root: &Path, discovered: Discovered) -> Result<()> {
    let config = config(root)?;
    let Some(peer) = config
        .peers
        .iter()
        .find(|p| p.node.node_id == discovered.node_id)
    else {
        return Ok(());
    };
    update_presence(root, &discovered.node_id, |p| {
        p.discovered = discovered.addresses
    })?;
    let state: BTreeMap<String, Presence> = load(&root.join("sync/presence.yaml"))?;
    let mut presence = state.get(&discovered.node_id).cloned().unwrap_or_default();
    // A fresh announcement may follow a missed hello. Do not clear the durable
    // bye on mDNS alone: only a successful authenticated /node clears it.
    presence.bye = None;
    let credentials: Vec<Credential> = load(&root.join("sync/peers.yaml"))?;
    let secret = credentials.iter().find(|c| c.node_id == discovered.node_id);
    probe_peer(root, &config, peer, secret, &presence, in_lan(&config)?).await?;
    super::sync::kick();
    Ok(())
}

#[derive(Serialize)]
struct Discovered {
    node_id: String,
    name: String,
    role: String,
    proto: String,
    addresses: Vec<SocketAddr>,
}
fn resolved(info: &ServiceInfo, networks: &[IpNet]) -> Option<Discovered> {
    let node_id = info.get_property_val_str("node_id")?.to_owned();
    let name = info.get_property_val_str("name")?.to_owned();
    let role = info.get_property_val_str("role")?.to_owned();
    let proto = info.get_property_val_str("proto")?.to_owned();
    let mut addresses: Vec<_> = info
        .get_addresses()
        .iter()
        .filter(|ip| !ip.is_loopback() && allowed(**ip, networks))
        .map(|ip| SocketAddr::new(*ip, info.get_port()))
        .collect();
    addresses.sort();
    if addresses.is_empty() || info.get_port() == 0 {
        return None;
    }
    Some(Discovered {
        node_id,
        name,
        role,
        proto,
        addresses,
    })
}
fn mdns_error(e: mdns_sd::Error) -> KanbanError {
    invalid(e.to_string())
}
fn browser(networks: &[IpNet]) -> Result<(ServiceDaemon, flume::Receiver<ServiceEvent>)> {
    let daemon = ServiceDaemon::new().map_err(mdns_error)?;
    let setup = (|| {
        daemon.disable_interface(mdns_sd::IfKind::All)?;
        for interface in if_addrs::get_if_addrs().map_err(|e| mdns_sd::Error::Msg(e.to_string()))? {
            let ip = interface.ip();
            if !ip.is_loopback() && allowed(ip, networks) {
                daemon.enable_interface(mdns_sd::IfKind::Addr(ip))?;
            }
        }
        daemon.browse(SERVICE)
    })();
    match setup {
        Ok(receiver) => Ok((daemon, receiver)),
        Err(e) => {
            let _ = daemon.shutdown();
            Err(mdns_error(e))
        }
    }
}
async fn discover(root: &Path) -> Result<Vec<Discovered>> {
    let config = config(root)?;
    if !in_lan(&config)? {
        return Ok(Vec::new());
    }
    let local_id = Journal::open(root)?.node.node_id;
    let (daemon, receiver) = browser(&config.home_networks)?;
    let mut found = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.probe_timeout);
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, receiver.recv_async()).await {
        if let ServiceEvent::ServiceResolved(info) = event {
            if let Some(peer) = resolved(&info, &config.home_networks) {
                if peer.node_id != local_id {
                    found.insert(peer.node_id.clone(), peer);
                }
            }
        }
    }
    let stopped = daemon.shutdown().map_err(mdns_error)?;
    let _ = stopped.recv_async().await;
    Ok(found.into_values().collect())
}

pub(super) struct Running {
    daemon: Option<ServiceDaemon>,
    worker: tokio::task::JoinHandle<()>,
}
impl Running {
    pub(super) async fn stop(self) {
        self.worker.abort();
        let _ = self.worker.await;
        if let Some(daemon) = self.daemon {
            if let Ok(stopped) = daemon.shutdown() {
                let _ = stopped.recv_async().await;
            }
        }
    }
}
pub(super) async fn start(root: &Path, networks: &[IpNet], port: u16) -> Result<Running> {
    let config = config(root)?;
    let node = Journal::open(root)?.node;
    let ips: Vec<_> = if node.outbound_only() {
        BTreeSet::new()
    } else {
        addresses(networks, port)?
    }
    .iter()
    .map(|a| a.ip())
    .filter(|ip| !ip.is_loopback())
    .collect();
    let mdns = (|| -> Result<_> {
        let (daemon, receiver) = browser(networks)?;
        let role = match node.role {
            NodeRole::Server => "server",
            NodeRole::Laptop => "laptop",
            NodeRole::Mobile => "mobile",
        };
        let properties = [
            ("node_id", node.node_id.as_str()),
            ("name", node.name.as_str()),
            ("role", role),
            ("proto", "1"),
        ];
        if !ips.is_empty() {
            let info = ServiceInfo::new(
                SERVICE,
                &node.node_id,
                &format!("{}.local.", node.node_id),
                ips.as_slice(),
                port,
                &properties[..],
            )
            .map_err(mdns_error)?;
            daemon.register(info).map_err(mdns_error)?;
        }
        Ok((daemon, receiver))
    })();
    let (daemon, receiver) = match mdns {
        Ok((d, r)) => (Some(d), Some(r)),
        Err(e) => {
            eprintln!("mDNS unavailable; using static peers: {e}");
            (None, None)
        }
    };
    let root = root.to_path_buf();
    let worker = tokio::spawn(async move {
        if let Err(e) = notify(&root, None).await {
            eprintln!("hello: {e}");
        }
        let mut interval = tokio::time::interval(Duration::from_secs(config.interval));
        loop {
            tokio::select! {
                _ = interval.tick() => { if let Err(e) = probes(&root).await { eprintln!("peer probe: {e}"); } }
                event = async { match &receiver { Some(r) => r.recv_async().await.ok(), None => std::future::pending().await } } => {
                    if let Some(ServiceEvent::ServiceResolved(info)) = event {
                        if let Some(peer) = resolved(&info, &config.home_networks) {
                            if peer.node_id != node.node_id {
                                if let Err(e) = rediscovered(&root, peer).await { eprintln!("discovery: {e}"); }
                            }
                        }
                    }
                }
            }
        }
    });
    Ok(Running { daemon, worker })
}
pub(super) fn run(store: &ProjectStore, options: DiscoveryOptions) -> Result<bool> {
    if !options.discover
        && !options.probe
        && options.bye.is_none()
        && !options.hello
        && !options.sync
    {
        return Ok(false);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            if options.discover {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&discover(store.root()).await?)
                        .map_err(|e| invalid(e.to_string()))?
                );
            } else if options.probe {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&probes(store.root()).await?)
                        .map_err(|e| invalid(e.to_string()))?
                );
            } else if options.sync {
                let mut failed = Vec::new();
                for (peer, result) in super::sync::once(store.root(), true).await? {
                    match result {
                        Ok(()) => println!("synced with {peer}"),
                        Err(e) => {
                            eprintln!("sync {peer}: {e}");
                            failed.push(peer);
                        }
                    }
                }
                if !failed.is_empty() {
                    return Err(invalid(format!("Cannot sync with: {}", failed.join(", "))));
                }
            } else {
                notify(store.root(), options.bye.as_deref()).await?;
            }
            Ok::<(), KanbanError>(())
        })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use tower::ServiceExt;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    fn peer(root: &Path, address: SocketAddr) -> PeerCard {
        let mut node = Journal::open(root).unwrap().node;
        node.node_id = "remote".into();
        node.name = "server".into();
        node.role = NodeRole::Server;
        PeerCard { node, address }
    }

    #[test]
    fn mobile_departure_and_stale_contact_are_neutral_even_outside_lan() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let mut mobile = peer(root.path(), "127.0.0.1:1".parse().unwrap());
            mobile.node.role = NodeRole::Mobile;
            let config = Config::default();
            let now = Utc::now();
            for presence in [
                Presence::default(),
                Presence {
                    last_seen: Some(now - chrono::Duration::seconds(91)),
                    ..Default::default()
                },
                Presence {
                    last_seen: Some(now),
                    bye: Some(Bye {
                        reason: "shutdown".into(),
                        at: now,
                    }),
                    ..Default::default()
                },
            ] {
                let result = probe_peer(root.path(), &config, &mobile, None, &presence, false)
                    .await
                    .unwrap();
                assert_eq!(result.code, "OFFLINE");
                assert_eq!(result.status, "offline");
                assert!(!result.message.contains("не отвечает"));
            }
            let presence = Presence {
                last_seen: Some(now),
                ..Default::default()
            };
            let result = probe_peer(root.path(), &config, &mobile, None, &presence, true)
                .await
                .unwrap();
            assert_eq!(result.code, "ONLINE");
            assert!(peer_addresses(&mobile, None, &presence, &config).is_empty());
        });
    }

    #[test]
    fn out_of_lan_precedes_departure_and_departure_precedes_network() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let peer = peer(root.path(), "127.0.0.1:1".parse().unwrap());
            let now = Utc::now();
            let presence = Presence {
                last_seen: Some(now),
                bye: Some(Bye {
                    reason: "sleep".into(),
                    at: now,
                }),
                discovered: Vec::new(),
            };
            let result = probe_peer(
                root.path(),
                &Config::default(),
                &peer,
                None,
                &presence,
                false,
            )
            .await
            .unwrap();
            assert_eq!(result.code, "NODE_OUT_OF_LAN");
            let result = probe_peer(
                root.path(),
                &Config::default(),
                &peer,
                None,
                &presence,
                true,
            )
            .await
            .unwrap();
            assert_eq!(result.code, "NODE_POWERED_OFF");
            assert_eq!(result.since, Some(now));
            assert!(result.message.starts_with("Сервер спит с "));
        });
    }

    #[test]
    fn discovery_failure_falls_back_to_static_address_and_distinguishes_http_errors() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let peer = peer(root.path(), listener.local_addr().unwrap());
            let secret = Credential {
                node_id: peer.node.node_id.clone(),
                token: "a".repeat(64),
                inbound_sha256: "b".repeat(64),
                address: None,
                addresses: Vec::new(),
            };
            let rejected = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            let presence = Presence {
                discovered: vec![rejected],
                ..Presence::default()
            };
            let app = Router::new()
                .route(
                    "/version",
                    get(|| async { Json(json!({"proto": 1, "min_proto": 1})) }),
                )
                .route("/node", get(|| async { StatusCode::UNAUTHORIZED }));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let result = probe_peer(
                root.path(),
                &Config::default(),
                &peer,
                Some(&secret),
                &presence,
                true,
            )
            .await
            .unwrap();
            assert_eq!(result.code, "NODE_AUTH_FAILED");
            server.abort();
            let _ = server.await;
            let result = probe_peer(
                root.path(),
                &Config::default(),
                &peer,
                Some(&secret),
                &Presence::default(),
                true,
            )
            .await
            .unwrap();
            assert_eq!(result.code, "NODE_SERVICE_DOWN");
            let listener = TcpListener::bind(peer.address).await.unwrap();
            let app = Router::new().route(
                "/version",
                get(|| async { Json(json!({"proto": 2, "min_proto": 2})) }),
            );
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let result = probe_peer(
                root.path(),
                &Config::default(),
                &peer,
                Some(&secret),
                &Presence::default(),
                true,
            )
            .await
            .unwrap();
            assert_eq!(result.code, "NODE_VERSION_MISMATCH");
            assert!(result.message.contains("(2, здесь 1)"));
            server.abort();
            let _ = server.await;
        });
    }

    #[test]
    fn paired_bye_is_durable_stale_bye_is_ignored_and_hello_clears_it() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            Journal::open(root.path()).unwrap();
            let secret = "a".repeat(64);
            let inbound = token_hash(&secret);
            atomic_write_text_raw(
                &root.path().join("sync/peers.yaml"),
                &format!(
                    "- node_id: remote\n  token: {}\n  inbound_sha256: {inbound}\n",
                    "b".repeat(64)
                ),
            )
            .unwrap();
            let app = routes(root.path().to_path_buf()).layer(Extension(inbound));
            let now = Utc::now();
            let request = |path: &str, body: serde_json::Value| {
                axum::http::Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap()
            };
            let reply = app
                .clone()
                .oneshot(request(
                    "/sync/bye",
                    json!({"reason": "shutdown", "at": now}),
                ))
                .await
                .unwrap();
            assert_eq!(reply.status(), StatusCode::OK);
            let state: BTreeMap<String, Presence> =
                load(&root.path().join("sync/presence.yaml")).unwrap();
            assert_eq!(state["remote"].bye.as_ref().unwrap().reason, "shutdown");
            let reply = app
                .clone()
                .oneshot(request("/sync/hello", json!({})))
                .await
                .unwrap();
            assert_eq!(reply.status(), StatusCode::OK);
            let reply = app
                .clone()
                .oneshot(request(
                    "/sync/bye",
                    json!({"reason": "sleep", "at": now - chrono::Duration::minutes(1)}),
                ))
                .await
                .unwrap();
            assert_eq!(reply.status(), StatusCode::OK);
            let state: BTreeMap<String, Presence> =
                load(&root.path().join("sync/presence.yaml")).unwrap();
            assert!(state["remote"].bye.is_none());
            assert!(state["remote"].last_seen.unwrap() >= now);
            let unpaired = routes(root.path().to_path_buf()).layer(Extension("c".repeat(64)));
            let reply = unpaired
                .oneshot(request("/sync/bye", json!({"reason": "sleep", "at": now})))
                .await
                .unwrap();
            assert_eq!(reply.status(), StatusCode::UNAUTHORIZED);
            let body = to_bytes(reply.into_body(), 4096).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["code"],
                "NODE_AUTH_FAILED"
            );
        });
    }
}
