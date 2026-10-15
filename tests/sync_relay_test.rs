use kanban4ai::core::models::TaskStatus;
use kanban4ai::core::storage::{NewTask, Storage, atomic_write_text};
use kanban4ai::core::sync::{Journal, NodeRole, relay};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::Command;

fn git(remote: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(remote)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
struct Network {
    root: tempfile::TempDir,
    nodes: Vec<tempfile::TempDir>,
    ids: Vec<String>,
}
impl Network {
    fn new(encrypted: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "--bare", "--quiet"]);
        let nodes: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let store_id = Journal::open(nodes[0].path()).unwrap().node.store_id;
        let roles = [NodeRole::Laptop, NodeRole::Server, NodeRole::Mobile];
        let mut identities = Vec::new();
        for (i, dir) in nodes.iter().enumerate() {
            let mut node = Journal::open(dir.path()).unwrap().node;
            node.store_id = store_id.clone();
            node.role = roles[i];
            node.name = ["laptop", "server", "mobile"][i].into();
            fs::write(
                dir.path().join("sync/node.yaml"),
                serde_yaml_ng::to_string(&node).unwrap(),
            )
            .unwrap();
            identities.push(node);
        }
        for (i, dir) in nodes.iter().enumerate() {
            let peers: Vec<_> = identities
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, node)| {
                    let mut peer = serde_json::to_value(node).unwrap();
                    peer["address"] = json!("127.0.0.1:0");
                    peer
                })
                .collect();
            fs::write(dir.path().join("config.yaml"), serde_yaml_ng::to_string(&json!({
                "sync": {"relay_remote": root.path(), "relay_interval": 60, "relay_encrypt": encrypted,
                    "home_networks": ["192.0.2.0/24"], "peers": peers},
                "net": {"tailscale": {"mode": "off", "auth_key_file": "/private/secret-auth"}}
            })).unwrap()).unwrap();
        }
        if encrypted {
            let keys: Vec<_> = nodes
                .iter()
                .map(|dir| relay::recipient(dir.path()).unwrap().unwrap())
                .collect();
            for (i, dir) in nodes.iter().enumerate() {
                let peers: Vec<_> = identities
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(j, node)| {
                        json!({"node_id": node.node_id, "token": "bearer-do-not-relay",
                        "inbound_sha256": "private-hash", "relay_recipient": keys[j]})
                    })
                    .collect();
                fs::write(
                    dir.path().join("sync/peers.yaml"),
                    serde_yaml_ng::to_string(&peers).unwrap(),
                )
                .unwrap();
            }
        }
        Self {
            root,
            nodes,
            ids: identities.into_iter().map(|node| node.node_id).collect(),
        }
    }
    fn cycle(&self, i: usize) {
        assert_eq!(relay::cycle(self.nodes[i].path(), true).unwrap(), 1);
    }
    fn board(&self, i: usize) -> Storage {
        Storage::new(self.nodes[i].path().join("projects/demo"))
    }
    fn seed(&self) -> String {
        let board = self.board(1);
        fs::create_dir_all(&board.project_path).unwrap();
        atomic_write_text(
            &board.project_path.join("project.yaml"),
            "id: demo\nname: Demo\n",
        )
        .unwrap();
        board.init_board().unwrap();
        let task = board.create_task(NewTask::titled("initial")).unwrap();
        self.cycle(1);
        self.cycle(0);
        self.cycle(2);
        task.id
    }
    fn catch_up(&self) {
        for _ in 0..2 {
            for i in 0..3 {
                self.cycle(i);
            }
        }
    }
}

