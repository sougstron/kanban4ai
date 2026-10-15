//! Store-scoped operations behind Global Settings → Sync (TASK-415). The
//! TUI calls these instead of CLI subcommands; nothing here prints.
use super::discovery::{self, PeerCard};
use super::*;
use crate::core::sync::{Journal, NodeDial, NodeRole, VersionVector};
use serde_json::Value;
use std::collections::BTreeMap;
use std::process::Command;

const UNIT: &str = "kanban4ai-serve.service";

/// The last vector a peer confirmed, per peer, in `sync/exchanges.yaml`.
#[derive(Default, Serialize, Deserialize)]
pub(super) struct Exchange {
    at: Option<chrono::DateTime<chrono::Utc>>,
    acknowledged: VersionVector,
}

/// Remember what `peer` holds after a push or a lease request.
pub(super) fn acknowledge(root: &Path, peer: &str, vector: &VersionVector) -> Result<()> {
    let _guard = lock_file_reentrant(&root.join("sync/.lock"))?;
    let path = root.join("sync/exchanges.yaml");
    let mut all: BTreeMap<String, Exchange> = discovery::load(&path)?;
    all.insert(
        peer.into(),
        Exchange {
            at: Some(chrono::Utc::now()),
            acknowledged: vector.clone(),
        },
    );
    atomic_write_text_raw(&path, &serde_yaml_ng::to_string(&all)?)?;
    drop(_guard);
    // Prune outside the sync lock. A peer with no ack contributes an empty vector.
    let journal = Journal::open(root)?;
    let mut acks = BTreeMap::new();
    if let Ok(config) = discovery::config(root) {
        for peer in config.peers {
            acks.insert(peer.node.node_id, VersionVector::new());
        }
    }
    let saved: BTreeMap<String, Exchange> = discovery::load(&path)?;
    for (peer, exchange) in saved {
        acks.insert(peer, exchange.acknowledged);
    }
    journal.prune(&acks)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Peer {
    pub id: String,
    pub name: String,
    pub role: NodeRole,
    /// «LAN», «tailnet», or why no route was tried.
    pub route: String,
    /// Status code (section 13.1); empty for a phone, which is shown neutrally.
    pub code: String,
    pub message: String,
    /// Local time of the last confirmed exchange.
    pub exchange: Option<String>,
    /// Local operations the peer has not confirmed yet.
    pub pending: u64,
    pub clones: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct View {
    pub name: String,
    pub role: NodeRole,
    /// `serve.enabled`: the switch the human flips.
    pub api_enabled: bool,
    /// What systemd reports right now; `None` when it could not be asked.
    pub api_active: Option<bool>,

    /// In priority order (`sync.peers`).
    pub peers: Vec<Peer>,
    pub conflicts: Vec<String>,
    pub local_clones: Vec<String>,
    /// Peer statuses come from a live probe rather than stored data.
    pub probed: bool,
}

fn systemctl(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("systemctl").arg("--user").args(args).output()
}

fn unit_active() -> Option<bool> {
    if cfg!(test) {
        return None;
    }
    let output = systemctl(&["is-active", "--quiet", UNIT]).ok()?;
    Some(output.status.success())
}

fn api_enabled(root: &Path) -> Result<bool> {
    let config = ProjectStore::at(root).load_global_config()?;
    Ok(config
        .extras
        .get("serve")
        .and_then(|serve| serve.get("enabled"))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false))
}

fn time_text(at: chrono::DateTime<chrono::Utc>) -> String {
    let at = at.with_timezone(&chrono::Local);
    if at.date_naive() == chrono::Local::now().date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%d.%m %H:%M").to_string()
    }
}

