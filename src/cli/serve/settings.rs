//! Store-scoped settings and read-only server/client status.
use super::discovery;
use super::*;
use crate::core::sync::{Journal, NodeDial, NodeRole};
use serde_json::Value;
use std::process::Command;
const UNIT: &str = "kanban4ai-serve.service";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Peer {
    pub id: String,
    pub name: String,
    pub role: NodeRole,
    pub route: String,
    pub code: String,
    pub message: String,
    pub exchange: Option<String>,
    pub pending: u64,
    pub clones: Vec<String>,
    pub cursor: u64,
    pub lag: u64,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Server {
    pub id: String,
    pub name: String,
    pub urls: Vec<String>,
    pub route: String,
    pub code: String,
    pub message: String,
    pub exchange: Option<String>,
    pub pending: u64,
    pub last_seq: u64,
    pub head: u64,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct View {
    pub name: String,
    pub role: NodeRole,
    pub api_enabled: bool,
    pub api_active: Option<bool>,
    pub server: Option<Server>,
    pub nodes: Vec<Peer>,
    pub conflicts: Vec<String>,
    pub local_clones: Vec<String>,
    pub probed: bool,
}
#[derive(Debug, Clone)]
pub(crate) struct BoardPeer {
    pub name: String,
    pub role: NodeRole,
    pub code: String,
    pub exchange: Option<chrono::DateTime<chrono::Utc>>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub pending: u64,
}
fn systemctl(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("systemctl").arg("--user").args(args).output()
}
fn unit_active() -> Option<bool> {
    if cfg!(test) {
        None
    } else {
        systemctl(&["is-active", "--quiet", UNIT])
            .ok()
            .map(|o| o.status.success())
    }
}
fn api_enabled(root: &Path) -> Result<bool> {
    Ok(ProjectStore::at(root)
        .load_global_config()?
        .extras
        .get("serve")
        .and_then(|s| s.get("enabled"))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false))
}
fn time_text(at: chrono::DateTime<chrono::Utc>) -> String {
    at.with_timezone(&chrono::Local)
        .format("%d.%m %H:%M")
        .to_string()
}
pub(super) fn clone_line(value: &Value) -> String {
    format!(
        "{}: {} · {}",
        value["project"].as_str().unwrap_or("—"),
        value["state"].as_str().unwrap_or("—"),
        value["path"].as_str().unwrap_or("—")
    )
}
pub(crate) fn local_view(root: &Path) -> Result<View> {
    let journal = Journal::open(root)?;
    let config = discovery::config(root)?;
    let cursor: Value = fs::read_to_string(root.join("sync/cursor.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let runtime: Value = fs::read_to_string(root.join("sync/server-status.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let own = journal
        .vector()?
        .get(&journal.node.node_id)
        .copied()
        .unwrap_or(0);
    let server = config.server.map(|s| Server {
        id: s.node_id,
        name: s.name,
        urls: s.urls,
        route: runtime["route"].as_str().unwrap_or("not connected").into(),
        code: runtime["code"].as_str().unwrap_or("").into(),
        message: runtime["message"].as_str().unwrap_or("").into(),
        exchange: runtime["at"].as_str().map(str::to_owned),
        pending: own.saturating_sub(cursor["pushed"].as_u64().unwrap_or(0)),
        last_seq: cursor["last_seq"].as_u64().unwrap_or(0),
        head: runtime["head"].as_u64().unwrap_or(0),
    });
    let nodes = if journal.node.role == NodeRole::Server {
        super::hub::node_statuses(root)?
    } else {
        fs::read_to_string(root.join("sync/nodes.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    .into_iter()
    .map(|n: discovery::Status| Peer {
        id: n.node,
        name: n.name,
        role: n.role,
        route: n.path,
        code: n.code,
        message: n.message,
        exchange: n.last_seen.map(time_text),
        pending: n.lag,
        clones: Vec::new(),
        cursor: n.cursor,
        lag: n.lag,
    })
    .collect();
    let local_clones = ProjectStore::at(root)
        .list()?
        .into_iter()
        .map(|p| match crate::core::code::status(root, &p.id) {
            Ok(s) => clone_line(&serde_json::to_value(s).unwrap_or_default()),
            Err(e) => format!("{}: {e}", p.id),
        })
        .collect();
    Ok(View {
        name: journal.node.name.clone(),
        role: journal.node.role,
        api_enabled: api_enabled(root)?,
        api_active: unit_active(),
        server,
        nodes,
        conflicts: journal
            .conflicts()?
            .into_iter()
            .map(|c| format!("{}/{} {}: {}", c.project, c.entity, c.id, c.field))
            .collect(),
        local_clones,
        probed: false,
    })
}
pub(crate) fn refresh(root: &Path) -> Result<View> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let statuses = runtime.block_on(discovery::probes(root))?;
    atomic_write_text_raw(
        &root.join("sync/nodes.json"),
        &serde_json::to_string(&statuses).map_err(|e| invalid(e.to_string()))?,
    )?;
    let mut view = local_view(root)?;
    view.probed = true;
    Ok(view)
}
pub(crate) fn board_peers(root: &Path) -> Result<Vec<BoardPeer>> {
    let journal = Journal::open(root)?;
    let statuses: Vec<discovery::Status> = if journal.node.role == NodeRole::Server {
        super::hub::node_statuses(root)?
    } else {
        fs::read_to_string(root.join("sync/nodes.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    };
    let mut nodes: Vec<_> = statuses
        .into_iter()
        .filter(|n| n.node != journal.node.node_id)
        .map(|n| BoardPeer {
            name: n.name,
            role: n.role,
            code: n.code,
            exchange: n.last_seen,
            since: n.since,
            pending: n.lag,
        })
        .collect();
    if let Some(server) = discovery::config(root)?.server {
        {
            let status: Value = fs::read_to_string(root.join("sync/server-status.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            let cursor: Value = fs::read_to_string(root.join("sync/cursor.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            let own = journal
                .vector()?
                .get(&journal.node.node_id)
                .copied()
                .unwrap_or(0);
            let pending = own.saturating_sub(cursor["pushed"].as_u64().unwrap_or(0));
            if let Some(node) = nodes.iter_mut().find(|n| n.role == NodeRole::Server) {
                node.pending = pending;
            } else {
                nodes.insert(
                    0,
                    BoardPeer {
                        name: server.name,
                        role: NodeRole::Server,
                        code: status["code"].as_str().unwrap_or("OFFLINE").into(),
                        exchange: None,
                        since: None,
                        pending,
                    },
                );
            }
        }
    }
    Ok(nodes)
}
/// Rename this node or change its role; the dial follows the role.
pub(crate) fn save_node(root: &Path, name: &str, role: NodeRole) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(invalid(
            "Node name: lowercase latin letters, digits and dashes",
        ));
    }
    let store = ProjectStore::at(root);
    let node = Journal::open(root)?.node;
    {
        let _guard = store.lock()?;
        let mut config = store.load_global_config()?;
        let dial = if role == NodeRole::Mobile {
            NodeDial::Outbound
        } else {
            NodeDial::Inbound
        };
        config.extras.insert(
            "node".into(),
            serde_yaml_ng::to_value(json!({
                "node_id": node.node_id,
                "name": name,
                "role": role,
                "dial": dial,
            }))?,
        );
        store.save_global_config(&config)?;
    }
    // Re-open so node.yaml picks up the new identity right away.
    Journal::open(root)?;
    Ok(())
}

/// Machine-local mode changes preserve all other network settings and login state.
pub(crate) fn set_network_mode(root: &Path, mode: crate::core::net::Mode) -> Result<()> {
    use crate::core::net::{self, Mode};
    let previous = net::settings(root)?.mode;
    if previous == Mode::Managed && mode != previous {
        net::down(root)?;
    }
    let store = ProjectStore::at(root);
    let _guard = store.lock()?;
    let mut config = store.load_global_config()?;
    let section = config
        .extras
        .entry("net".into())
        .or_insert_with(|| serde_yaml_ng::Value::Mapping(Default::default()));
    let tailscale = section
        .as_mapping_mut()
        .ok_or_else(|| invalid("net must be a mapping"))?
        .entry(serde_yaml_ng::Value::String("tailscale".into()))
        .or_insert_with(|| serde_yaml_ng::Value::Mapping(Default::default()));
    let name = match mode {
        Mode::Off => "off",
        Mode::System => "system",
        Mode::Managed => "managed",
    };
    tailscale
        .as_mapping_mut()
        .ok_or_else(|| invalid("net.tailscale must be a mapping"))?
        .insert(
            serde_yaml_ng::Value::String("mode".into()),
            serde_yaml_ng::Value::String(name.into()),
        );
    store.save_global_config(&config)
}
fn run_systemctl(args: &[&str]) -> Result<()> {
    let output = systemctl(args)?;
    if !output.status.success() {
        return Err(invalid(format!(
            "systemctl --user {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn unit_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('\n', "\\n")
    )
}

/// Install a user unit for this binary and store when no package did.
fn ensure_unit(root: &Path) -> Result<()> {
    if systemctl(&["cat", UNIT]).is_ok_and(|output| output.status.success()) {
        return Ok(());
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| invalid("HOME is not set"))?)
            .join(".config"),
    };
    let path = base.join("systemd/user").join(UNIT);
    fs::create_dir_all(path.parent().expect("unit directory"))?;
    let exe = std::env::current_exe()?;
    let text = format!(
        "[Unit]\nDescription=kanban4ai LAN HTTP server\nAfter=network.target\n\n\
         [Service]\nType=simple\nExecStart={} serve\nEnvironment={}\n\
         Restart=on-failure\nRestartSec=30\nKillSignal=SIGTERM\nTimeoutStopSec=30\nUMask=0077\n\n\
         [Install]\nWantedBy=default.target\n",
        unit_quote(&exe.to_string_lossy()),
        unit_quote(&format!("KANBAN_HOME={}", root.display()))
    );
    atomic_write_text_raw(&path, &text)?;
    run_systemctl(&["daemon-reload"])
}

/// The "API server enabled" switch: enable/disable the user unit now and
/// at login, then record `serve.enabled`.
pub(crate) fn set_api(root: &Path, enabled: bool) -> Result<()> {
    if enabled {
        if Journal::open(root)?.node.outbound_only() {
            return Err(invalid(
                "A phone only dials out; it does not run the API server",
            ));
        }
        ensure_unit(root)?;
        run_systemctl(&["enable", "--now", UNIT])?;
    } else {
        run_systemctl(&["disable", "--now", UNIT])?;
    }
    let store = ProjectStore::at(root);
    let _guard = store.lock()?;
    let mut config = store.load_global_config()?;
    let serve = config
        .extras
        .entry("serve".into())
        .or_insert_with(|| serde_yaml_ng::Value::Mapping(Default::default()));
    if !serve.is_mapping() {
        *serve = serde_yaml_ng::Value::Mapping(Default::default());
    }
    serve["enabled"] = serde_yaml_ng::Value::Bool(enabled);
    store.save_global_config(&config)
}

/// A one-time pairing code; with this node's LAN or tailnet `address` also
/// the phone's join command and its QR rendered as terminal text.
pub(crate) fn pairing_code(
    root: &Path,
    address: &str,
) -> Result<(String, Option<(String, String)>)> {
    let store = ProjectStore::at(root);
    let address = address.trim();
    if address.is_empty() {
        return Ok((pairing::create_code(&store)?, None));
    }
    let invite = pairing::phone_invite(&store, address).map_err(|_| {
        invalid("The phone QR needs this server's address: LAN/tailnet host:port or https URL, e.g. 192.168.0.10:7420")
    })?;
    Ok((invite.code, Some((invite.command, invite.qr))))
}

pub(crate) fn pair(root: &Path, address: &str, code: &str) -> Result<String> {
    if code.trim().is_empty() && pairing::parse_invite(address).is_some() {
        return pairing::join(root, address, None);
    }
    if code.trim().is_empty() {
        return Err(invalid("Enter the pairing code"));
    }
    pairing::pair(&ProjectStore::at(root), address.trim(), code.trim().into())
}
