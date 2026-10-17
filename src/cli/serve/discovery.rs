//! Server discovery is advisory; bearer credentials establish identity.
use super::*;
use crate::core::sync::{Journal, NodeIdentity, NodeRole};
use chrono::{DateTime, Utc};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::BTreeMap;

const SERVICE: &str = "_kanban4ai._tcp.local.";

#[derive(clap::Args, Default)]
#[group(id = "discovery_action", multiple = true)]
pub struct DiscoveryOptions {
    /// Discover sync servers on the LAN.
    #[arg(long)]
    discover: bool,
    /// Probe the configured sync server.
    #[arg(long)]
    probe: bool,
    /// Push the outbox and announce departure to the server.
    #[arg(long, value_parser = ["shutdown", "sleep"])]
    bye: Option<String>,
    /// Announce arrival to the server.
    #[arg(long)]
    hello: bool,
    /// Run one push/pull cycle with the server.
    #[arg(long)]
    sync: bool,
    /// Explicitly delete this node's old sync/log refs from project origins.
    #[arg(long, conflicts_with_all=["discover","probe","bye","hello","sync","pair","pair_code","revoke_token","phone_qr","issue_token"])]
    purge_relay_refs: bool,
    /// List refs without deleting them (with --purge-relay-refs).
    #[arg(long, requires = "purge_relay_refs")]
    dry_run: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct ServerConfig {
    pub node_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub urls: Vec<String>,
}
#[derive(Clone, Deserialize)]
#[serde(default)]
pub(super) struct Config {
    pub home_networks: Vec<IpNet>,
    pub probe_timeout: u64,
    pub interval: u64,
    pub run_heartbeat: u64,
    pub max_skew: u64,
    pub server: Option<ServerConfig>,
    // One resolved server identity; addresses remain ordered in server.urls.
    #[serde(skip)]
    pub target: Option<ServerCard>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            home_networks: Vec::new(),
            probe_timeout: 3,
            interval: 30,
            run_heartbeat: 60,
            max_skew: 300,
            server: None,
            target: None,
        }
    }
}
#[derive(Clone)]
pub(super) struct ServerCard {
    pub node: NodeIdentity,
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub(super) struct Credential {
    pub node_id: String,
    pub token: String,
    #[serde(default)]
    pub urls: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Bye {
    pub reason: String,
    pub at: DateTime<Utc>,
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub(super) struct Presence {
    pub last_seen: Option<DateTime<Utc>>,
    pub bye: Option<Bye>,
    #[serde(default)]
    discovered: Vec<SocketAddr>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Status {
    pub node: String,
    pub name: String,
    pub role: NodeRole,
    pub status: String,
    pub code: String,
    pub path: String,
    pub message: String,
    pub since: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cursor: u64,
    #[serde(default)]
    pub lag: u64,
}

pub(super) fn load<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(serde_yaml_ng::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn server_credential(root: &Path) -> Result<Option<Credential>> {
    let secret: Credential = load(&root.join("sync/server.yaml"))?;
    Ok((!secret.node_id.is_empty()).then_some(secret))
}
pub(super) fn config(root: &Path) -> Result<Config> {
    let global = ProjectStore::at(root).load_global_config()?;
    let mut config: Config = global
        .extras
        .get("sync")
        .cloned()
        .map(serde_yaml_ng::from_value)
        .transpose()?
        .unwrap_or_default();
    if config.probe_timeout == 0 || config.interval == 0 || config.run_heartbeat == 0 {
        return Err(invalid(
            "sync.probe_timeout, sync.interval and sync.run_heartbeat must be positive",
        ));
    }
    if let Some(server) = &config.server {
        if !server.node_id.is_empty() {
            let mut node = Journal::open(root)?.node;
            node.node_id = server.node_id.clone();
            node.name = server.name.clone();
            node.role = NodeRole::Server;
            node.dial = crate::core::sync::NodeDial::Inbound;
            config.target = Some(ServerCard { node });
        }
    }
    Ok(config)
}
pub(super) fn base_url(address: &str) -> Result<reqwest::Url> {
    let text = if address.contains("://") {
        address.to_owned()
    } else {
        format!("http://{address}")
    };
    let mut url =
        reqwest::Url::parse(&text).map_err(|_| invalid("Expected http(s) URL or host:port"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid(
            "Expected http(s) server origin without credentials, query or path",
        ));
    }
    url.set_path("");
    Ok(url)
}
pub(super) fn base(address: &str) -> String {
    address.trim_end_matches('/').to_string()
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
    Ok(if_addrs::get_if_addrs()?
        .iter()
        .any(|i| !i.ip().is_loopback() && allowed(i.ip(), &config.home_networks)))
}
pub(super) fn network_available(_root: &Path, _config: &Config) -> Result<bool> {
    Ok(true)
}
pub(super) fn server_addresses(
    root: &Path,
    peer: &ServerCard,
    secret: Option<&Credential>,
    presence: &Presence,
    config: &Config,
) -> Result<Vec<String>> {
    let Some(server) = &config.server else {
        return Ok(Vec::new());
    };
    if server.node_id != peer.node.node_id {
        return Err(invalid("SYNC_NOT_A_SERVER"));
    }
    let mut urls: Vec<String> = presence
        .discovered
        .iter()
        .filter(|a| allowed(a.ip(), &config.home_networks))
        .map(|a| format!("http://{a}"))
        .collect();
    urls.extend(server.urls.clone());
    if let Some(secret) = secret {
        urls.extend(secret.urls.clone());
    }
    let enabled = crate::core::net::settings(root)?.mode != crate::core::net::Mode::Off;
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for text in urls {
        let url = base_url(&text)?;
        if crate::core::net::is_tailnet_address(url.host_str().unwrap_or_default()) && !enabled {
            continue;
        }
        let text = base(url.as_str());
        if seen.insert(text.clone()) {
            result.push(text);
        }
    }
    Ok(result)
}
pub(super) fn server_client(
    root: &Path,
    address: &str,
    connect: Duration,
    timeout: Option<Duration>,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect);
    let url = base_url(address)?;
    let host = url.host_str().unwrap_or_default();
    let authority = format!("{host}:{}", url.port_or_known_default().unwrap_or(7420));
    if let Some(proxy) = crate::core::net::proxy_for(root, &authority)? {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| invalid(e.to_string()))?);
    }
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder.build().map_err(|e| invalid(e.to_string()))
}
pub(super) async fn runner_online(root: &Path, peer: &ServerCard) -> Result<bool> {
    let config = config(root)?;
    if !config
        .target
        .iter()
        .any(|p| p.node.node_id == peer.node.node_id)
    {
        return Ok(false);
    }
    let states: BTreeMap<String, Presence> = load(&root.join("sync/presence.yaml"))?;
    Ok(states.get(&peer.node.node_id).is_some_and(|p| {
        p.bye.is_none()
            && p.last_seen.is_some_and(|seen| {
                (Utc::now() - seen).num_seconds() < config.interval.saturating_mul(3) as i64
            })
    }))
}
pub(super) async fn probes(root: &Path) -> Result<Vec<Status>> {
    if Journal::open(root)?.node.role == NodeRole::Server {
        return super::hub::node_statuses(root);
    }
    let config = config(root)?;
    let Some(peer) = config.target.as_ref() else {
        return Ok(Vec::new());
    };
    let result = super::sync::server_nodes(root).await;
    let (online, mut statuses, message) = match result {
        Ok(nodes) => (true, nodes, String::new()),
        Err(e) => (false, Vec::new(), e.to_string()),
    };
    if !online {
        let states: BTreeMap<String, Presence> = load(&root.join("sync/presence.yaml"))?;
        let state = states.get(&peer.node.node_id).cloned().unwrap_or_default();
        statuses.push(Status {
            node: peer.node.node_id.clone(),
            name: peer.node.name.clone(),
            role: NodeRole::Server,
            status: "offline".into(),
            code: "NODE_UNREACHABLE".into(),
            path: "none".into(),
            message,
            since: None,
            last_seen: state.last_seen,
            cursor: 0,
            lag: 0,
        });
    }
    Ok(statuses)
}
pub(super) fn known_nodes(root: &Path) -> Result<Vec<NodeIdentity>> {
    let local = Journal::open(root)?.node;
    let statuses = if local.role == NodeRole::Server {
        super::hub::node_statuses(root)?
    } else {
        fs::read_to_string(root.join("sync/nodes.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    };
    let mut nodes: Vec<_> = statuses
        .into_iter()
        .map(|s: Status| {
            let mut node = local.clone();
            node.node_id = s.node;
            node.name = s.name;
            node.role = s.role;
            node
        })
        .collect();
    if let Some(server) = config(root)?.target {
        if !nodes.iter().any(|n| n.node_id == server.node.node_id) {
            nodes.push(server.node);
        }
    }
    Ok(nodes)
}
pub(super) fn routes(root: PathBuf) -> Router {
    Router::new()
        .route("/node", get(node))
        .route("/nodes", get(nodes))
        .with_state(root)
}
async fn node(State(root): State<PathBuf>) -> Response {
    match Journal::open(&root) {
        Ok(journal) if journal.node.role == NodeRole::Server => Json(json!({
            "node_id":journal.node.node_id,"name":journal.node.name,"role":journal.node.role,
            "store_id":journal.node.store_id,"proto":PROTO,"min_proto":MIN_PROTO,
            "hlc":[Utc::now().timestamp_millis(),0,journal.node.node_id]
        }))
        .into_response(),
        Ok(_) => super::hub::not_server(),
        Err(e) => super::sync::failure(e),
    }
}
async fn nodes(State(root): State<PathBuf>) -> Response {
    match probes(&root).await {
        Ok(nodes) => Json(nodes).into_response(),
        Err(e) => super::sync::failure(e),
    }
}
pub(super) async fn notify(root: &Path, reason: Option<&str>) -> Result<()> {
    if Journal::open(root)?.node.role == NodeRole::Server {
        return Ok(());
    }
    super::sync::notify_server(root, reason).await
}
async fn rediscovered(root: &Path, discovered: Discovered) -> Result<()> {
    let config = config(root)?;
    if config
        .server
        .as_ref()
        .is_none_or(|s| s.node_id != discovered.node_id)
    {
        return Ok(());
    }
    update_presence(root, &discovered.node_id, |p| {
        p.discovered = discovered.addresses
    })?;
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
    if role != "server" || addresses.is_empty() || info.get_port() == 0 {
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
            ("proto", "3"),
        ];
        if node.role == NodeRole::Server && !ips.is_empty() {
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
            eprintln!("mDNS unavailable; using static server: {e}");
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
    if options.purge_relay_refs {
        purge_relay_refs(store, options.dry_run)?;
        return Ok(true);
    }
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

fn purge_relay_refs(store: &ProjectStore, dry_run: bool) -> Result<()> {
    let node = Journal::open(store.root())?.node.node_id;
    for project in store.list()? {
        let work = &project.work_path;
        if !work.is_dir() {
            continue;
        }
        let origin = std::process::Command::new("git")
            .arg("-C")
            .arg(work)
            .args(["remote", "get-url", "origin"])
            .output()?;
        if !origin.status.success() {
            println!("{} has no origin; skipped", project.id);
            continue;
        }
        let mut list = std::process::Command::new("git");
        let output = list
            .arg("-C")
            .arg(work)
            .args([
                "ls-remote",
                "--refs",
                "origin",
                &format!("refs/kanban/sync/{node}"),
                &format!("refs/kanban/logs/{node}/*"),
            ])
            .output()?;
        if !output.status.success() {
            return Err(invalid(format!(
                "{}: {}",
                project.id,
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let refs: Vec<_> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1))
            .map(str::to_owned)
            .collect();
        if refs.is_empty() {
            continue;
        }
        for name in &refs {
            println!(
                "{} origin {} {name}",
                project.id,
                if dry_run { "would delete" } else { "deleting" }
            );
        }
        if !dry_run {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(work)
                .args(["push", "origin", "--delete"])
                .args(&refs)
                .output()?;
            if !output.status.success() {
                return Err(invalid(String::from_utf8_lossy(&output.stderr).to_string()));
            }
        }
    }
    Ok(())
}