/// Operations in `local` that `acknowledged` does not cover.
fn pending(local: &VersionVector, acknowledged: &VersionVector) -> u64 {
    local
        .iter()
        .map(|(node, seq)| seq.saturating_sub(acknowledged.get(node).copied().unwrap_or(0)))
        .sum()
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

/// Lightweight board status: no systemd or clone subprocesses.
pub(crate) fn board_peers(root: &Path) -> Result<Vec<BoardPeer>> {
    let config = discovery::config(root)?;
    if config.peers.is_empty() {
        return Ok(Vec::new());
    }
    let local = Journal::open(root)?.vector()?;
    let exchanges: BTreeMap<String, Exchange> = discovery::load(&root.join("sync/exchanges.yaml"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let statuses = runtime.block_on(discovery::probes(root))?;
    Ok(config
        .peers
        .iter()
        .map(|peer| {
            let status = statuses.iter().find(|s| s.node == peer.node.node_id);
            let exchange = exchanges.get(&peer.node.node_id);
            BoardPeer {
                name: peer.node.name.clone(),
                role: peer.node.role,
                code: status.map_or("OFFLINE", |s| s.code).into(),
                exchange: exchange.and_then(|e| e.at),
                since: status.and_then(|s| s.since.or(s.last_seen)),
                pending: pending(
                    &local,
                    &exchange.map(|e| e.acknowledged.clone()).unwrap_or_default(),
                ),
            }
        })
        .collect())
}

pub(super) fn clone_line(value: &Value) -> String {
    let text = |key| value[key].as_str().unwrap_or("—");
    let state = text("state");
    let integration = value["integration"]
        .as_str()
        .map_or("—".to_string(), |oid| oid.chars().take(8).collect());
    format!(
        "{}: {state} · {} · integration {integration}",
        text("project"),
        text("path")
    )
}

fn conflict_line(conflict: &crate::core::sync::Conflict) -> String {
    let short = |value: &Value| {
        let text = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let mut chars = text.chars();
        let head: String = chars.by_ref().take(24).collect();
        if chars.next().is_some() {
            format!("{head}…")
        } else {
            head
        }
    };
    format!(
        "{}/{} {} · {}: kept «{}», lost «{}»",
        conflict.project,
        conflict.entity,
        conflict.id,
        conflict.field,
        short(&conflict.winner_value),
        short(&conflict.loser_value)
    )
}

fn route_unknown(peer: &PeerCard) -> String {
    if peer.node.outbound_only() {
        "phone dials in".into()
    } else {
        "not probed".into()
    }
}

/// Everything readable without the network: identity, stored exchanges,
/// conflicts and this node's clones. Peer status waits for [`refresh`].
pub(crate) fn local_view(root: &Path) -> Result<View> {
    let store = ProjectStore::at(root);
    let journal = Journal::open(root)?;
    let local = journal.vector()?;
    let config = discovery::config(root)?;
    let exchanges: BTreeMap<String, Exchange> = discovery::load(&root.join("sync/exchanges.yaml"))?;
    let peers = config
        .peers
        .iter()
        .map(|peer| {
            let exchange = exchanges.get(&peer.node.node_id);
            Peer {
                id: peer.node.node_id.clone(),
                name: peer.node.name.clone(),
                role: peer.node.role,
                route: route_unknown(peer),
                code: String::new(),
                message: "Not probed yet — press Refresh".into(),
                exchange: exchange.and_then(|e| e.at).map(time_text),
                pending: pending(
                    &local,
                    &exchange.map(|e| e.acknowledged.clone()).unwrap_or_default(),
                ),
                clones: Vec::new(),
            }
        })
        .collect();
    let local_clones = store
        .list()?
        .into_iter()
        .map(
            |project| match crate::core::code::status(root, &project.id) {
                Ok(status) => clone_line(&serde_json::to_value(status).unwrap_or_default()),
                Err(e) => format!("{}: {e}", project.id),
            },
        )
        .collect();
    Ok(View {
        name: journal.node.name.clone(),
        role: journal.node.role,
        api_enabled: api_enabled(root)?,
        api_active: unit_active(),

        peers,
        conflicts: journal.conflicts()?.iter().map(conflict_line).collect(),
        local_clones,
        probed: false,
    })
}

/// [`local_view`] plus a live probe of every peer and, for reachable ones,
/// their clone statuses. Blocks for up to `sync.probe_timeout` per peer.
pub(crate) fn refresh(root: &Path) -> Result<View> {
    let mut view = local_view(root)?;
    let config = discovery::config(root)?;
    let projects: Vec<String> = ProjectStore::at(root)
        .list()?
        .into_iter()
        .map(|p| p.id)
        .collect();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let statuses = discovery::probes(root).await?;
        for (peer, card) in view.peers.iter_mut().zip(&config.peers) {
            if let Some(status) = statuses.iter().find(|s| s.node == peer.id) {
                peer.code = status.code.into();
                peer.message = status.message.clone();
                if peer.role == NodeRole::Mobile && status.code != "ONLINE" {
                    peer.code.clear();
                    peer.message = status.last_seen.map_or_else(
                        || "Телефон: ещё не был в сети".into(),
                        |at| format!("Телефон: был в сети {}", time_text(at)),
                    );
                }
            }
            if card.node.outbound_only() {
                continue;
            }
            if peer.code != "ONLINE" {
                peer.route = "no route".into();
                peer.clones
                    .push("peer unreachable — clone state unknown".into());
                continue;
            }
            match sync::settings_peer(root, &config, card, &projects).await {
                Ok((route, clones)) => {
                    peer.route = route;
                    peer.clones = clones;
                }
                Err(e) => peer.clones.push(format!("clone status: {e}")),
            }
        }
        Ok::<_, KanbanError>(())
    })?;
    view.probed = true;
    Ok(view)
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
/// Move a peer one step up (`-1`) or down (`1`) in `sync.peers`, the order
/// in which the phone tries nodes.
pub(crate) fn move_peer(root: &Path, id: &str, offset: isize) -> Result<()> {
    let store = ProjectStore::at(root);
    let _guard = store.lock()?;
    let mut config = store.load_global_config()?;
    let peers = config
        .extras
        .get_mut("sync")
        .and_then(|sync| sync.get_mut("peers"))
        .and_then(serde_yaml_ng::Value::as_sequence_mut)
        .ok_or_else(|| invalid("No paired nodes"))?;
    let index = peers
        .iter()
        .position(|p| p.get("node_id").and_then(serde_yaml_ng::Value::as_str) == Some(id))
        .ok_or_else(|| invalid("The node is no longer paired"))?;
    let next = index
        .saturating_add_signed(offset)
        .min(peers.len().saturating_sub(1));
    if next == index {
        return Ok(());
    }
    peers.swap(index, next);
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
        invalid("The phone QR needs this node's LAN or tailnet address, e.g. 192.168.0.10:7420")
    })?;
    Ok((invite.code, Some((invite.command, invite.qr))))
}