fn convergence(encrypted: bool) {
    let network = Network::new(encrypted);
    let id = network.seed();
    let image = ".kanban/assets/images/pasted-relay.png";
    fs::create_dir_all(network.board(0).project_path.join(".kanban/assets/images")).unwrap();
    fs::write(network.board(0).project_path.join(image), [255, 128, 0, 42]).unwrap();
    let initial = git(
        network.root.path(),
        &["rev-parse", &format!("refs/kanban/sync/{}", network.ids[0])],
    );
    // Three disconnected replicas change different fields concurrently.
    for i in 0..3 {
        let board = network.board(i);
        let mut task = board.load_task(&id).unwrap().unwrap();
        match i {
            0 => task.title = "laptop edit".into(),
            1 => task.description = "server edit".into(),
            _ => task.status = TaskStatus::Review,
        }
        task.session = Some("ses-machine-secret".into());
        task.worktree = Some("/private/machine-worktree".into());
        board.save_task(&task).unwrap();
        atomic_write_text(
            &network.nodes[i].path().join("config.yaml"),
            &fs::read_to_string(network.nodes[i].path().join("config.yaml")).unwrap(),
        )
        .unwrap();
        fs::write(
            network.nodes[i].path().join("sync/tokens.yaml"),
            "top-secret-bearer",
        )
        .unwrap();
    }
    network.catch_up();
    let expected = Journal::open(network.nodes[0].path())
        .unwrap()
        .vector()
        .unwrap();
    for i in 0..3 {
        assert_eq!(
            fs::read(network.board(i).project_path.join(image)).unwrap(),
            [255, 128, 0, 42]
        );
        let task = network.board(i).load_task(&id).unwrap().unwrap();
        assert_eq!(task.title, "laptop edit");
        assert_eq!(task.description, "server edit");
        assert_eq!(task.status, TaskStatus::Review);
        let journal = Journal::open(network.nodes[i].path()).unwrap();
        assert_eq!(journal.vector().unwrap(), expected);
        // Duplicate delivery through another transport cannot change the replica.
        let before = serde_json::to_value(journal.state().unwrap().fields).unwrap();
        let all = Journal::open(network.nodes[0].path())
            .unwrap()
            .operations_after(&Default::default(), usize::MAX)
            .unwrap()
            .operations;
        journal.apply_remote(&all, 300_000).unwrap();
        assert_eq!(
            serde_json::to_value(journal.state().unwrap().fields).unwrap(),
            before
        );
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let peer = &network.ids[(i + 1) % 3];
        assert!(relay::available(network.nodes[i].path(), peer, now).unwrap());
        assert!(!relay::available(network.nodes[i].path(), peer, now + 120_001).unwrap());
    }
    // Only the three node-owned refs exist, and old tips remain ancestors.
    let refs = git(
        network.root.path(),
        &["for-each-ref", "--format=%(refname)"],
    );
    assert_eq!(
        refs.lines().collect::<std::collections::BTreeSet<_>>(),
        network
            .ids
            .iter()
            .map(|id| format!("refs/kanban/sync/{id}"))
            .collect::<std::collections::BTreeSet<_>>()
            .iter()
            .map(String::as_str)
            .collect()
    );
    git(
        network.root.path(),
        &[
            "merge-base",
            "--is-ancestor",
            initial.trim(),
            &format!("refs/kanban/sync/{}", network.ids[0]),
        ],
    );
    let commits = git(network.root.path(), &["rev-list", "--all"]);
    for commit in commits.lines() {
        let name = if encrypted {
            "batch.age"
        } else {
            "batch.jsonl"
        };
        let content = git(network.root.path(), &["show", &format!("{commit}:{name}")]);
        for forbidden in [
            "top-secret-bearer",
            "ses-machine-secret",
            "/private/machine-worktree",
            "secret-auth",
            "bearer-do-not-relay",
        ] {
            assert!(!content.contains(forbidden), "leaked {forbidden}");
        }
        if encrypted {
            assert!(content.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
        }
    }
    // Lose the cache: replay from the remote is idempotent and our writer
    // resumes the old chain instead of replacing its ref.
    fs::remove_dir_all(network.nodes[1].path().join("sync/relay")).unwrap();
    network.cycle(1);
    assert_eq!(
        Journal::open(network.nodes[1].path())
            .unwrap()
            .vector()
            .unwrap(),
        expected
    );
    assert_eq!(relay::cycle(network.nodes[1].path(), false).unwrap(), 0);
}
#[test]
fn three_nodes_converge_only_through_bare_git_and_redelivery() {
    convergence(false);
}
#[test]
fn encrypted_three_node_relay_converges_without_publishing_plaintext() {
    if Command::new("age").arg("--version").output().is_err() {
        eprintln!("age unavailable; encrypted relay integration requires age + age-keygen");
        return;
    }
    convergence(true);
}
#[test]
fn missing_peer_encryption_key_fails_closed_before_push() {
    if Command::new("age").arg("--version").output().is_err() {
        return;
    }
    let network = Network::new(true);
    fs::write(network.nodes[0].path().join("sync/peers.yaml"), "[]\n").unwrap();
    assert!(
        relay::cycle(network.nodes[0].path(), true)
            .unwrap_err()
            .to_string()
            .contains("SYNC_RELAY_KEY_MISSING")
    );
    assert_eq!(
        git(
            network.root.path(),
            &["for-each-ref", "--format=%(refname)"]
        ),
        ""
    );
}
#[test]
fn relay_uses_project_origin_when_no_store_remote_is_configured() {
    let network = Network::new(false);
    let work = tempfile::tempdir().unwrap();
    git(work.path(), &["init", "--bare", "--quiet"]);
    git(
        work.path(),
        &[
            "remote",
            "add",
            "origin",
            network.root.path().to_str().unwrap(),
        ],
    );
    let board = network.board(0);
    fs::create_dir_all(&board.project_path).unwrap();
    atomic_write_text(
        &board.project_path.join("project.yaml"),
        &format!(
            "id: demo\nname: Demo\nwork_path: {}\ncreated_at: '2026-10-12T00:00:00'\n",
            work.path().display()
        ),
    )
    .unwrap();
    let path = network.nodes[0].path().join("config.yaml");
    let mut config: serde_json::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    config["sync"]
        .as_object_mut()
        .unwrap()
        .remove("relay_remote");
    fs::write(path, serde_yaml_ng::to_string(&config).unwrap()).unwrap();
    network.cycle(0);
    let batch = git(
        network.root.path(),
        &[
            "show",
            &format!("refs/kanban/sync/{}:batch.jsonl", network.ids[0]),
        ],
    );
    let operations: Vec<serde_json::Value> = batch
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let project = operations
        .iter()
        .find(|op| op["entity"] == "project")
        .unwrap();
    assert_eq!(project["set"]["name"], "Demo");
    assert!(project["set"].get("work_path").is_none());
}

#[test]
fn encryption_policy_replicates_without_replacing_local_transport_or_net_settings() {
    let network = Network::new(false);
    network.seed();
    let path = network.nodes[0].path().join("config.yaml");
    let mut config: serde_json::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    config["sync"]["relay_encrypt"] = json!(true);
    config["net"]["tailscale"]["auth_key_file"] = json!("/private/changed-key");
    atomic_write_text(&path, &serde_yaml_ng::to_string(&config).unwrap()).unwrap();
    let journal = Journal::open(network.nodes[0].path()).unwrap();
    let operations = journal
        .operations_after(&Default::default(), usize::MAX)
        .unwrap()
        .operations;
    let replica = Journal::open(network.nodes[1].path()).unwrap();
    replica.apply_remote(&operations, 300_000).unwrap();
    let peer: serde_json::Value = serde_yaml_ng::from_str(
        &fs::read_to_string(network.nodes[1].path().join("config.yaml")).unwrap(),
    )
    .unwrap();
    assert_eq!(peer["sync"]["relay_encrypt"], true);
    assert_eq!(
        peer["net"]["tailscale"]["auth_key_file"],
        "/private/secret-auth"
    );
    let empty = tempfile::tempdir().unwrap();
    Journal::open(empty.path()).unwrap();
    fs::write(empty.path().join("config.yaml"),
        "sync:\n  relay_remote: /local/remote\n  relay_interval: 31\nnet:\n  tailscale:\n    auth_key_file: /local/key\n").unwrap();
    kanban4ai::core::sync::install_snapshot(empty.path(), &journal.snapshot().unwrap()).unwrap();
    let installed: serde_json::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(empty.path().join("config.yaml")).unwrap())
            .unwrap();
    assert_eq!(installed["sync"]["relay_encrypt"], true);
    assert_eq!(installed["sync"]["relay_remote"], "/local/remote");
    assert_eq!(installed["sync"]["relay_interval"], 31);
    assert_eq!(installed["net"]["tailscale"]["auth_key_file"], "/local/key");
}

