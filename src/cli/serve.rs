//! LAN-first HTTP entry point with optional paired tailnet fallback.
//! Public probes bypass auth; all other routes remain fail-closed.
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::core::error::{KanbanError, Result};
use crate::core::project::ProjectStore;
use crate::core::storage::{atomic_write_text_raw, lock_file_reentrant};

mod api;
pub(crate) use api::{
    continuation_nodes, continue_on_board, launch_on_board, remote_runner_on_board,
};
mod embedded;
pub(crate) use embedded::Embedded;
mod discovery;
mod git;
mod pairing;
pub(crate) mod settings;
mod sync;
pub use discovery::DiscoveryOptions;
pub use pairing::PairingOptions;

/// Synchronous callers (TUI workers, CLI and daemon) share the HTTP runner path.
/// A dedicated runtime thread also permits calls from axum's blocking workers.
pub(crate) fn runner_action(
    board: &Path,
    task: &str,
    queue: bool,
) -> Result<Option<Option<String>>> {
    let Some(projects) = board
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "projects"))
    else {
        return Ok(None);
    };
    let Some(root) = projects.parent() else {
        return Ok(None);
    };
    if !crate::core::sync::enabled(root) || discovery::config(root)?.peers.is_empty() {
        return Ok(None);
    }
    let storage = crate::core::storage::Storage::new(board);
    let Some(task_state) = storage.load_task(task)? else {
        return Ok(None);
    };
    if api::local_runner(board, task_state.runner.as_ref())? {
        let local = crate::core::sync::Journal::open(root)?.node;
        if task_state
            .runner
            .as_ref()
            .is_some_and(|runner| runner.as_str() != local.name)
        {
            let _guard = storage.lock()?;
            if let Some(mut current) = storage.load_task(task)? {
                current.runner = Some(crate::core::config::Runner::Node(local.name));
                storage.save_task(&current)?;
            }
        }
        return Ok(None);
    }
    let id = board
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("Invalid project"))?;
    let (root, id, task) = (root.to_path_buf(), id.to_owned(), task.to_owned());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(KanbanError::Io)?
            .block_on(api::runner_action(&root, &id, &task, queue))
    })
    .join()
    .map_err(|_| invalid("Runner dispatch worker panicked"))?
}

pub(crate) fn runner_control(board: &Path, task: &str, action: &str) -> Result<bool> {
    crate::core::sync::control::request(board, task, action)
}

pub(crate) fn remote_log(
    board: &Path,
    task: &str,
    node: &str,
) -> Result<crate::core::runner_log::Snapshot> {
    api::remote_log(board, task, node)
}

pub(crate) fn local_runner(
    board: &Path,
    runner: Option<&crate::core::config::Runner>,
) -> Result<bool> {
    api::local_runner(board, runner)
}

const PROTO: u32 = 1;
const MIN_PROTO: u32 = 1;

