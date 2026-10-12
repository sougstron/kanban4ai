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
    /// Pair with a LAN node at IP:port.
    #[arg(long, requires = "code", conflicts_with_all = ["issue_token", "revoke_token", "phone_qr"])]
    pair: Option<SocketAddr>,
    /// Revoke a token by its SHA-256 fingerprint.
    #[arg(long, conflicts_with_all = ["issue_token", "phone_qr"])]
    revoke_token: Option<String>,
    /// Print a single-use pairing QR for an outbound mobile node.
    #[arg(long, conflicts_with = "issue_token")]
    phone_qr: Option<SocketAddr>,
    /// Single-use code displayed on the other node.
    #[arg(long, requires = "pair")]
    code: Option<String>,
    /// This node's reachable LAN IP:port.
    #[arg(long, requires = "pair")]
    address: Option<SocketAddr>,
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
    #[serde(default = "outbound_address")]
    address: SocketAddr,
    empty: bool,
    proto: u32,
    min_proto: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay_recipient: Option<String>,
}

fn outbound_address() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
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
    reverse_token: String,
}
#[derive(Serialize, Deserialize)]
struct SecretPeer {
    node_id: String,
    token: String,
    inbound_sha256: String,
    #[serde(default)]
    address: Option<SocketAddr>,
    #[serde(default)]
    addresses: Vec<SocketAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay_recipient: Option<String>,
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
fn secret() -> Result<String> {
    let mut bytes = [0; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}
fn card(store: &ProjectStore, address: SocketAddr) -> Result<Card> {
    Ok(Card {
        node: Journal::open(store.root())?.node,
        address,
        empty: store.list()?.is_empty(),
        proto: PROTO,
        min_proto: MIN_PROTO,
        relay_recipient: crate::core::sync::relay::recipient(store.root())?,
    })
}
fn validate(local: &Card, remote: &Card) -> Result<()> {
    if !remote.node.outbound_only()
        && (!allowed(remote.address.ip(), &[]) || remote.address.port() == 0)
    {
        return Err(invalid("Pairing requires a LAN address"));
    }
    if remote.proto < MIN_PROTO || remote.min_proto > PROTO {
        return Err(invalid("NODE_VERSION_MISMATCH"));
    }
    if local.node.node_id == remote.node.node_id
        || (local.node.role == NodeRole::Server && remote.node.role == NodeRole::Server)
        || local.node.name == remote.node.name
    {
        return Err(invalid(
            "Node ID and name must differ; server role must be unique",
        ));
    }
    if !local.empty && !remote.empty && local.node.store_id != remote.node.store_id {
        return Err(invalid("SYNC_STORE_MISMATCH"));
    }
    Ok(())
}
// Caller holds store lock, then sync lock. Raw writes keep machine-local data
// out of the replicated journal; config contains no bearer secrets.
fn install(store: &ProjectStore, peer: &Card, inbound: &str, outbound: &str) -> Result<()> {
    let root = store.root();
    let mut peer = peer.clone();
    if peer.node.role == NodeRole::Mobile {
        peer.node.dial = crate::core::sync::NodeDial::Outbound;
    }
    if [inbound, outbound]
        .iter()
        .any(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid("Invalid peer credential"));
    }
    let mut secrets: Vec<SecretPeer> = load(&root.join("sync/peers.yaml"))?;
    let mut tokens: Tokens = load(&root.join("sync/tokens.yaml"))?;
    if let Some(old) = secrets.iter().find(|p| p.node_id == peer.node.node_id) {
        tokens.tokens.retain(|t| t.sha256 != old.inbound_sha256);
    }
    secrets.retain(|p| p.node_id != peer.node.node_id);
    let hash = token_hash(inbound);
    tokens.tokens.push(Token {
        kind: TokenKind::Node,
        sha256: hash.clone(),
    });
    secrets.push(SecretPeer {
        node_id: peer.node.node_id.clone(),
        token: outbound.into(),
        inbound_sha256: hash,
        address: Some(peer.address),
        addresses: Vec::new(),
        relay_recipient: peer.relay_recipient.clone(),
    });
    let mut config: serde_yaml_ng::Value = load(&root.join("config.yaml"))?;
    if config.is_null() {
        config = serde_yaml_ng::Value::Mapping(Default::default());
    }
    if !config.is_mapping() {
        return Err(invalid("Global config must be a mapping"));
    }
    if config["sync"].is_null() {
        config["sync"] = serde_yaml_ng::Value::Mapping(Default::default());
    }
    if !config["sync"].is_mapping() {
        return Err(invalid("sync config must be a mapping"));
    }
    let mut peers: Vec<Card> = if config["sync"]["peers"].is_null() {
        Vec::new()
    } else {
        serde_yaml_ng::from_value(config["sync"]["peers"].clone())?
    };
    if peers.iter().any(|p| {
        p.node.node_id != peer.node.node_id
            && ((p.node.role == NodeRole::Server && peer.node.role == NodeRole::Server)
                || p.node.name == peer.node.name)
    }) {
        return Err(invalid("Peer name and server role must be unique"));
    }
    peers.retain(|p| p.node.node_id != peer.node.node_id);
    peers.push(peer.clone());
    config["sync"]["peers"] = serde_yaml_ng::to_value(peers)?;
    save(&root.join("sync/peers.yaml"), &secrets)?;
    save(&root.join("config.yaml"), &config)?;
    save(&root.join("sync/tokens.yaml"), &tokens)
}

pub(super) async fn accept(
    State(root): State<PathBuf>,
    Json(request): Json<PairRequest>,
) -> Response {
    match tokio::task::spawn_blocking(move || accept_sync(&root, request)).await {
        Ok(Ok(reply)) => Json(reply).into_response(),
        Ok(Err(e)) => {
            if !matches!(e, KanbanError::Invalid(_)) {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    "Cannot persist pairing",
                );
            }
            let message = e.to_string();
            let code = if message.contains("SYNC_STORE_MISMATCH") {
                "SYNC_STORE_MISMATCH"
            } else if message.contains("NODE_VERSION_MISMATCH") {
                "NODE_VERSION_MISMATCH"
            } else {
                "NODE_AUTH_FAILED"
            };
            let status = if code == "NODE_AUTH_FAILED" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            };
            error(status, code, &message)
        }
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "Pairing failed",
        ),
    }
}
fn accept_sync(root: &Path, request: PairRequest) -> Result<PairReply> {
    let store = ProjectStore::at(root);
    let node = Journal::open(root)?.node;
    let _store_lock = lock_file_reentrant(&root.join(".lock"))?;
    let _sync_lock = lock_file_reentrant(&root.join("sync/.lock"))?;
    let path = root.join("sync/pair-code.yaml");
    let mut code: Code = serde_yaml_ng::from_str(&fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            invalid("No active pairing code")
        } else {
            e.into()
        }
    })?)?;
    if code.expires <= chrono::Utc::now().timestamp() || code.attempts >= code.limit {
        return Err(invalid("Pairing code expired or exhausted"));
    }
    code.attempts += 1;
    save(&path, &code)?;
    if request.code.len() != 6
        || !bool::from(
            code.hash
                .as_bytes()
                .ct_eq(token_hash(&request.code).as_bytes()),
        )
    {
        return Err(invalid("Incorrect pairing code"));
    }
    let global = store.load_global_config()?;
    let settings: Settings = global
        .extras
        .get("serve")
        .cloned()
        .map(serde_yaml_ng::from_value)
        .transpose()?
        .unwrap_or_default();
    let port = settings.port;
    let local = Card {
        node,
        address: addresses(&[], port)?
            .into_iter()
            .find(|a| !a.ip().is_loopback())
            .unwrap_or(SocketAddr::from(([127, 0, 0, 1], port))),
        empty: store.list()?.is_empty(),
        proto: PROTO,
        min_proto: MIN_PROTO,
        relay_recipient: crate::core::sync::relay::recipient(root)?,
    };
    validate(&local, &request.node_card)?;
    // Consume before generating credentials: failures never make a used code reusable.
    code.expires = 0;
    save(&path, &code)?;
    let token = secret()?;
    let reverse_token = secret()?;
    install(&store, &request.node_card, &token, &reverse_token)?;
    Ok(PairReply {
        node_card: local,
        token,
        reverse_token,
    })
}

