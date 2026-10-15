#![cfg(unix)]
use std::process::Command;

fn command(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("kanban4ai"));
    cmd.env("KANBAN_HOME", root).env_remove("KANBAN_PROJECT");
    cmd
}

#[test]
fn phone_qr_uses_expiring_pair_code_not_bearer_credentials() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let output = command(dir.path())
        .args(["serve", "--phone-qr", "192.168.1.2:7420"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let payload: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(payload["role"], "mobile");
    assert_eq!(payload["dial"], "outbound");
    assert!(payload.get("token").is_none());
    let code = payload["code"].as_str().unwrap();
    assert_eq!(
        text.lines().nth(1).unwrap(),
        format!("On the phone run: kanban4ai join {code}@192.168.1.2:7420")
    );
    let path = dir.path().join("sync/pair-code.yaml");
    let registry = std::fs::read_to_string(&path).unwrap();
    assert!(!registry.contains(code));
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!dir.path().join("sync/tokens.yaml").exists());
}

#[test]
fn retiring_clients_preserves_existing_node_credentials() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sync")).unwrap();
    let path = dir.path().join("sync/tokens.yaml");
    let node_hash = "a".repeat(64);
    let client_hash = "b".repeat(64);
    std::fs::write(&path, format!(
        "tokens:\n- kind: node\n  sha256: {node_hash}\n- kind: client\n  sha256: {client_hash}\n"
    )).unwrap();
    let output = command(dir.path())
        .args(["serve", "--issue-token", "node"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let registry = std::fs::read_to_string(path).unwrap();
    assert!(registry.contains(&node_hash));
    assert!(!registry.contains(&client_hash));
    assert!(!registry.contains("kind: client"));
    let rejected = command(dir.path())
        .args(["serve", "--issue-token", "client"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
}

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn phone_joins_from_one_invite_and_configures_itself() {
    let server = tempfile::tempdir().unwrap();
    let phone = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::write(
        server.path().join("config.yaml"),
        format!("node:\n  name: srv\n  role: server\nserve:\n  port: {port}\n"),
    )
    .unwrap();
    let _serve = Server(
        command(server.path())
            .arg("serve")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(std::time::Instant::now() < deadline, "serve did not start");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let output = command(server.path())
        .args(["serve", "--phone-qr", &format!("127.0.0.1:{port}")])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let invite = text
        .lines()
        .nth(1)
        .and_then(|line| line.strip_prefix("On the phone run: kanban4ai join "))
        .unwrap()
        .to_string();
    let joined = command(phone.path())
        .args(["join", &invite])
        .output()
        .unwrap();
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stderr)
    );
    assert!(String::from_utf8_lossy(&joined.stdout).contains("Paired with srv"));
    let config = std::fs::read_to_string(phone.path().join("config.yaml")).unwrap();
    assert!(config.contains("name: phone") && config.contains("role: mobile"));
    assert!(config.contains("name: srv"));
    assert!(phone.path().join("sync/peers.yaml").exists());
    // The invite is single-use.
    let again = command(phone.path())
        .args(["join", &invite])
        .output()
        .unwrap();
    assert!(!again.status.success());
}
