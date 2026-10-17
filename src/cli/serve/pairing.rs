use super::*;
use crate::core::sync::NodeRole;
use crate::core::sync::{Journal, NodeIdentity};
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(clap::Args, Default)]
#[group(id = "pairing_action", multiple = true)]
pub struct PairingOptions {
    /// Create a single-use pairing code (serve may already be running).
    #[arg(long, conflicts_with_all = ["issue_token", "pair", "revoke_token", "phone_qr"])]
    pair_code: bool,
    /// Pair this client with the sync server at host:port or an https URL.
    #[arg(long, requires = "code", conflicts_with_all = ["issue_token", "revoke_token", "phone_qr"])]
    pair: Option<String>,
    /// Revoke a token by its SHA-256 fingerprint.
    #[arg(long, conflicts_with_all = ["issue_token", "phone_qr"])]
    revoke_token: Option<String>,
    /// Print a single-use phone invite and its QR; HOST:PORT defaults to
    /// this node's LAN address, else its MagicDNS name.
    #[arg(long, conflicts_with = "issue_token", num_args = 0..=1, default_missing_value = "", value_name = "HOST:PORT")]
    phone_qr: Option<String>,
    /// Single-use code displayed on the other node.
    #[arg(long, requires = "pair")]
    code: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Code {
    hash: String,
    expires: i64,
    attempts: u32,
    limit: u32,
}
#[derive(Serialize, Deserialize, Clone)]
struct Card {
    #[serde(flatten)]
    node: NodeIdentity,
    empty: bool,
    proto: u32,
    min_proto: u32,
}

#[derive(Serialize, Deserialize)]
pub(super) struct PairRequest {
    code: String,
    node_card: Card,
}
#[derive(Serialize, Deserialize)]
struct PairReply {
    node_card: Card,
    token: String,
}
#[derive(Deserialize)]
#[serde(default)]
struct PairingSettings {
    code_ttl_seconds: i64,
    code_max_attempts: u32,
}
impl Default for PairingSettings {
    fn default() -> Self {
        Self {
            code_ttl_seconds: 300,
            code_max_attempts: 5,
        }
    }
}

fn load<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(serde_yaml_ng::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}
fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write_text_raw(path, &serde_yaml_ng::to_string(value)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
fn endpoint(_root: &Path, address: &str) -> Result<reqwest::Url> { discovery::base_url(address) }
fn tailnet_address(root: &Path, port: u16) -> Result<Option<String>> {
    if port == 0 || crate::core::net::settings(root)?.mode == crate::core::net::Mode::Off { return Ok(None); }
    let Ok(status) = crate::core::net::status(root) else { return Ok(None); };
    Ok(status["Self"]["DNSName"].as_str().map(|s| format!("{}:{port}",s.trim_end_matches('.'))))
}
fn card(store: &ProjectStore) -> Result<Card> {
    Ok(Card { node: Journal::open(store.root())?.node, empty: store.list()?.is_empty(), proto: PROTO, min_proto: MIN_PROTO })
}
fn validate(local: &Card, remote: &Card) -> Result<()> {
    if remote.proto < MIN_PROTO || remote.min_proto > PROTO { return Err(invalid("SYNC_PROTOCOL_MISMATCH: обновите kanban4ai на всех узлах")); }
    if local.node.node_id == remote.node.node_id || local.node.name == remote.node.name
        || (local.node.role == NodeRole::Server && remote.node.role == NodeRole::Server) {
        return Err(invalid("Node ID and name must differ; server role must be unique"));
    }
    if !local.empty && !remote.empty && local.node.store_id != remote.node.store_id { return Err(invalid("SYNC_STORE_MISMATCH: use join --fresh")); }
    Ok(())
}
pub(super) async fn accept(State(root): State<PathBuf>, headers: axum::http::HeaderMap, Json(request): Json<PairRequest>) -> Response {
    if headers.get("x-kanban-sync-protocol").and_then(|v|v.to_str().ok())!=Some("3"){return error(StatusCode::UPGRADE_REQUIRED,"SYNC_PROTOCOL_MISMATCH","обновите kanban4ai на всех узлах");}
    match tokio::task::spawn_blocking(move || accept_sync(&root, request)).await {
        Ok(Ok(reply)) => Json(reply).into_response(),
        Ok(Err(e)) => super::sync::failure(e),
        Err(e) => super::sync::failure(invalid(e.to_string())),
    }
}
fn accept_sync(root: &Path, request: PairRequest) -> Result<PairReply> {
    let store = ProjectStore::at(root);
    if Journal::open(root)?.node.role != NodeRole::Server { return Err(invalid("SYNC_NOT_A_SERVER")); }
    let local = card(&store)?;
    let _store_lock = store.lock()?;
    let _sync_lock = lock_file_reentrant(&root.join("sync/.lock"))?;
    let path = root.join("sync/pair-code.yaml");
    let mut code: Code = serde_yaml_ng::from_str(&fs::read_to_string(&path).map_err(|_| invalid("NODE_AUTH_FAILED: No active pairing code"))?)?;
    if code.expires <= chrono::Utc::now().timestamp() || code.attempts >= code.limit { return Err(invalid("NODE_AUTH_FAILED: Pairing code expired or exhausted")); }
    code.attempts += 1;
    save(&path, &code)?;
    if request.code.len() != 6 || !bool::from(code.hash.as_bytes().ct_eq(token_hash(&request.code).as_bytes())) {
        return Err(invalid("NODE_AUTH_FAILED: Incorrect pairing code"));
    }
    validate(&local, &request.node_card)?;
    code.expires = 0;
    save(&path, &code)?;
    let token = issue_token(root, TokenKind::Node)?;
    super::hub::register(root, &request.node_card.node, &token_hash(&token))?;
    Ok(PairReply { node_card: local, token })
}

pub(super) fn create_code(store: &ProjectStore) -> Result<String> {
    let root = store.root();
    if Journal::open(root)?.node.role != NodeRole::Server { return Err(invalid("SYNC_NOT_A_SERVER")); }
    let global = store.load_global_config()?;
    let settings: PairingSettings = global
        .extras
        .get("sync")
        .and_then(|v| v.get("pairing"))
        .cloned()
        .map(serde_yaml_ng::from_value)
        .transpose()?
        .unwrap_or_default();
    if !(1..=300).contains(&settings.code_ttl_seconds)
        || !(1..=5).contains(&settings.code_max_attempts)
    {
        return Err(invalid(
            "Pairing TTL must be 1..300 seconds and attempts 1..5",
        ));
    }
    let _guard = lock_file_reentrant(&root.join("sync/.lock"))?;
    let mut bytes = [0; 4];
    let number = loop {
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let n = u32::from_ne_bytes(bytes);
        if n < u32::MAX - u32::MAX % 1_000_000 {
            break n % 1_000_000;
        }
    };
    let code = format!("{number:06}");
    save(
        &root.join("sync/pair-code.yaml"),
        &Code {
            hash: token_hash(&code),
            expires: chrono::Utc::now().timestamp() + settings.code_ttl_seconds,
            attempts: 0,
            limit: settings.code_max_attempts,
        },
    )?;
    Ok(code)
}

pub(super) fn pair(store: &ProjectStore, address: &str, code: String) -> Result<String> {
    let root = store.root();
    let url = endpoint(root, address)?;
    let local = card(store)?;
    if local.node.role == NodeRole::Server { return Err(invalid("A sync server cannot pair as a client")); }
    let request = PairRequest { code, node_card: local.clone() };
    let mut command = Command::new("curl");
    let authority = format!("{}:{}",url.host_str().unwrap_or_default(),url.port_or_known_default().unwrap_or(7420));
    if let Some(proxy) = crate::core::net::proxy_for(root, &authority)? {
        command.args(["--proxy", &proxy, "--noproxy", ""]);
    } else { command.args(["--noproxy", "*"]); }
    let mut child = command.args(["--silent","--show-error","--fail-with-body","--connect-timeout","5","--max-time","15",
        "-H","Content-Type: application/json","-H","x-kanban-sync-protocol: 3","--data-binary","@-"])
        .arg(format!("{}/pair",discovery::base(url.as_str())))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    child.stdin.take().ok_or_else(|| invalid("curl stdin missing"))?.write_all(&serde_json::to_vec(&request).map_err(|e| invalid(e.to_string()))?)?;
    let output = child.wait_with_output()?;
    if !output.status.success() { return Err(invalid(format!("Pair failed: {} {}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr)))); }
    let reply: PairReply = serde_json::from_slice(&output.stdout).map_err(|e| invalid(e.to_string()))?;
    if reply.node_card.node.role != NodeRole::Server { return Err(invalid("SYNC_NOT_A_SERVER")); }
    validate(&local, &reply.node_card)?;
    if reply.token.len() != 64 || !reply.token.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(invalid("Invalid server credential")); }
    {
        let _store = store.lock()?;
        let _sync = lock_file_reentrant(&root.join("sync/.lock"))?;
        let server = discovery::ServerConfig { node_id: reply.node_card.node.node_id.clone(), name: reply.node_card.node.name.clone(), urls: vec![discovery::base(url.as_str())] };
        let credential = discovery::Credential { node_id: server.node_id.clone(), token: reply.token, urls: server.urls.clone() };
        let mut config = store.load_global_config()?;
        let sync = config.extras.entry("sync".into()).or_insert_with(|| serde_yaml_ng::Value::Mapping(Default::default()));
        if !sync.is_mapping() { return Err(invalid("sync must be a mapping")); }
        sync["server"] = serde_yaml_ng::to_value(&server)?;
        save(&root.join("sync/server.yaml"), &credential)?;
        store.save_global_config(&config)?;
    }
    crate::core::sync::activate(root)?;
    super::sync::kick();
    Ok(reply.node_card.node.name)
}

fn serve_port(store: &ProjectStore) -> Result<u16> {
    Ok(store
        .load_global_config()?
        .extras
        .get("serve")
        .cloned()
        .map(serde_yaml_ng::from_value::<Settings>)
        .transpose()?
        .unwrap_or_default()
        .port)
}

/// The address a phone should dial: a LAN IPv4 first, then this node's
/// MagicDNS name, then any tailnet IPv4 serve listens on. LAN addresses on
/// the default-route interface win over other live links; addresses on
/// links that are down (an idle VPN) are only a last resort.
pub(super) fn phone_address(root: &Path) -> Result<String> {
    let port = serve_port(&ProjectStore::at(root))?;
    let networks = discovery::config(root)?.home_networks;
    let defaults =
        default_route_interfaces(&fs::read_to_string("/proc/net/route").unwrap_or_default());
    let mut lan: Vec<(u8, SocketAddr)> = if_addrs::get_if_addrs()?
        .into_iter()
        .filter(|i| i.ip().is_ipv4() && !i.ip().is_loopback() && allowed(i.ip(), &networks))
        .map(|i| {
            let down = fs::read_to_string(format!("/sys/class/net/{}/operstate", i.name))
                .is_ok_and(|state| state.trim() == "down");
            let rank = if defaults.contains(&i.name) {
                0
            } else if down {
                2
            } else {
                1
            };
            (rank, SocketAddr::new(i.ip(), port))
        })
        .collect();
    lan.sort();
    let tailnet = |a: &SocketAddr| crate::core::net::is_tailnet_address(&a.to_string());
    if let Some((_, address)) = lan.iter().find(|(rank, a)| *rank < 2 && !tailnet(a)) {
        return Ok(address.to_string());
    }
    if let Some(address) = tailnet_address(root, port)? {
        return Ok(address);
    }
    lan.first()
        .map(|(_, a)| a.to_string())
        .ok_or_else(|| invalid("No LAN or tailnet address found; pass this node's HOST:PORT"))
}

/// Interfaces carrying a main-table IPv4 default route, lowest metric first,
/// parsed from `/proc/net/route`. Policy-routing tables (VPN tunnels) are not
/// listed there, so this names the physical uplink.
fn default_route_interfaces(table: &str) -> Vec<String> {
    let mut routes: Vec<(u32, String)> = table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let up = u16::from_str_radix(fields.get(3)?, 16).ok()? & 1 == 1;
            (up && fields.get(1)? == &"00000000")
                .then(|| Some((fields.get(6)?.parse().ok()?, fields[0].to_string())))
                .flatten()
        })
        .collect();
    routes.sort();
    routes.into_iter().map(|(_, name)| name).collect()
}

/// A single-use phone invite: `CODE@HOST:PORT` carries everything
/// `kanban4ai join` needs and is short enough to type by hand.
pub(crate) struct PhoneInvite {
    pub code: String,
    /// The `{address, port, node_id, code, role, dial}` JSON payload.
    pub payload: String,
    /// `kanban4ai join CODE@HOST:PORT`, the text the QR encodes.
    pub command: String,
    pub qr: String,
}

pub(crate) fn phone_invite(store: &ProjectStore, address: &str) -> Result<PhoneInvite> {
    let root = store.root();
    let url = endpoint(root, address)?;
    let node = Journal::open(root)?.node;
    let code = create_code(store)?;
    let payload = json!({"address":url.host_str().unwrap().trim_matches(['[', ']']),"port":url.port_or_known_default().unwrap(),"node_id":node.node_id,"code":code,"role":"mobile","dial":"outbound"}).to_string();
    let command = format!("kanban4ai join {code}@{address}");
    let qr = qrcode::QrCode::new(command.as_bytes()).map_err(|e| invalid(e.to_string()))?;
    Ok(PhoneInvite {
        code,
        payload,
        command,
        qr: qr.render::<qrcode::render::unicode::Dense1x2>().build(),
    })
}

/// Split an invite into `(code, host:port)`. A pasted whole command line
/// (`kanban4ai join CODE@HOST:PORT`) is accepted too.
pub(crate) fn parse_invite(text: &str) -> Option<(&str, &str)> {
    let (code, address) = text.split_whitespace().last()?.split_once('@')?;
    (code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) && !address.is_empty())
        .then_some((code, address))
}

/// Phone setup from one invite: become an outbound `mobile` node, bring up
/// managed Tailscale when the invite names a tailnet host, then pair.
/// Returns the peer's name.
pub(crate) fn join(root: &Path, invite: &str, name: Option<&str>) -> Result<String> {
    use crate::core::net::{self, Mode};
    let (code, address) =
        parse_invite(invite).ok_or_else(|| invalid("Expected an invite CODE@HOST:PORT"))?;
    let node = Journal::open(root)?.node;
    if node.role == NodeRole::Server {
        return Err(invalid("The server node cannot join as a phone"));
    }
    if node.role != NodeRole::Mobile || name.is_some() {
        let name = name.unwrap_or(if node.role == NodeRole::Mobile {
            &node.name
        } else {
            "phone"
        });
        settings::save_node(root, name, NodeRole::Mobile)?;
    }
    if net::is_tailnet_address(address) {
        if net::settings(root)?.mode == Mode::Off {
            settings::set_network_mode(root, Mode::Managed)?;
        }
        if net::settings(root)?.mode == Mode::Managed {
            if let Err(e) = net::up(root, None) {
                if !e.to_string().contains("NET_TAILSCALE_MISSING") {
                    return Err(e);
                }
                net::install(root)?;
                net::up(root, None)?;
            }
        }
    }
    pair(&ProjectStore::at(root), address, code.into())
}

pub(super) fn run(store: &ProjectStore, options: &PairingOptions) -> Result<bool> {
    let root = store.root();
    if options.pair_code {
        println!("{}", create_code(store)?);
    } else if let Some(hash) = &options.revoke_token {
        let _guard = lock_file_reentrant(&root.join("sync/.lock"))?;
        let mut tokens: Tokens = load(&root.join("sync/tokens.yaml"))?;
        tokens.tokens.retain(|t| &t.sha256 != hash);
        save(&root.join("sync/tokens.yaml"), &tokens)?;
        super::hub::revoke(root, hash)?;
        println!("Token revoked");
    } else if let Some(address) = &options.phone_qr {
        let address = match address.trim() {
            "" => phone_address(root)?,
            address => address.to_string(),
        };
        let invite = phone_invite(store, &address)?;
        println!(
            "{}\nOn the phone run: {}\n{}",
            invite.payload, invite.command, invite.qr
        );
    } else if let Some(address) = &options.pair {
        let name = pair(
            store,
            address,
            options
                .code
                .clone()
                .ok_or_else(|| invalid("--code required"))?,
        )?;
        println!("Paired with {name}");
    } else {
        return Ok(false);
    }
    Ok(true)
}


#[cfg(test)]
mod tests {
    use super::*;
    fn node(name:&str,role:&str)->tempfile::TempDir {
        let dir=tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.yaml"),format!("node:\n  name: {name}\n  role: {role}\n")).unwrap();
        Journal::open(dir.path()).unwrap();dir
    }
    #[test]
    fn server_code_is_one_use_and_registers_only_an_inbound_node_token() {
        let server=node("server","server");let client=node("laptop","laptop");
        let store=ProjectStore::at(server.path());
        let code=create_code(&store).unwrap();
        let card=card(&ProjectStore::at(client.path())).unwrap();
        let reply=accept_sync(server.path(),PairRequest{code:code.clone(),node_card:card.clone()}).unwrap();
        let value=serde_json::to_value(&reply).unwrap();
        assert!(value.get("reverse_token").is_none());
        assert_eq!(reply.token.len(),64);
        let id=super::super::hub::caller(server.path(),&token_hash(&reply.token)).unwrap();
        assert_eq!(id,card.node.node_id);
        assert!(!server.path().join("sync/server.yaml").exists());
        assert!(!client.path().join("sync/tokens.yaml").exists());
        assert!(accept_sync(server.path(),PairRequest{code,node_card:card}).is_err());
    }
    #[test]
    fn clients_cannot_issue_pair_codes_or_accept_pairing() {
        let client=node("laptop","laptop");
        assert!(create_code(&ProjectStore::at(client.path())).unwrap_err().to_string().contains("SYNC_NOT_A_SERVER"));
        let card=card(&ProjectStore::at(client.path())).unwrap();
        let error=accept_sync(client.path(),PairRequest{code:"123456".into(),node_card:card}).err().unwrap();
        assert!(error.to_string().contains("SYNC_NOT_A_SERVER"));
    }
    #[test]
    fn incorrect_codes_exhaust_the_attempt_limit() {
        let server=node("server","server");let client=node("laptop","laptop");
        let code=create_code(&ProjectStore::at(server.path())).unwrap();
        let bad=if code=="000000"{"111111"}else{"000000"};
        let card=card(&ProjectStore::at(client.path())).unwrap();
        for _ in 0..5 {assert!(accept_sync(server.path(),PairRequest{code:bad.into(),node_card:card.clone()}).is_err());}
        assert!(accept_sync(server.path(),PairRequest{code,node_card:card}).is_err());
    }
    #[test]
    fn invite_accepts_https_and_preserves_the_server_origin() {
        let server=node("server","server");
        let invite=phone_invite(&ProjectStore::at(server.path()),"https://sync.example.test").unwrap();
        assert_eq!(parse_invite(&invite.command).unwrap().1,"https://sync.example.test");
        assert!(!invite.payload.contains("token"));
    }
}
