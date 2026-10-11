#![cfg(unix)]
use std::process::Command;

fn command(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("kanban4ai"));
    cmd.env("KANBAN_HOME", root).env_remove("KANBAN_PROJECT");
    cmd
}

#[test]
fn phone_credentials_are_private_and_revocable() {
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
    let token = payload["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    let path = dir.path().join("sync/tokens.yaml");
    let registry = std::fs::read_to_string(&path).unwrap();
    assert!(!registry.contains(token));
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&registry).unwrap();
    assert_eq!(value["tokens"][0]["kind"].as_str(), Some("client"));
    let fingerprint = value["tokens"][0]["sha256"].as_str().unwrap();
    assert!(
        command(dir.path())
            .args(["serve", "--revoke-token", fingerprint])
            .status()
            .unwrap()
            .success()
    );
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        &std::fs::read_to_string(dir.path().join("sync/tokens.yaml")).unwrap(),
    )
    .unwrap();
    assert!(value["tokens"].as_sequence().unwrap().is_empty());
}
