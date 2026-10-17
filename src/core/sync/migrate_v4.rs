//! Restartable format-3 peer configuration cutover. FORMAT is the commit.
use super::*;
use crate::core::storage::{BoardGuard, Storage};

fn read_yaml(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(serde_yaml_ng::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
        Err(e) => Err(e.into()),
    }
}
fn preserve(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        atomic_write_text_raw(path, text)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
fn urls(peer: &Value, secret: Option<&Value>) -> Vec<String> {
    let mut urls = Vec::new();
    for value in std::iter::once(peer).chain(secret) {
        for field in ["address", "tailscale_address"] {
            if let Some(address) = value[field].as_str() {
                add_url(&mut urls, address);
            }
        }
        for field in ["addresses", "urls"] {
            for address in value[field]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                add_url(&mut urls, address);
            }
        }
    }
    urls
}
fn add_url(urls: &mut Vec<String>, address: &str) {
    if address.is_empty() {
        return;
    }
    let address = if address.contains("://") {
        address.to_owned()
    } else {
        format!("http://{address}")
    };
    if !urls.contains(&address) {
        urls.push(address);
    }
}
pub(super) fn migrate(store: &Path) -> Result<()> {
    let _store = lock_file_reentrant(&store.join(".lock"))?;
    let mut boards = Vec::new();
    if store.join("projects").is_dir() {
        for entry in fs::read_dir(store.join("projects"))? {
            let path = entry?.path();
            if path.is_dir() {
                boards.push(path);
            }
        }
    }
    boards.sort();
    let _boards: Vec<BoardGuard> = boards
        .iter()
        .map(|p| Storage::new(p).lock())
        .collect::<Result<_>>()?;
    let root = store.join("sync");
    let _sync = lock_file_reentrant(&root.join(".lock"))?;
    if format::read_format(store)?.is_some_and(|v| v >= STORE_FORMAT) {
        return Ok(());
    }
    let path = store.join("config.yaml");
    let mut config = read_yaml(&path)?;
    if config.is_null() {
        config = serde_json::json!({});
    }
    if !config.is_object() {
        return Err(invalid("global config is not a mapping"));
    }
    if config.get("sync").is_none_or(Value::is_null) {
        config["sync"] = serde_json::json!({});
    }
    let legacy = root.join("legacy-v3");
    let old_peers = config["sync"].get("peers").cloned();
    if let Some(peers) = &old_peers {
        preserve(
            &legacy.join("peers.yaml"),
            &serde_yaml_ng::to_string(peers)?,
        )?;
    }
    let secret_path = root.join("peers.yaml");
    if secret_path.exists() {
        preserve(
            &legacy.join("credentials.yaml"),
            &fs::read_to_string(&secret_path)?,
        )?;
    }
    // Use preserved inputs after a crash that committed config but not FORMAT.
    let peers = old_peers.unwrap_or(read_yaml(&legacy.join("peers.yaml"))?);
    let secrets = if secret_path.exists() {
        read_yaml(&secret_path)?
    } else {
        read_yaml(&legacy.join("credentials.yaml"))?
    };
    let peers = peers.as_array().cloned().unwrap_or_default();
    let secrets = secrets.as_array().cloned().unwrap_or_default();
    let node = read_yaml(&root.join("node.yaml"))?;
    let server = config
        .pointer("/node/role")
        .or_else(|| node.get("role"))
        .and_then(Value::as_str)
        == Some("server");
    let servers: Vec<_> = peers.iter().filter(|p| p["role"] == "server").collect();
    if !server && servers.len() > 1 {
        return Err(invalid(
            "multiple legacy servers; choose one before migration",
        ));
    }
    if server {
        fs::create_dir_all(root.join("hub"))?;
        let mut clients: BTreeMap<String, Value> = load(&root.join("hub/clients.json"))?;
        for peer in &peers {
            let Some(id) = peer["node_id"].as_str() else {
                continue;
            };
            let secret = secrets.iter().find(|s| s["node_id"].as_str() == Some(id));
            let client = clients.entry(id.into()).or_insert_with(
                || serde_json::json!({"cursor":0,"last_seen":chrono::Utc::now().to_rfc3339()}),
            );
            client["name"] = peer["name"].clone();
            client["role"] = peer["role"].clone();
            if let Some(hash) = secret.and_then(|s| s["inbound_sha256"].as_str()) {
                client["token_sha256"] = Value::from(hash);
            }
        }
        atomic_write_text_raw(&root.join("hub/clients.json"), &json(&clients)?)?;
    } else if config["sync"].get("server").is_none_or(Value::is_null) {
        if let Some(peer) = servers.first() {
            let id = peer["node_id"]
                .as_str()
                .ok_or_else(|| invalid("legacy server has no node_id"))?;
            let secret = secrets.iter().find(|s| s["node_id"].as_str() == Some(id));
            let addresses = urls(peer, secret);
            config["sync"]["server"] =
                serde_json::json!({"node_id":id,"name":peer["name"],"urls":addresses});
            let token = secret.and_then(|s| s["token"].as_str()).unwrap_or("");
            preserve(
                &root.join("server.yaml"),
                &serde_yaml_ng::to_string(
                    &serde_json::json!({"node_id":id,"token":token,"urls":addresses}),
                )?,
            )?;
        } else if !peers.is_empty() {
            eprintln!("Sync migration: подключите сервер: docs/sync-server.md");
        }
    }
    let had_relay = config["sync"]
        .as_object()
        .is_some_and(|s| s.keys().any(|k| k.starts_with("relay_")))
        || root.join("relay").exists();
    let sync = config["sync"]
        .as_object_mut()
        .ok_or_else(|| invalid("sync config is not a mapping"))?;
    sync.retain(|key, _| key != "peers" && !key.starts_with("relay_"));
    atomic_write_text_raw(&path, &serde_yaml_ng::to_string(&config)?)?;
    if !server && !root.join("cursor.json").exists() {
        atomic_write_text_raw(
            &root.join("cursor.json"),
            &json(
                &serde_json::json!({"server_node_id":config.pointer("/sync/server/node_id").and_then(Value::as_str).unwrap_or(""),"store_id":node["store_id"].as_str().unwrap_or(""),"hub_epoch":"","last_seq":0,"pushed":0,"migrating":true}),
            )?,
        )?;
    }
    for name in [
        "peers.yaml",
        "exchanges.yaml",
        "relay-ack.json",
        "relay-age.key",
    ] {
        match fs::remove_file(root.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if root.join("relay").exists() {
        fs::remove_dir_all(root.join("relay"))?;
    }
    if server {
        let hub = hub::Hub::init_if_missing(store)?;
        if !node.is_null() {
            let node: NodeIdentity =
                serde_json::from_value(node).map_err(|e| invalid(&e.to_string()))?;
            let journal = Journal {
                root: root.clone(),
                node,
            };
            journal.recover()?;
            hub.ingest_journal(&journal)?;
        }
    }
    if had_relay {
        eprintln!(
            "Sync migration: remove published relay refs with kanban serve --purge-relay-refs; see docs/sync-server.md"
        );
    }
    format::write_format(store, STORE_FORMAT)
}