#[derive(Deserialize)]
struct Settings {
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_shutdown_timeout")]
    shutdown_timeout: u64,
}
fn default_port() -> u16 {
    7420
}
fn default_shutdown_timeout() -> u64 {
    10
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            port: default_port(),
            shutdown_timeout: default_shutdown_timeout(),
        }
    }
}
#[derive(Default, Deserialize)]
struct Networks {
    #[serde(default)]
    home_networks: Vec<IpNet>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TokenKind {
    Node,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Token {
    kind: TokenKind,
    sha256: String,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tokens {
    #[serde(default)]
    tokens: Vec<Token>,
}

fn invalid(message: impl Into<String>) -> KanbanError {
    KanbanError::Invalid(message.into())
}
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 15) as usize] as char);
    }
    text
}
fn token_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn issue_token(root: &Path, kind: TokenKind) -> Result<String> {
    fs::create_dir_all(root.join("sync"))?;
    let _guard = lock_file_reentrant(&root.join("sync/.lock"))?;
    let path = root.join("sync/tokens.yaml");
    let mut tokens = match fs::read_to_string(&path) {
        Ok(text) => serde_yaml_ng::from_str::<Tokens>(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Tokens::default(),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let secret = hex(&bytes);
    tokens.tokens.push(Token {
        kind,
        sha256: token_hash(&secret),
    });
    // The raw writer uses a private temp file and rename; no sync operation
    // is emitted for this machine-local secret registry.
    atomic_write_text_raw(&path, &serde_yaml_ng::to_string(&tokens)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(secret)
}

fn allowed(ip: IpAddr, networks: &[IpNet]) -> bool {
    if ip.is_loopback() {
        return true;
    }
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    if !networks.is_empty() {
        return networks.iter().any(|net| net.contains(&ip));
    }
    match ip {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => ip.is_unique_local(),
    }
}

fn addresses(networks: &[IpNet], port: u16) -> Result<BTreeSet<SocketAddr>> {
    let mut addresses = BTreeSet::new();
    for interface in if_addrs::get_if_addrs()? {
        let ip = interface.ip();
        if !allowed(ip, networks) {
            continue;
        }
        let address = match ip {
            IpAddr::V6(ip) if ip.is_unicast_link_local() => {
                let name = std::ffi::CString::new(interface.name)
                    .map_err(|_| invalid("invalid network interface name"))?;
                // SAFETY: name is a valid NUL-terminated interface name.
                let scope = unsafe { libc::if_nametoindex(name.as_ptr()) };
                if scope == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                SocketAddr::V6(SocketAddrV6::new(ip, port, 0, scope))
            }
            _ => SocketAddr::new(ip, port),
        };
        addresses.insert(address);
    }
    if !addresses.iter().any(|addr| addr.ip().is_loopback()) {
        return Err(invalid("serve requires an active loopback interface"));
    }
    Ok(addresses)
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

async fn authenticate(State(path): State<PathBuf>, mut request: Request, next: Next) -> Response {
    let secret = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(secret) =
        secret.filter(|secret| secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return error(
            StatusCode::UNAUTHORIZED,
            "NODE_AUTH_FAILED",
            "Missing or invalid bearer token",
        );
    };
    let hash = token_hash(secret);
    // Read on every request so revocation is effective without a restart.
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return error(
                StatusCode::UNAUTHORIZED,
                "NODE_AUTH_FAILED",
                "Token is not registered",
            );
        }
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "Cannot read token registry",
            );
        }
    };
    let tokens: Tokens = match serde_yaml_ng::from_str(&text) {
        Ok(tokens) => tokens,
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "Invalid token registry",
            );
        }
    };
    let kind = tokens
        .tokens
        .iter()
        .find(|token| token.sha256.as_bytes().ct_eq(hash.as_bytes()).into())
        .map(|token| token.kind);
    let Some(kind) = kind else {
        return error(
            StatusCode::UNAUTHORIZED,
            "NODE_AUTH_FAILED",
            "Token is unknown or revoked",
        );
    };
    request.extensions_mut().insert(kind);
    request.extensions_mut().insert(hash);
    next.run(request).await
}

fn router(tokens: PathBuf) -> Router {
    let root = tokens
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let protected = Router::new()
        .merge(discovery::routes(root.clone()))
        .merge(sync::routes(root.clone()))
        .merge(git::routes(root.clone()))
        .merge(api::routes(root))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "NOT_FOUND", "Unknown endpoint") })
        .layer(middleware::from_fn_with_state(tokens.clone(), authenticate));
    Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .route(
            "/version",
            get(|| async {
                Json(json!({
                    "app_version": env!("CARGO_PKG_VERSION"), "proto": PROTO, "min_proto": MIN_PROTO
                }))
            }),
        )
        .route(
            "/pair",
            axum::routing::post(pairing::accept).with_state(
                tokens
                    .parent()
                    .and_then(Path::parent)
                    .unwrap_or(Path::new("."))
                    .to_path_buf(),
            ),
        )
        .merge(protected)
}

pub(super) fn shutdown_signal() -> Result<impl Future<Output = Result<()>>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
            Ok(())
        })
    }
    #[cfg(not(unix))]
    Ok(async { tokio::signal::ctrl_c().await.map_err(Into::into) })
}