/// Pair with the node at `address` using the code it showed. Returns its name.
pub(crate) fn pair(root: &Path, address: &str, code: &str, local: &str) -> Result<String> {
    if code.trim().is_empty() && pairing::parse_invite(address).is_some() {
        return pairing::join(root, address, None);
    }
    let local = match local.trim() {
        "" => None,
        text => Some(
            text.parse()
                .map_err(|_| invalid("This node's address: IP:port"))?,
        ),
    };
    if local.is_none() && !Journal::open(root)?.node.outbound_only() {
        return Err(invalid(
            "Enter this node's LAN address so the peer can dial back",
        ));
    }
    let code = code.trim();
    if code.is_empty() {
        return Err(invalid("Enter the pairing code"));
    }
    pairing::pair(&ProjectStore::at(root), address.trim(), code.into(), local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_counts_only_unconfirmed_operations() {
        let local = VersionVector::from([("a".into(), 5), ("b".into(), 2)]);
        let ack = VersionVector::from([("a".into(), 3), ("b".into(), 4)]);
        assert_eq!(pending(&local, &ack), 2);
        assert_eq!(pending(&local, &VersionVector::new()), 7);
    }

    #[test]
    fn node_settings_validate_and_follow_role() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(save_node(root, "Bad Name", NodeRole::Laptop).is_err());
        save_node(root, "phone", NodeRole::Mobile).unwrap();
        let node = Journal::open(root).unwrap().node;
        assert_eq!((node.name.as_str(), node.role), ("phone", NodeRole::Mobile));
        assert!(node.outbound_only());
        save_node(root, "srv", NodeRole::Server).unwrap();
        assert!(!Journal::open(root).unwrap().node.outbound_only());
    }

    #[test]
    fn peers_reorder_within_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = ProjectStore::at(root);
        let mut config = store.load_global_config().unwrap();
        config.extras.insert(
            "sync".into(),
            serde_yaml_ng::from_str("peers: [{node_id: a}, {node_id: b}]").unwrap(),
        );
        store.save_global_config(&config).unwrap();
        move_peer(root, "b", -1).unwrap();
        move_peer(root, "b", -1).unwrap();
        let config = store.load_global_config().unwrap();
        let order: Vec<_> = config.extras["sync"]["peers"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|p| p["node_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(order, ["b", "a"]);
        assert!(move_peer(root, "zzz", 1).is_err());
    }
}