#[test]
fn rewound_peer_ref_is_rejected_without_rolling_back_the_replica() {
    let network = Network::new(false);
    let id = network.seed();
    let reference = format!("refs/kanban/sync/{}", network.ids[1]);
    let old = git(network.root.path(), &["rev-parse", &reference]);
    let board = network.board(1);
    let mut task = board.load_task(&id).unwrap().unwrap();
    task.title = "must survive relay rewind".into();
    board.save_task(&task).unwrap();
    network.catch_up();
    git(network.root.path(), &["update-ref", &reference, old.trim()]);
    assert!(relay::cycle(network.nodes[0].path(), true).is_err());
    assert_eq!(
        network.board(0).load_task(&id).unwrap().unwrap().title,
        "must survive relay rewind"
    );
}

#[test]
fn new_encrypted_peer_skips_history_already_received_by_bootstrap() {
    if Command::new("age").arg("--version").output().is_err() {
        return;
    }
    let network = Network::new(true);
    let mut originals = Vec::new();
    for i in 0..2 {
        let path = network.nodes[i].path().join("config.yaml");
        let mut config: serde_json::Value =
            serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        originals.push(config.clone());
        config["sync"]["peers"]
            .as_array_mut()
            .unwrap()
            .retain(|peer| peer["node_id"] != network.ids[2]);
        fs::write(path, serde_yaml_ng::to_string(&config).unwrap()).unwrap();
    }
    let board = network.board(1);
    fs::create_dir_all(&board.project_path).unwrap();
    atomic_write_text(
        &board.project_path.join("project.yaml"),
        "id: demo\nname: Demo\n",
    )
    .unwrap();
    board.init_board().unwrap();
    let mut task = board
        .create_task(NewTask::titled("before mobile pairing"))
        .unwrap();
    network.cycle(1);
    network.cycle(0);
    let source = Journal::open(network.nodes[1].path()).unwrap();
    kanban4ai::core::sync::install_snapshot(network.nodes[2].path(), &source.snapshot().unwrap())
        .unwrap();
    for (i, config) in originals.iter().enumerate() {
        fs::write(
            network.nodes[i].path().join("config.yaml"),
            serde_yaml_ng::to_string(config).unwrap(),
        )
        .unwrap();
    }
    task.title = "after mobile pairing".into();
    board.save_task(&task).unwrap();
    network.cycle(1);
    network.cycle(0);
    // The old ciphertext never included mobile's recipient. It must not be
    // decrypted again when the bootstrap vector already covers those operations.
    network.cycle(2);
    assert_eq!(
        network.board(2).load_task(&task.id).unwrap().unwrap().title,
        "after mobile pairing"
    );
}