async fn serve(root: &Path, networks: &[IpNet], port: u16, grace: Duration) -> Result<()> {
    // Install handlers before listeners can advertise readiness.
    let shutdown = shutdown_signal()?;
    tokio::pin!(shutdown);
    let mut net = crate::core::net::Manager::new(root)?;
    if let Err(e) = net.tick() {
        eprintln!("serve network: {e}");
    }
    let mut bind_networks = networks.to_vec();
    if matches!(
        crate::core::net::settings(root)?.mode,
        crate::core::net::Mode::System
    ) {
        bind_networks.extend(
            ["100.64.0.0/10", "fd7a:115c:a1e0::/48"]
                .map(|cidr| cidr.parse::<IpNet>().expect("static tailnet CIDR")),
        );
        // Preserve default private LAN binding when adding explicit tailnet CIDRs.
        if networks.is_empty() {
            bind_networks.extend(
                ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"]
                    .map(|cidr| cidr.parse::<IpNet>().expect("static private CIDR")),
            );
        }
    }
    let app = router(root.join("sync/tokens.yaml"));
    // Bind everything before starting any request handling: a failed bind
    // must not leave a partially started service running.
    let mut listeners = Vec::new();
    let node = crate::core::sync::Journal::open(root)?.node;
    let bind_addresses = if node.outbound_only() {
        BTreeSet::from([SocketAddr::from(([127, 0, 0, 1], port))])
    } else {
        addresses(&bind_networks, port)?
    };
    for address in bind_addresses {
        listeners.push(TcpListener::bind(address).await?);
    }
    let replication = Embedded::serving(root)?;
    let discovery = discovery::start(root, networks, port).await?;
    let (stop, _) = watch::channel(false);
    let mut tasks = JoinSet::new();
    for listener in listeners {
        let mut stopped = stop.subscribe();
        let app = app.clone();
        println!("serve listening on http://{}", listener.local_addr()?);
        tasks.spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = stopped.changed().await;
            })
            .await
        });
    }
    let mut net_tick =
        tokio::time::interval(Duration::from_secs(discovery::config(root)?.interval));
    let result = loop {
        tokio::select! {
            result = &mut shutdown => break result,
            result = tasks.join_next() => break match result {
                Some(Ok(Err(e))) => Err(e.into()),
                Some(Err(e)) => Err(invalid(e.to_string())),
                _ => Err(invalid("HTTP listener stopped unexpectedly")),
            },
            _ = net_tick.tick() => {
                if let Err(e) = net.tick() {
                    eprintln!("serve network: {e}");
                }
            }
        }
    };
    discovery.stop().await;
    // Ends live SSE streams too, so graceful drain does not wait for them.
    drop(replication);
    let _ = stop.send(true);
    let drain = async {
        while let Some(task) = tasks.join_next().await {
            task.map_err(|e| invalid(e.to_string()))??;
        }
        Ok::<(), KanbanError>(())
    };
    match tokio::time::timeout(grace, drain).await {
        Ok(result) => result?,
        Err(_) => {
            eprintln!("serve shutdown grace expired; closing remaining connections");
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
    }
    println!("serve stopped");
    result
}

/// Remove retired client credentials without invalidating paired node tokens.
fn retire_client_tokens(root: &Path) -> Result<()> {
    let path = root.join("sync/tokens.yaml");
    if !path.exists() {
        return Ok(());
    }
    let _lock = lock_file_reentrant(&root.join("sync/.lock"))?;
    let mut registry: serde_yaml_ng::Value = serde_yaml_ng::from_str(&fs::read_to_string(&path)?)?;
    if let Some(tokens) = registry
        .get_mut("tokens")
        .and_then(serde_yaml_ng::Value::as_sequence_mut)
    {
        let before = tokens.len();
        tokens.retain(|token| {
            token.get("kind").and_then(serde_yaml_ng::Value::as_str) != Some("client")
        });
        if tokens.len() != before {
            let validated: Tokens = serde_yaml_ng::from_value(registry)?;
            atomic_write_text_raw(&path, &serde_yaml_ng::to_string(&validated)?)?;
        }
    }
    Ok(())
}

