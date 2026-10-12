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