fn create_code(store: &ProjectStore) -> Result<String> {
    let root = store.root();
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

pub(super) fn run(store: &ProjectStore, options: &PairingOptions) -> Result<bool> {
    let root = store.root();
    if options.pair_code {
        println!("{}", create_code(store)?);
    } else if let Some(hash) = &options.revoke_token {
        let _guard = lock_file_reentrant(&root.join("sync/.lock"))?;
        let mut tokens: Tokens = load(&root.join("sync/tokens.yaml"))?;
        tokens.tokens.retain(|t| &t.sha256 != hash);
        save(&root.join("sync/tokens.yaml"), &tokens)?;
        println!("Token revoked");
    } else if let Some(address) = options.phone_qr {
        if !allowed(address.ip(), &[]) || address.port() == 0 {
            return Err(invalid("QR requires a LAN address"));
        }
        let node = Journal::open(root)?.node;
        let code = create_code(store)?;
        let payload = json!({"address":address.ip().to_string(),"port":address.port(),"node_id":node.node_id,"code":code,"role":"mobile","dial":"outbound"}).to_string();
        let qr = qrcode::QrCode::new(payload.as_bytes()).map_err(|e| invalid(e.to_string()))?;
        println!(
            "{payload}\n{}",
            qr.render::<qrcode::render::unicode::Dense1x2>().build()
        );
    } else if let Some(address) = options.pair {
        if !allowed(address.ip(), &[]) || address.port() == 0 {
            return Err(invalid("Pairing requires a LAN address"));
        }
        let node = Journal::open(root)?.node;
        let local = card(store, options.address.unwrap_or_else(outbound_address))?;
        if !node.outbound_only() && options.address.is_none() {
            return Err(invalid("--address required for an inbound node"));
        }
        let request = PairRequest {
            code: options
                .code
                .clone()
                .ok_or_else(|| invalid("--code required"))?,
            node_card: local.clone(),
        };
        let mut child = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--fail-with-body",
                "--noproxy",
                "*",
                "--connect-timeout",
                "5",
                "--max-time",
                "15",
                "-H",
                "Content-Type: application/json",
                "--data-binary",
                "@-",
            ])
            .arg(format!("http://{address}/pair"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or_else(|| invalid("curl stdin missing"))?
            .write_all(&serde_json::to_vec(&request).map_err(|e| invalid(e.to_string()))?)?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(invalid(format!(
                "Pair failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let mut reply: PairReply =
            serde_json::from_slice(&output.stdout).map_err(|e| invalid(e.to_string()))?;
        reply.node_card.address = address;
        validate(&local, &reply.node_card)?;
        let _store_lock = lock_file_reentrant(&root.join(".lock"))?;
        let _sync_lock = lock_file_reentrant(&root.join("sync/.lock"))?;
        install(store, &reply.node_card, &reply.reverse_token, &reply.token)?;
        println!("Paired with {}", reply.node_card.node.name);
    } else {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(root: &Path, code: &str) -> PairRequest {
        let mut node = Journal::open(root).unwrap().node;
        node.node_id = "remote-node".into();
        node.name = "server".into();
        node.role = crate::core::sync::NodeRole::Server;
        PairRequest {
            code: code.into(),
            node_card: Card {
                node,
                address: "127.0.0.1:7421".parse().unwrap(),
                empty: true,
                proto: PROTO,
                min_proto: MIN_PROTO,
                relay_recipient: None,
            },
        }
    }
    fn set_code(root: &Path, expires: i64) {
        save(
            &root.join("sync/pair-code.yaml"),
            &Code {
                hash: token_hash("123456"),
                expires,
                attempts: 0,
                limit: 5,
            },
        )
        .unwrap();
    }
    #[test]
    fn code_is_single_use_and_both_directions_are_registered() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path(), "123456");
        set_code(dir.path(), chrono::Utc::now().timestamp() + 300);
        let reply = accept_sync(dir.path(), req).unwrap();
        let tokens: Tokens = load(&dir.path().join("sync/tokens.yaml")).unwrap();
        assert_eq!(tokens.tokens[0].sha256, token_hash(&reply.token));
        let peers: Vec<SecretPeer> = load(&dir.path().join("sync/peers.yaml")).unwrap();
        assert_eq!(peers[0].token, reply.reverse_token);
        assert!(accept_sync(dir.path(), request(dir.path(), "123456")).is_err());
    }
    #[test]
    fn fifth_failure_exhausts_code_and_expiry_denies_correct_code() {
        let dir = tempfile::tempdir().unwrap();
        let wrong = request(dir.path(), "000000");
        set_code(dir.path(), chrono::Utc::now().timestamp() + 300);
        for _ in 0..5 {
            assert!(
                accept_sync(
                    dir.path(),
                    PairRequest {
                        code: wrong.code.clone(),
                        node_card: wrong.node_card.clone()
                    }
                )
                .is_err()
            );
        }
        assert!(accept_sync(dir.path(), request(dir.path(), "123456")).is_err());
        set_code(dir.path(), chrono::Utc::now().timestamp() - 1);
        assert!(accept_sync(dir.path(), request(dir.path(), "123456")).is_err());
        assert!(!dir.path().join("sync/tokens.yaml").exists());
    }
    #[test]
    fn nonempty_stores_and_duplicate_servers_cannot_pair() {
        let dir = tempfile::tempdir().unwrap();
        let local = card(
            &ProjectStore::at(dir.path()),
            "127.0.0.1:7420".parse().unwrap(),
        )
        .unwrap();
        let mut remote = request(dir.path(), "123456").node_card;
        let mut populated = local.clone();
        populated.empty = false;
        remote.empty = false;
        remote.node.store_id = "different-store".into();
        assert!(
            validate(&populated, &remote)
                .unwrap_err()
                .to_string()
                .contains("SYNC_STORE_MISMATCH")
        );
        remote.empty = true;
        remote.node.role = NodeRole::Laptop;
        validate(&local, &remote).unwrap();
        let mut server = local.clone();
        server.node.role = NodeRole::Server;
        remote.node.role = NodeRole::Server;
        assert!(validate(&server, &remote).is_err());
    }
    #[test]
    fn multiple_outbound_mobiles_pair_without_reachable_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::at(dir.path());
        let local = card(&store, "127.0.0.1:7420".parse().unwrap()).unwrap();
        for name in ["phone-one", "phone-two"] {
            let mut remote = request(dir.path(), "123456").node_card;
            remote.node.node_id = name.into();
            remote.node.name = name.into();
            remote.node.role = NodeRole::Mobile;
            remote.node.dial = crate::core::sync::NodeDial::Outbound;
            remote.address = "127.0.0.1:0".parse().unwrap();
            validate(&local, &remote).unwrap();
            install(&store, &remote, &secret().unwrap(), &secret().unwrap()).unwrap();
        }
        let peers = discovery::config(dir.path()).unwrap().peers;
        assert_eq!(
            peers
                .iter()
                .map(|p| p.node.name.as_str())
                .collect::<Vec<_>>(),
            ["phone-one", "phone-two"]
        );
    }
}