/// `kanban4ai join`: set this node up as a phone from one invite, pair,
/// then pull the shared board once.
/// `fresh` first moves a store that already holds projects aside, so the
/// phone can adopt the server's board instead of failing with
/// `SYNC_STORE_MISMATCH`.
pub fn join(invite: &str, name: Option<&str>, fresh: bool) -> Result<ExitCode> {
    let mut store = ProjectStore::open()?;
    if fresh && !store.list()?.is_empty() {
        let root = store.root().to_path_buf();
        let stem = root
            .file_name()
            .ok_or_else(|| invalid("Store root has no name"))?
            .to_string_lossy();
        let backup = root.with_file_name(format!(
            "{stem}.pre-join-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ));
        fs::rename(&root, &backup)?;
        println!("Previous store moved to {}", backup.display());
        store = ProjectStore::open()?;
    }
    let peer = match pairing::join(store.root(), invite, name) {
        Err(e) if e.to_string().contains("SYNC_STORE_MISMATCH") => {
            return Err(invalid(format!(
                "{e}\nThis store already holds another board. To replace it with the \
                 server's board, rerun with a fresh invite: kanban4ai join --fresh CODE@HOST:PORT \
                 (the current store is moved aside, not deleted)"
            )));
        }
        other => other?,
    };
    println!("Paired with {peer}");
    let results = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(sync::once(store.root(), true))?;
    for (peer, result) in results {
        match result {
            Ok(()) => println!("synced with {peer}"),
            Err(e) => eprintln!("sync {peer}: {e} (the TUI or daemon retries)"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn run(
    port: Option<u16>,
    token_kind: Option<&str>,
    pairing: PairingOptions,
    discovery_options: DiscoveryOptions,
) -> Result<ExitCode> {
    let store = ProjectStore::open()?;
    retire_client_tokens(store.root())?;
    if let Some(kind) = token_kind {
        let secret = issue_token(
            store.root(),
            match kind {
                "node" => TokenKind::Node,
                _ => return Err(invalid("token kind must be node")),
            },
        )?;
        println!("{secret}");
        return Ok(ExitCode::SUCCESS);
    }
    if pairing::run(&store, &pairing)? {
        return Ok(ExitCode::SUCCESS);
    }
    if discovery::run(&store, discovery_options)? {
        return Ok(ExitCode::SUCCESS);
    }
    let global = store.load_global_config()?;
    let settings: Settings = global
        .extras
        .get("serve")
        .cloned()
        .map(serde_yaml_ng::from_value)
        .transpose()?
        .unwrap_or_default();
    let networks: Networks = global
        .extras
        .get("sync")
        .cloned()
        .map(serde_yaml_ng::from_value)
        .transpose()?
        .unwrap_or_default();
    let port = port.unwrap_or(settings.port);
    if port == 0 {
        return Err(invalid("serve.port must be greater than zero"));
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(
            store.root(),
            &networks.home_networks,
            port,
            Duration::from_secs(settings.shutdown_timeout),
        ))?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use tower::ServiceExt;

    #[test]
    fn authentication_is_fail_closed_and_revocation_is_immediate() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sync")).unwrap();
        let path = dir.path().join("sync/tokens.yaml");
        let node = "a".repeat(64);
        let app = router(path.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let request = |path: &str, token: Option<&str>| {
                let mut builder = axum::http::Request::builder().uri(path);
                if let Some(token) = token {
                    builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
                }
                builder.body(Body::empty()).unwrap()
            };
            let response = app.clone().oneshot(request("/health", None)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                json!({"ok": true})
            );
            let response = app
                .clone()
                .oneshot(request("/node", Some(&node)))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            fs::write(
                &path,
                serde_yaml_ng::to_string(&Tokens {
                    tokens: vec![Token {
                        kind: TokenKind::Node,
                        sha256: token_hash(&node),
                    }],
                })
                .unwrap(),
            )
            .unwrap();
            for (path, token, expected) in [
                ("/node", None, StatusCode::UNAUTHORIZED),
                ("/node", Some("short"), StatusCode::UNAUTHORIZED),
                (
                    "/node",
                    Some("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"),
                    StatusCode::UNAUTHORIZED,
                ),
                ("/node", Some(node.as_str()), StatusCode::OK),
                ("/sync/unknown", Some(node.as_str()), StatusCode::NOT_FOUND),
            ] {
                let response = app.clone().oneshot(request(path, token)).await.unwrap();
                assert_eq!(response.status(), expected, "{path}");
            }
            fs::write(&path, "tokens: []\n").unwrap();
            let response = app
                .clone()
                .oneshot(request("/node", Some(&node)))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            fs::write(&path, "invalid yaml: [").unwrap();
            let response = app
                .clone()
                .oneshot(request("/node", Some(&node)))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        });
    }

    #[test]
    fn bind_policy_never_uses_wildcards_or_public_defaults() {
        for address in [
            "127.0.0.1",
            "::1",
            "10.2.3.4",
            "172.16.0.1",
            "192.168.1.1",
            "fd00::1",
        ] {
            assert!(allowed(address.parse().unwrap(), &[]), "{address}");
        }
        for address in [
            "0.0.0.0",
            "::",
            "8.8.8.8",
            "172.15.0.1",
            "172.32.0.1",
            "169.254.1.1",
            "fe80::1",
            "224.0.0.1",
            "2001:4860:4860::8888",
        ] {
            assert!(!allowed(address.parse().unwrap(), &[]), "{address}");
        }
        let networks = [
            "192.168.20.0/24".parse().unwrap(),
            "fd12:3456::/48".parse().unwrap(),
        ];
        assert!(allowed("127.0.0.1".parse().unwrap(), &networks));
        assert!(allowed("::1".parse().unwrap(), &networks));
        assert!(allowed("192.168.20.42".parse().unwrap(), &networks));
        assert!(allowed("fd12:3456::1".parse().unwrap(), &networks));
        assert!(!allowed("192.168.21.42".parse().unwrap(), &networks));
        assert!(!allowed("fd12:3457::1".parse().unwrap(), &networks));
    }
}
