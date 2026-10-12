#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use kanban4ai::core::net::{self, Manager, Mode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SECRET: &str = "tskey-auth-NEVER-LOG-THIS-TEST-SECRET";

fn script(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

struct Fixture {
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("net/bin")).unwrap();
        fs::create_dir(root.join("net/tailscale")).unwrap();
        fs::create_dir(root.join("empty-path")).unwrap();
        fs::write(
            root.join("config.yaml"),
            "net:\n  tailscale:\n    mode: managed\n    socks_port: 11055\n",
        )
        .unwrap();
        script(
            &root.join("net/bin/tailscaled"),
            r#"#!/bin/sh
set -eu
socket=''
state=''
for arg in "$@"; do
    case "$arg" in
        --socket=*) socket=${arg#--socket=} ;;
        --statedir=*) state=${arg#--statedir=} ;;
    esac
done
printf '%s\n' "$@" > "$state/daemon-args"
export KANBAN_NET_FAKE_SOCKET="$socket"
export KANBAN_NET_FAKE_STATE="$state"
export KANBAN_NET_FAKE_PARENT=$$
"$KANBAN_NET_TEST_EXE" --exact fake_daemon_helper --nocapture &
worker=$!
trap 'kill "$worker" 2>/dev/null || :; wait "$worker" 2>/dev/null || :; exit 0' TERM INT
wait "$worker"
"#,
        );
        script(
            &root.join("net/bin/tailscale"),
            r#"#!/bin/sh
set -eu
umask 077
state="$KANBAN_NET_TEST_ROOT/net/tailscale"
printf '%s\n' "$@" > "$state/cli-args"
case "${1-}" in --socket=*) shift ;; esac
verb=${1-}
shift || :
case "$verb" in
    status)
        [ -S "$KANBAN_NET_TEST_ROOT/net/tailscale.sock" ] || [ -f "$state/system" ] || exit 1
        if [ -f "$state/running" ]; then
            printf '%s\n' '{"BackendState":"Running","Self":{"TailscaleIPs":["100.64.0.1"]}}'
        else
            printf '%s\n' '{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/fake-login"}'
        fi
        ;;
    up)
        auth=''
        for arg in "$@"; do
            case "$arg" in --auth-key=file:*) auth=${arg#--auth-key=file:} ;; esac
        done
        if [ -n "$auth" ]; then
            if [ -f "$state/fail-login" ]; then
                /bin/cat "$auth" >&2
                exit 1
            fi
            [ -s "$auth" ] || exit 1
            [ -f "$state/pending-login" ] && exit 0
            : > "$state/running"
        else
            [ -f "$state/running" ] && exit 0
            printf '%s\n' '{"AuthURL":"https://login.tailscale.com/a/fake-login"}'
            exit 1
        fi
        ;;
    logout|down) /bin/rm -f "$state/running" ;;
    *) exit 2 ;;
esac
"#,
        );
        Self { temp }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn run(&self, case: &str, fallback: bool) -> Output {
        let path = if fallback {
            self.root().join("empty-path")
        } else {
            self.root().join("net/bin")
        };
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fake_process_scenario", "--nocapture"])
            .env("KANBAN_NET_TEST_CASE", case)
            .env("KANBAN_NET_TEST_ROOT", self.root())
            .env("KANBAN_NET_TEST_EXE", std::env::current_exe().unwrap())
            .env("PATH", path)
            .output()
            .unwrap()
    }

    fn check(&self, case: &str) -> Output {
        let output = self.run(case, false);
        assert!(
            output.status.success(),
            "scenario {case}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

fn record(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("net/tailscale/pid.json")).unwrap()).unwrap()
}

fn pid(root: &Path) -> u32 {
    record(root)["pid"].as_u64().unwrap() as u32
}

fn alive(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat[stat.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .next();
    !matches!(state, Some("Z" | "X"))
}

fn wait_dead(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!alive(pid), "fake sidecar {pid} did not stop");
}

fn running(root: &Path) {
    fs::write(root.join("net/tailscale/running"), "").unwrap();
}

fn key(root: &Path) -> PathBuf {
    let path = root.join("auth-key");
    fs::write(&path, SECRET).unwrap();
    path
}

// Child processes isolate PATH changes from parallel test threads. The fake
// binaries never contact Tailscale or the network; the only socket is Unix.
#[test]
fn fake_process_scenario() {
    let Ok(case) = std::env::var("KANBAN_NET_TEST_CASE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("KANBAN_NET_TEST_ROOT").unwrap());
    match case.as_str() {
        "lifecycle" => {
            running(&root);
            let mut first = Manager::new(&root).unwrap();
            first.tick().unwrap();
            let original = pid(&root);
            let mut observer = Manager::new(&root).unwrap();
            observer.tick().unwrap();
            assert_eq!(pid(&root), original);
            drop(observer);
            assert!(alive(original));
            unsafe {
                libc::kill(original as i32, libc::SIGKILL);
            }
            wait_dead(original);
            first.tick().unwrap();
            let restarted = pid(&root);
            assert_ne!(original, restarted);
            first.tick().unwrap();
            assert_eq!(pid(&root), restarted);
            let state = root.join("net/tailscale");
            for dir in [&root.join("net"), &state] {
                assert_eq!(
                    fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            for file in [
                "pid.json",
                "owner.lock",
                "control.lock",
                "tailscaled.state",
                "daemon-args",
            ] {
                assert_eq!(
                    fs::metadata(state.join(file)).unwrap().permissions().mode() & 0o777,
                    0o600,
                    "{file}"
                );
            }
            assert_eq!(
                fs::metadata(root.join("net/tailscale.sock"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            let args = fs::read_to_string(state.join("daemon-args")).unwrap();
            assert!(args.contains("--tun=userspace-networking"));
            assert!(args.contains("--socks5-server=127.0.0.1:11055"));
            assert!(args.contains(&format!("--statedir={}", state.display())));
            assert_eq!(
                net::proxy_for(&root, "100.64.1.2:7443").unwrap().as_deref(),
                Some("socks5h://127.0.0.1:11055")
            );
            assert_eq!(
                net::proxy_for(&root, "machine.tail123.ts.net:7443")
                    .unwrap()
                    .as_deref(),
                Some("socks5h://127.0.0.1:11055")
            );
            assert_eq!(net::proxy_for(&root, "192.168.1.2:7443").unwrap(), None);
            drop(first);
            wait_dead(restarted);
            assert!(!state.join("pid.json").exists());
        }
        "stop" => {
            running(&root);
            let mut manager = Manager::new(&root).unwrap();
            manager.tick().unwrap();
            let child = pid(&root);
            net::down(&root).unwrap();
            wait_dead(child);
            manager.tick().unwrap();
            assert!(!root.join("net/tailscale/pid.json").exists());
            let auth = key(&root);
            net::up(&root, Some(&auth)).unwrap();
            let detached = pid(&root);
            manager.tick().unwrap();
            let supervised = pid(&root);
            assert_ne!(detached, supervised);
            wait_dead(detached);
            assert!(!record(&root)["persistent"].as_bool().unwrap());
            drop(manager);
            wait_dead(supervised);
        }
        "login" => {
            let mut manager = Manager::new(&root).unwrap();
            assert!(
                manager
                    .tick()
                    .unwrap_err()
                    .to_string()
                    .contains("NET_TAILSCALE_LOGIN_REQUIRED")
            );
            assert!(
                net::up(&root, None)
                    .unwrap_err()
                    .to_string()
                    .contains("NET_TAILSCALE_LOGIN_REQUIRED")
            );
            let auth = key(&root);
            fs::write(root.join("net/tailscale/fail-login"), "").unwrap();
            assert!(
                net::up(&root, Some(&auth))
                    .unwrap_err()
                    .to_string()
                    .contains("NET_TAILSCALE_LOGIN_REQUIRED")
            );
            assert!(auth.exists());
            assert_eq!(
                fs::metadata(&auth).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::remove_file(root.join("net/tailscale/fail-login")).unwrap();
            fs::write(root.join("net/tailscale/pending-login"), "").unwrap();
            assert!(
                net::up(&root, Some(&auth))
                    .unwrap_err()
                    .to_string()
                    .contains("NET_TAILSCALE_LOGIN_REQUIRED")
            );
            assert!(
                auth.exists(),
                "successful command alone must not consume the key"
            );
            fs::remove_file(root.join("net/tailscale/pending-login")).unwrap();
            net::up(&root, Some(&auth)).unwrap();
            assert!(!auth.exists());
            assert_eq!(net::status(&root).unwrap()["BackendState"], "Running");
            let args = fs::read_to_string(root.join("net/tailscale/cli-args")).unwrap();
            assert!(args.contains(&format!(
                "--socket={}",
                root.join("net/tailscale.sock").display()
            )));
            net::logout(&root).unwrap();
            assert_eq!(net::status(&root).unwrap()["BackendState"], "NeedsLogin");
            assert!(
                net::proxy_for(&root, "100.64.0.2:7443")
                    .unwrap_err()
                    .to_string()
                    .contains("NET_TAILSCALE_LOGIN_REQUIRED")
            );
            drop(manager);
            net::down(&root).unwrap();
        }
        "auth-config" => {
            let auth = key(&root);
            fs::write(
                root.join("config.yaml"),
                "net:\n  tailscale:\n    mode: managed\n    auth_key_file: auth-key\n",
            )
            .unwrap();
            let mut manager = Manager::new(&root).unwrap();
            manager.tick().unwrap();
            assert!(!auth.exists());
            manager.tick().unwrap();
            let child = pid(&root);
            drop(manager);
            wait_dead(child);
        }
        "owner-exit" => {
            running(&root);
            let mut manager = Manager::new(&root).unwrap();
            manager.tick().unwrap();
            // Simulate abrupt supervisor exit without Rust Drop.
            std::process::exit(0);
        }
        "persist" => {
            let auth = key(&root);
            net::up(&root, Some(&auth)).unwrap();
            assert!(!auth.exists());
            assert!(alive(pid(&root)));
        }
        "adopt" => {
            let child = pid(&root);
            let mut manager = Manager::new(&root).unwrap();
            manager.tick().unwrap();
            let supervised = pid(&root);
            assert_ne!(supervised, child);
            wait_dead(child);
            drop(manager);
            wait_dead(supervised);
        }
        "missing" => {
            let mut manager = Manager::new(&root).unwrap();
            assert!(
                manager
                    .tick()
                    .unwrap_err()
                    .to_string()
                    .contains("NET_TAILSCALE_MISSING")
            );
        }
        "fallback" => {
            running(&root);
            let mut manager = Manager::new(&root).unwrap();
            manager.tick().unwrap();
            drop(manager);
        }
        "stale" => {
            let start = fs::read_to_string(format!("/proc/{}/stat", std::process::id())).unwrap();
            let start = start[start.rfind(')').unwrap() + 2..]
                .split_whitespace()
                .nth(19)
                .unwrap();
            let record = json!({"pid":std::process::id(),"start":start,"boot":fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),"persistent":false});
            fs::write(root.join("net/tailscale/pid.json"), record.to_string()).unwrap();
            net::down(&root).unwrap();
            assert!(!root.join("net/tailscale/pid.json").exists());
        }
        "system" => {
            fs::write(
                root.join("config.yaml"),
                "net:\n  tailscale:\n    mode: system\n",
            )
            .unwrap();
            fs::write(root.join("net/tailscale/system"), "").unwrap();
            running(&root);
            let mut manager = Manager::new(&root).unwrap();
            manager.tick().unwrap();
            assert!(!root.join("net/tailscale/pid.json").exists());
            assert_eq!(net::status(&root).unwrap()["mode"], "system");
            assert_eq!(net::proxy_for(&root, "100.64.0.2:7443").unwrap(), None);
            assert!(
                !fs::read_to_string(root.join("net/tailscale/cli-args"))
                    .unwrap()
                    .contains("--socket")
            );
            net::down(&root).unwrap();
            assert!(!root.join("net/tailscale/running").exists());
        }
        "install" => net::install(&root).unwrap(),
        "bad-install" => assert!(net::install(&root).is_err()),
        _ => panic!("unknown fake scenario {case}"),
    }
}

#[test]
fn fake_daemon_helper() {
    let Some(socket) = std::env::var_os("KANBAN_NET_FAKE_SOCKET") else {
        return;
    };
    let parent: libc::pid_t = std::env::var("KANBAN_NET_FAKE_PARENT")
        .unwrap()
        .parse()
        .unwrap();
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM), 0);
        if libc::getppid() != parent {
            return;
        }
    }
    let state = PathBuf::from(std::env::var_os("KANBAN_NET_FAKE_STATE").unwrap());
    fs::write(state.join("tailscaled.state"), "fake-state").unwrap();
    let _listener = UnixListener::bind(socket).unwrap();
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn managed_restart_permissions_ownership_and_drop() {
    Fixture::new().check("lifecycle");
}

#[test]
fn down_prevents_restart_up_reenables_detached() {
    Fixture::new().check("stop");
}

#[test]
fn login_url_qr_key_cleanup_logout_and_secret_redaction() {
    let output = Fixture::new().check("login");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("https://login.tailscale.com/a/fake-login"));
    assert!(text.contains('█') || text.contains('▀') || text.contains('▄'));
    assert!(!text.contains(SECRET));
}

#[test]
fn configured_auth_file_is_consumed_once_after_login() {
    Fixture::new().check("auth-config");
}

#[test]
fn detached_up_survives_cli_exit_and_manager_adoption() {
    let fixture = Fixture::new();
    fixture.check("persist");
    let child = pid(fixture.root());
    assert!(alive(child));
    fixture.check("adopt");
    wait_dead(child);
}

#[test]
fn abrupt_manager_exit_stops_ephemeral_child() {
    let fixture = Fixture::new();
    fixture.check("owner-exit");
    wait_dead(pid(fixture.root()));
}

#[test]
fn missing_binary_reports_machine_code() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root().join("net/bin/tailscaled")).unwrap();
    let output = fixture.run("missing", true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn installed_binaries_resolve_without_path() {
    let fixture = Fixture::new();
    let output = fixture.run("fallback", true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stale_identity_does_not_kill_unrelated_process() {
    Fixture::new().check("stale");
}

#[test]
fn system_mode_never_owns_daemon_or_uses_private_socket() {
    Fixture::new().check("system");
}

#[test]
fn defaults_config_and_tailnet_classification() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let settings = net::settings(root).unwrap();
    assert_eq!(settings.mode, Mode::Off);
    assert_eq!(settings.hostname, "kanban-laptop");
    assert_eq!(settings.socks_port, 1055);
    fs::create_dir(root.join("sync")).unwrap();
    for role in ["laptop", "server", "mobile"] {
        fs::write(root.join("sync/node.yaml"), format!("role: {role}\n")).unwrap();
        let config = net::settings(root).unwrap();
        assert_eq!(config.mode, Mode::Off);
        assert_eq!(config.hostname, format!("kanban-{role}"));
    }
    for address in [
        "100.64.0.1:7443",
        "100.127.255.255",
        "[fd7a:115c:a1e0::1]:7443",
        "https://HOST.tail123.ts.net:7443/api",
        "host.tail123.ts.net.",
    ] {
        assert!(net::is_tailnet_address(address), "{address}");
    }
    for address in [
        "100.63.255.255",
        "100.128.0.1",
        "192.168.1.1:7443",
        "10.0.0.1",
        "127.0.0.1",
        "[fd7a:115c:a1e1::1]:7443",
        "localhost",
        "host.lan",
        "foo.ts.net.evil.com",
        "λ.ts.net",
        "🦀🦀.ts.net",
    ] {
        assert!(!net::is_tailnet_address(address), "{address}");
    }
    for text in [
        "mode: invalid",
        "socks_port: 0",
        "socks_port: 65536",
        "hostname: ../bad",
        "auth_key_file: 42",
    ] {
        fs::write(
            root.join("config.yaml"),
            format!("net:\n  tailscale:\n    {text}\n"),
        )
        .unwrap();
        assert!(net::settings(root).is_err(), "{text}");
    }
    fs::write(
        root.join("config.yaml"),
        "net:\n  tailscale:\n    auth_key_file: secret/key\n",
    )
    .unwrap();
    assert_eq!(
        net::settings(root).unwrap().auth_key_file,
        Some(root.join("secret/key"))
    );
}

#[test]
fn private_state_rejects_symlink_directory() {
    let temp = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("config.yaml"),
        "net:\n  tailscale:\n    mode: managed\n",
    )
    .unwrap();
    fs::create_dir(temp.path().join("net")).unwrap();
    symlink(external.path(), temp.path().join("net/tailscale")).unwrap();
    assert!(Manager::new(temp.path()).is_err());
}

fn installer_fixture(fixture: &Fixture, kind: &str) {
    let root = fixture.root();
    let remote = root.join("remote");
    fs::create_dir(&remote).unwrap();
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return,
    };
    fs::write(
        remote.join("index.json"),
        json!({"Tarballs":{(arch):format!("tailscale_1.2.3_{arch}.tgz")}}).to_string(),
    )
    .unwrap();
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    for name in ["tailscale", "tailscaled"] {
        let mut header = tar::Header::new_gnu();
        let data = format!("#!/bin/sh\n# fake {name}\n");
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        if matches!(kind, "link" | "hardlink") && name == "tailscaled" {
            header.set_entry_type(if kind == "hardlink" {
                tar::EntryType::Link
            } else {
                tar::EntryType::Symlink
            });
            header.set_link_name("/tmp/escaped-tailnet").unwrap();
            header.set_size(0);
            header.set_cksum();
            archive
                .append_data(
                    &mut header,
                    format!("tailscale_1.2.3_{arch}/{name}"),
                    &[][..],
                )
                .unwrap();
        } else if kind != "missing" || name != "tailscaled" {
            header.set_cksum();
            archive
                .append_data(
                    &mut header,
                    format!("tailscale_1.2.3_{arch}/{name}"),
                    data.as_bytes(),
                )
                .unwrap();
        }
        if kind == "duplicate" && name == "tailscale" {
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(
                    &mut header,
                    format!("tailscale_1.2.3_{arch}/{name}"),
                    &b"bad"[..],
                )
                .unwrap();
        }
    }
    // Ignored extra members cannot overwrite anything outside the install.
    let mut header = tar::Header::new_gnu();
    header.set_size(7);
    header.set_mode(0o777);
    header.set_cksum();
    archive
        .append_data(&mut header, "unrelated/payload", &b"ignored"[..])
        .unwrap();
    let data = archive.into_inner().unwrap().finish().unwrap();
    fs::write(remote.join("archive.tgz"), &data).unwrap();
    let checksum = if kind == "checksum" {
        "0".repeat(64)
    } else {
        format!("{:x}", Sha256::digest(&data))
    };
    fs::write(remote.join("archive.sha256"), checksum).unwrap();
    script(
        &root.join("net/bin/curl"),
        r#"#!/bin/sh
set -eu
dest=''
url=''
while IFS= read -r line; do
    case "$line" in
        'output = "'*) dest=${line#output = \"}; dest=${dest%\"} ;;
        'url = "'*) url=${line#url = \"}; url=${url%\"} ;;
    esac
done
case "$url" in
    *mode=json) name=index.json ;;
    *.sha256) name=archive.sha256 ;;
    *.tgz) name=archive.tgz ;;
    *) exit 1 ;;
esac
/bin/cp "$KANBAN_NET_TEST_ROOT/remote/$name" "$dest"
"#,
    );
}

#[test]
fn installer_verifies_checksum_and_extracts_only_regular_exact_members() {
    if !matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
        return;
    }
    let fixture = Fixture::new();
    installer_fixture(&fixture, "good");
    fixture.check("install");
    for name in ["tailscale", "tailscaled"] {
        let path = fixture.root().join("net/bin").join(name);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("#!/bin/sh\n# fake {name}\n")
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert!(!fixture.root().join("net/bin/unrelated").exists());
    for kind in ["checksum", "duplicate", "link", "hardlink", "missing"] {
        let fixture = Fixture::new();
        let before = fs::read(fixture.root().join("net/bin/tailscale")).unwrap();
        installer_fixture(&fixture, kind);
        fixture.check("bad-install");
        assert_eq!(
            fs::read(fixture.root().join("net/bin/tailscale")).unwrap(),
            before,
            "{kind}"
        );
    }
}
