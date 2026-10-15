//! Opt-in Tailscale networking. The system daemon is never owned or stopped.
//! Managed daemons use a private userspace socket/state directory. A lifetime
//! owner lock elects one supervisor; a separate operation lock serializes CLI
//! actions with it. PID records include Linux process identity, not just a PID.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::error::{KanbanError, Result};
use super::global::read_global_config;
use super::http::http_download;

const MISSING: &str = "NET_TAILSCALE_MISSING";
const LOGIN: &str = "NET_TAILSCALE_LOGIN_REQUIRED";
const DOWN: &str = "NET_TAILSCALE_SIDECAR_DOWN";
const SOCKS_PORT: u16 = 1055;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    System,
    Managed,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::System => "system",
            Self::Managed => "managed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub mode: Mode,
    pub hostname: String,
    pub socks_port: u16,
    pub auth_key_file: Option<PathBuf>,
}

fn invalid(message: impl Into<String>) -> KanbanError {
    KanbanError::Invalid(message.into())
}

pub fn settings(root: &Path) -> Result<Settings> {
    let config = read_global_config(&root.join("config.yaml"))?;
    let net = config.extras.get("net").and_then(|v| v.get("tailscale"));
    if net.is_some_and(|v| !v.is_mapping()) {
        return Err(invalid("net.tailscale must be a mapping"));
    }
    let get = |key: &str| net.and_then(|v| v.get(key));
    let mode = match get("mode").and_then(|v| v.as_str()).unwrap_or("off") {
        "off" => Mode::Off,
        "system" => Mode::System,
        "managed" => Mode::Managed,
        _ => {
            return Err(invalid(
                "net.tailscale.mode must be off, system, or managed",
            ));
        }
    };
    if get("mode").is_some_and(|v| v.as_str().is_none()) {
        return Err(invalid("net.tailscale.mode must be a string"));
    }
    let node_path = root.join("sync/node.yaml");
    let node: serde_yaml_ng::Value = match fs::read_to_string(node_path) {
        Ok(text) => serde_yaml_ng::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_yaml_ng::Value::Null,
        Err(e) => return Err(e.into()),
    };
    let role = node
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("laptop");
    let hostname = match get("hostname") {
        Some(v) => v
            .as_str()
            .ok_or_else(|| invalid("net.tailscale.hostname must be a string"))?
            .to_owned(),
        None => format!("kanban-{role}"),
    };
    if hostname.is_empty()
        || hostname.len() > 63
        || !hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || hostname.starts_with('-')
        || hostname.ends_with('-')
    {
        return Err(invalid(
            "net.tailscale.hostname must be a DNS hostname label",
        ));
    }
    let socks_port = match get("socks_port") {
        Some(v) => v
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| *n != 0)
            .ok_or_else(|| invalid("net.tailscale.socks_port must be between 1 and 65535"))?,
        None => SOCKS_PORT,
    };
    let auth_key_file = match get("auth_key_file") {
        Some(v) if !v.is_null() => {
            let path = v
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("net.tailscale.auth_key_file must be a file path"))?;
            let path = PathBuf::from(path);
            Some(if path.is_absolute() {
                path
            } else {
                root.join(path)
            })
        }
        _ => None,
    };
    Ok(Settings {
        mode,
        hostname,
        socks_port,
        auth_key_file,
    })
}

#[derive(Clone)]
struct Paths {
    net: PathBuf,
    state: PathBuf,
    socket: PathBuf,
}

impl Paths {
    fn new(root: &Path) -> Self {
        let net = root.join("net");
        Self {
            state: net.join("tailscale"),
            socket: net.join("tailscale.sock"),
            net,
        }
    }

    fn prepare(&self) -> Result<()> {
        private_dir(&self.net)?;
        private_dir(&self.state)?;
        Ok(())
    }

    fn record(&self) -> PathBuf {
        self.state.join("pid.json")
    }
    fn stopped(&self) -> PathBuf {
        self.state.join("stopped")
    }
    fn operation(&self) -> Result<File> {
        let file = private_file(&self.state.join("control.lock"))?;
        file.lock_exclusive()?;
        secure_tree(&self.state)?;
        Ok(file)
    }
}

fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && m.uid() == unsafe { libc::geteuid() } => {}
        Ok(_) => {
            return Err(invalid(
                "managed Tailscale directory is not a user-owned directory",
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        Err(e) => return Err(e.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } {
        return Err(invalid("managed Tailscale file is not user-owned"));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn secure_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.file_type().is_symlink() {
            return Err(invalid("managed Tailscale state contains an unsafe entry"));
        }
        if metadata.is_dir() {
            private_dir(&entry.path())?;
            secure_tree(&entry.path())?;
        } else {
            match fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o600)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn binary(root: &Path, name: &str) -> Result<PathBuf> {
    // Unit tests drive stub binaries under `<store>/net/bin`; a system-wide
    // Tailscale on PATH would otherwise shadow them.
    if let Some(path) = std::env::var_os("PATH").filter(|_| !cfg!(test)) {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    let installed = root.join("net/bin").join(name);
    if executable(&installed) {
        return Ok(installed);
    }
    Err(invalid(format!(
        "{MISSING}: {name} unavailable; run kanban net install"
    )))
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    pid: u32,
    start: String,
    boot: String,
    persistent: bool,
}

fn process_identity(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat.get(stat.rfind(')')? + 2..)?.split_whitespace();
    if matches!(fields.next(), Some("Z" | "X")) {
        return None;
    }
    fields.nth(18).map(str::to_owned)
}

fn boot_identity() -> Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

fn live(record: &Record, paths: &Paths) -> bool {
    if process_identity(record.pid).as_deref() != Some(record.start.as_str())
        || boot_identity().ok().as_deref() != Some(record.boot.as_str())
    {
        return false;
    }
    let Ok(cmdline) = fs::read(format!("/proc/{}/cmdline", record.pid)) else {
        return false;
    };
    let socket = format!("--socket={}", paths.socket.display());
    cmdline
        .split(|b| *b == 0)
        .any(|arg| arg == socket.as_bytes())
}

fn record(paths: &Paths) -> Result<Option<Record>> {
    match fs::read(paths.record()) {
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|_| invalid(format!("{DOWN}: invalid sidecar ownership record"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save_record(paths: &Paths, record: &Record) -> Result<()> {
    let data = serde_json::to_vec(record).map_err(|e| invalid(e.to_string()))?;
    write_private(&paths.record(), &data)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn socket_permissions(paths: &Paths) -> Result<()> {
    match fs::symlink_metadata(&paths.socket) {
        Ok(m) if m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() } => {
            fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
            Ok(())
        }
        Ok(_) => Err(invalid(format!("{DOWN}: unsafe private socket"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn stop_record(paths: &Paths, rec: &Record) -> Result<()> {
    if live(rec, paths) {
        unsafe {
            libc::kill(rec.pid as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while live(rec, paths) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if live(rec, paths) {
            unsafe {
                libc::kill(rec.pid as libc::pid_t, libc::SIGKILL);
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while live(rec, paths) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if live(rec, paths) {
                return Err(invalid(format!("{DOWN}: sidecar could not be stopped")));
            }
        }
    }
    remove_if_present(&paths.record())?;
    remove_if_present(&paths.socket)?;
    Ok(())
}

fn daemon(root: &Path, config: &Settings, paths: &Paths, persistent: bool) -> Result<Child> {
    binary(root, "tailscale")?;
    let program = binary(root, "tailscaled")?;
    // Linux sockaddr_un.sun_path includes its terminating NUL.
    if paths.socket.as_os_str().as_encoded_bytes().len() >= 108 {
        return Err(invalid(format!(
            "{DOWN}: store path is too long for a private Unix socket"
        )));
    }
    remove_if_present(&paths.socket)?;
    let mut command = Command::new(program);
    command
        .arg("--tun=userspace-networking")
        .arg(format!("--socket={}", paths.socket.display()))
        .arg(format!("--statedir={}", paths.state.display()))
        .arg(format!(
            "--state={}",
            paths.state.join("tailscaled.state").display()
        ))
        .arg(format!("--socks5-server=127.0.0.1:{}", config.socks_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let parent_pid = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            libc::umask(0o077);
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if !persistent {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent may have exited between fork and prctl.
                if libc::getppid() != parent_pid {
                    return Err(std::io::Error::other("Tailscale supervisor exited"));
                }
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|_| invalid(format!("{DOWN}: tailscaled could not start")))?;
    let start = match process_identity(child.id()) {
        Some(start) => start,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid(format!("{DOWN}: tailscaled exited during startup")));
        }
    };
    let boot = match boot_identity() {
        Ok(boot) => boot,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let rec = Record {
        pid: child.id(),
        start,
        boot,
        persistent,
    };
    if let Err(e) = save_record(paths, &rec) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok(child)
}

fn command(root: &Path, mode: Mode) -> Result<Command> {
    let mut command = Command::new(binary(root, "tailscale")?);
    if mode == Mode::Managed {
        command.arg(format!("--socket={}", Paths::new(root).socket.display()));
    }
    command.stdin(Stdio::null());
    Ok(command)
}

fn cli_status(root: &Path, mode: Mode) -> Result<Value> {
    let output = command(root, mode)?
        .args(["status", "--json"])
        .output()
        .map_err(|_| invalid(format!("{DOWN}: tailscale status unavailable")))?;
    if !output.status.success() {
        return Err(invalid(format!(
            "{DOWN}: tailscale cannot reach its daemon"
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| invalid(format!("{DOWN}: tailscale returned invalid status JSON")))
}

fn ready(root: &Path, paths: &Paths) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(rec) = record(paths)? {
            if !live(&rec, paths) {
                return Err(invalid(format!("{DOWN}: tailscaled exited")));
            }
        } else {
            return Err(invalid(format!("{DOWN}: no owned sidecar")));
        }
        socket_permissions(paths)?;
        if let Ok(value) = cli_status(root, Mode::Managed) {
            // The daemon may have bound its socket since the call above, and
            // the next full supervision pass can be SUPERVISE_INTERVAL away.
            socket_permissions(paths)?;
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(invalid(format!(
                "{DOWN}: private daemon did not become ready"
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn logged_in(status: &Value) -> bool {
    status.get("BackendState").and_then(Value::as_str) == Some("Running")
}

fn show_login(status: &Value, output: Option<&Output>) {
    let text = output.map(|o| {
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    });
    let url = status
        .get("AuthURL")
        .and_then(Value::as_str)
        .filter(|s| s.starts_with("https://login.tailscale.com/"))
        .or_else(|| text.as_deref().and_then(login_url));
    if let Some(url) = url {
        print_login(url);
    }
}

fn login_url(text: &str) -> Option<&str> {
    text.split_whitespace()
        .map(|s| s.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | '}' | ']')))
        .find(|s| s.starts_with("https://login.tailscale.com/"))
}

fn print_login(url: &str) {
    println!("Tailscale login: {url}");
    if let Ok(qr) = qrcode::QrCode::new(url.as_bytes()) {
        println!(
            "{}",
            qr.render::<qrcode::render::unicode::Dense1x2>().build()
        );
    }
    let _ = std::io::stdout().flush();
}

fn login_output(reader: impl Read, announce: bool, printed: Arc<AtomicBool>) -> Vec<u8> {
    let mut output = Vec::new();
    for line in BufReader::new(reader).split(b'\n') {
        let Ok(line) = line else {
            break;
        };
        if announce {
            if let Some(url) = login_url(&String::from_utf8_lossy(&line)) {
                if !printed.swap(true, Ordering::Relaxed) {
                    print_login(url);
                }
            }
        }
        output.extend_from_slice(&line);
        output.push(b'\n');
    }
    output
}

fn login(
    root: &Path,
    config: &Settings,
    auth: Option<&Path>,
    announce: bool,
) -> Result<Option<String>> {
    let mut cmd = command(root, config.mode)?;
    cmd.arg("up")
        .arg("--json")
        .arg(format!("--hostname={}", config.hostname))
        .arg("--timeout=5s");
    if let Some(path) = auth {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(invalid(
                "Tailscale auth key must be a user-owned regular file",
            ));
        }
    }
    let auth = auth.map(fs::canonicalize).transpose()?;
    if let Some(path) = &auth {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(invalid(
                "Tailscale auth key must be a user-owned regular file",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        cmd.arg(format!("--auth-key=file:{}", path.display()));
    }
    // Never expose command output on an auth-key failure: even a malicious
    // helper echoing its input must not leak the credential into our logs.
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|_| invalid(format!("{DOWN}: tailscale up failed")))?;
    let printed = Arc::new(AtomicBool::new(false));
    let announce = announce && auth.is_none();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out_printed = Arc::clone(&printed);
    let err_printed = Arc::clone(&printed);
    let out = thread::spawn(move || login_output(stdout, announce, out_printed));
    let err = thread::spawn(move || login_output(stderr, announce, err_printed));
    let exit = child.wait()?;
    let output = Output {
        status: exit,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    };
    let status = cli_status(root, config.mode)?;
    if output.status.success() && logged_in(&status) {
        if let Some(path) = auth {
            fs::remove_file(path)?;
        }
        Ok(None)
    } else {
        if auth.is_none() && announce && !printed.load(Ordering::Relaxed) {
            show_login(&status, Some(&output));
        }
        if auth.is_none() && !announce {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if let Some(url) = status
                .get("AuthURL")
                .and_then(Value::as_str)
                .filter(|url| url.starts_with("https://login.tailscale.com/"))
                .or_else(|| login_url(&text))
            {
                return Ok(Some(url.to_owned()));
            }
        }
        Err(invalid(format!(
            "{LOGIN}: run kanban net up to authenticate"
        )))
    }
}

/// Longest gap between full supervision passes. A pass parses the config,
/// re-secures the state tree and runs `tailscale status`, and the TUI ticks
/// every second on its UI thread; in between, a tick only `stat`s the inputs
/// and checks the owned sidecar's `/proc` entry, so an edit, a handover or a
/// crashed daemon still triggers a pass on the next tick.
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(15);

/// Modification times of everything a pass reads that another process may
/// change: the global config, node identity, the sidecar record and the
/// `net down` marker.
type NetStamp = [Option<std::time::SystemTime>; 4];

struct Supervised {
    at: Instant,
    stamp: NetStamp,
    /// The sidecar this manager owned after the pass, if any.
    sidecar: Option<Record>,
}

pub struct Manager {
    root: PathBuf,
    paths: Paths,
    owner: Option<File>,
    child: Option<Child>,
    supervised: Option<Supervised>,
}

impl Manager {
    pub fn new(root: &Path) -> Result<Self> {
        let config = settings(root)?;
        let root = fs::canonicalize(root)?;
        let paths = Paths::new(&root);
        let mut owner = None;
        if config.mode == Mode::Managed {
            paths.prepare()?;
            let file = private_file(&paths.state.join("owner.lock"))?;
            match file.try_lock_exclusive() {
                Ok(()) => owner = Some(file),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Self {
            root,
            paths,
            owner,
            child: None,
            supervised: None,
        })
    }

    /// Keep the managed sidecar running. Cheap unless something changed or
    /// [`SUPERVISE_INTERVAL`] passed; see there.
    pub fn tick(&mut self) -> Result<()> {
        if self.supervised.as_ref().is_some_and(|last| {
            last.at.elapsed() < SUPERVISE_INTERVAL
                && last.stamp == self.stamp()
                && last
                    .sidecar
                    .as_ref()
                    .is_none_or(|rec| live(rec, &self.paths))
        }) {
            return Ok(());
        }
        let result = self.supervise();
        self.supervised = Some(Supervised {
            at: Instant::now(),
            stamp: self.stamp(),
            sidecar: self
                .owner
                .as_ref()
                .and_then(|_| record(&self.paths).ok().flatten()),
        });
        result
    }

    fn stamp(&self) -> NetStamp {
        [
            self.root.join("config.yaml"),
            self.root.join("sync/node.yaml"),
            self.paths.record(),
            self.paths.stopped(),
        ]
        .map(|path| fs::metadata(path).and_then(|m| m.modified()).ok())
    }

    fn supervise(&mut self) -> Result<()> {
        let config = settings(&self.root)?;
        if config.mode != Mode::Managed {
            self.stop_owned()?;
            self.owner = None;
            return Ok(());
        }
        self.paths.prepare()?;
        if self.owner.is_none() {
            let owner = private_file(&self.paths.state.join("owner.lock"))?;
            match owner.try_lock_exclusive() {
                Ok(()) => self.owner = Some(owner),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let value = status(&self.root)?;
                    return if logged_in(&value) {
                        Ok(())
                    } else {
                        Err(invalid(LOGIN))
                    };
                }
                Err(e) => return Err(e.into()),
            }
        }
        let _operation = self.paths.operation()?;
        if self.paths.stopped().exists() {
            return Ok(());
        }
        if let Some(child) = &mut self.child {
            if child.try_wait()?.is_some() {
                self.child = None;
            }
        }
        let previous = record(&self.paths)?;
        // A detached `net up` is handed over to the lifetime supervisor.
        // Restart once to attach parent-death cleanup without losing login state.
        if previous
            .as_ref()
            .is_some_and(|rec| rec.persistent && live(rec, &self.paths))
        {
            stop_record(&self.paths, previous.as_ref().unwrap())?;
        }
        if !previous
            .as_ref()
            .is_some_and(|rec| !rec.persistent && live(rec, &self.paths))
        {
            self.child = Some(daemon(&self.root, &config, &self.paths, false)?);
        }
        let value = ready(&self.root, &self.paths)?;
        secure_tree(&self.paths.state)?;
        if logged_in(&value) {
            Ok(())
        } else if let Some(auth) = config.auth_key_file.as_deref().filter(|p| p.exists()) {
            login(&self.root, &config, Some(auth), false).map(|_| ())
        } else {
            Err(invalid(format!(
                "{LOGIN}: run kanban net up to authenticate"
            )))
        }
    }

    fn stop_owned(&mut self) -> Result<()> {
        if self.owner.is_none() || !self.paths.state.exists() {
            return Ok(());
        }
        let _operation = self.paths.operation()?;
        if let Some(rec) = record(&self.paths)? {
            stop_record(&self.paths, &rec)?;
        }
        if let Some(mut child) = self.child.take() {
            // Reap the stopped child without waiting on a replaced process.
            let _ = child.try_wait();
        }
        Ok(())
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        let _ = self.stop_owned();
    }
}

pub fn up(root: &Path, auth: Option<&Path>) -> Result<()> {
    up_inner(root, auth, true).map(|_| ())
}

/// TUI login: no terminal output, and the one-time key is never configuration.
/// The private temporary file is removed on success, failure, or cancellation.
pub fn connect(root: &Path, key: &str) -> Result<Option<String>> {
    let paths = Paths::new(root);
    paths.prepare()?;
    let mut auth = if key.is_empty() {
        None
    } else {
        let mut file = tempfile::NamedTempFile::new_in(&paths.net)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(key.as_bytes())?;
        file.as_file().sync_all()?;
        Some(file)
    };
    let result = up_inner(root, auth.as_ref().map(|f| f.path()), false);
    if let Some(file) = auth.take() {
        let _ = file.close();
    }
    result
}

fn up_inner(root: &Path, auth: Option<&Path>, announce: bool) -> Result<Option<String>> {
    let root = fs::canonicalize(root)?;
    let config = settings(&root)?;
    if config.mode == Mode::Off {
        return Err(invalid(
            "net.tailscale.mode is off; select managed or system first",
        ));
    }
    if config.mode == Mode::System {
        return Err(invalid(
            "system Tailscale is read-only; manage login with the installed tailscale CLI",
        ));
    }
    let paths = Paths::new(&root);
    let _operation = if config.mode == Mode::Managed {
        paths.prepare()?;
        Some(paths.operation()?)
    } else {
        None
    };
    if config.mode == Mode::Managed {
        remove_if_present(&paths.stopped())?;
        match record(&paths)? {
            Some(rec) if live(&rec, &paths) => {}
            _ => {
                drop(daemon(&root, &config, &paths, true)?);
            }
        }
        ready(&root, &paths)?;
    }
    login(
        &root,
        &config,
        auth.or(config.auth_key_file.as_deref().filter(|p| p.exists())),
        announce,
    )
}

pub fn down(root: &Path) -> Result<()> {
    let root = fs::canonicalize(root)?;
    let config = settings(&root)?;
    if config.mode == Mode::System {
        return Err(invalid(
            "system Tailscale is read-only; manage it with the installed tailscale CLI",
        ));
    }
    let paths = Paths::new(&root);
    // A managed sidecar can still exist after switching the config to off.
    if config.mode == Mode::Managed || paths.record().exists() {
        paths.prepare()?;
        let _operation = paths.operation()?;
        write_private(&paths.stopped(), b"stopped\n")?;
        if let Some(rec) = record(&paths)? {
            stop_record(&paths, &rec)?;
        }
        return Ok(());
    }
    Ok(())
}

pub fn logout(root: &Path) -> Result<()> {
    let root = fs::canonicalize(root)?;
    let config = settings(&root)?;
    if config.mode == Mode::Off {
        return Err(invalid("net.tailscale.mode is off"));
    }
    if config.mode == Mode::System {
        return Err(invalid(
            "system Tailscale is read-only; manage login with the installed tailscale CLI",
        ));
    }
    let paths = Paths::new(&root);
    let _operation = if config.mode == Mode::Managed {
        paths.prepare()?;
        Some(paths.operation()?)
    } else {
        None
    };
    let output = command(&root, config.mode)?.arg("logout").output()?;
    if !output.status.success() {
        return Err(invalid(format!("{DOWN}: tailscale logout failed")));
    }
    Ok(())
}

pub fn status(root: &Path) -> Result<Value> {
    let root = fs::canonicalize(root)?;
    let config = settings(&root)?;
    if config.mode == Mode::Off {
        return Ok(json!({"mode":"off", "BackendState":"Disabled"}));
    }
    let paths = Paths::new(&root);
    if config.mode == Mode::Managed {
        if !record(&paths)?
            .as_ref()
            .is_some_and(|rec| live(rec, &paths))
        {
            return Err(invalid(format!("{DOWN}: private sidecar is not running")));
        }
        socket_permissions(&paths)?;
    }
    let mut value = cli_status(&root, config.mode)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid(format!("{DOWN}: invalid status object")))?;
    object.insert("mode".into(), json!(config.mode.name()));
    if config.mode == Mode::Managed {
        object.insert("socket".into(), json!(paths.socket));
        object.insert("socks_port".into(), json!(config.socks_port));
    }
    Ok(value)
}

/// Tailnet IPs and fully-qualified MagicDNS names. Bare hostnames are not
/// classified: they may be LAN names and must never be sent through SOCKS.
pub fn is_tailnet_address(address: &str) -> bool {
    let address = address.split_once("://").map_or(address, |(_, s)| s);
    let authority = address.split('/').next().unwrap_or(address);
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or(bracketed)
    } else if authority.parse::<IpAddr>().is_ok() {
        authority
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]),
        Ok(IpAddr::V6(ip)) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
        Err(_) => {
            let host = host.trim_end_matches('.');
            host.is_ascii()
                && host.len() > 7
                && host[host.len() - 7..].eq_ignore_ascii_case(".ts.net")
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        }
    }
}

pub fn proxy_for(root: &Path, address: &str) -> Result<Option<String>> {
    let config = settings(root)?;
    if config.mode != Mode::Managed || !is_tailnet_address(address) {
        return Ok(None);
    }
    let value = status(root)?;
    if !logged_in(&value) {
        return Err(invalid(format!(
            "{LOGIN}: run kanban net up to authenticate"
        )));
    }
    Ok(Some(format!("socks5h://127.0.0.1:{}", config.socks_port)))
}

/// Download the official stable static Linux build through the existing curl
/// transport, verify SHA-256, then extract only the two exact binary members
/// into private staged files. Archive paths never write to the filesystem.
pub fn install(root: &Path) -> Result<()> {
    if std::env::consts::OS != "linux" {
        return Err(invalid("Tailscale static install supports Linux only"));
    }
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => {
            return Err(invalid(
                "Tailscale static install supports amd64 and arm64 only",
            ));
        }
    };
    let root = fs::canonicalize(root)?;
    let paths = Paths::new(&root);
    paths.prepare()?;
    let _operation = paths.operation()?;
    let staging = tempfile::Builder::new()
        .prefix("install-")
        .tempdir_in(&paths.net)?;
    private_dir(staging.path())?;
    let latest = staging.path().join("index.json");
    http_download("https://pkgs.tailscale.com/stable/?mode=json", &latest)
        .map_err(|e| invalid(format!("Tailscale release lookup failed: {e}")))?;
    let index: Value = serde_json::from_slice(&fs::read(&latest)?)
        .map_err(|_| invalid("Tailscale release index is invalid"))?;
    let filename = index
        .get("Tarballs")
        .and_then(|v| v.get(arch))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Tailscale release index lacks the requested architecture"))?;
    let prefix = "tailscale_";
    let suffix = format!("_{arch}.tgz");
    let version = filename
        .strip_prefix(prefix)
        .and_then(|f| f.strip_suffix(&suffix))
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
        .ok_or_else(|| invalid("Tailscale release index contains an unsafe archive name"))?;
    let url = format!("https://pkgs.tailscale.com/stable/{filename}");
    let archive = staging.path().join("archive.tgz");
    let checksum = staging.path().join("archive.sha256");
    http_download(&url, &archive)
        .map_err(|e| invalid(format!("Tailscale download failed: {e}")))?;
    http_download(&format!("{url}.sha256"), &checksum)
        .map_err(|e| invalid(format!("Tailscale checksum download failed: {e}")))?;
    let expected = fs::read_to_string(&checksum)?
        .split_whitespace()
        .next()
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| invalid("Tailscale checksum file is invalid"))?
        .to_ascii_lowercase();
    let mut file = File::open(&archive)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if format!("{:x}", hasher.finalize()) != expected {
        return Err(invalid(
            "Tailscale archive SHA-256 mismatch; nothing installed",
        ));
    }
    let bin = paths.net.join("bin");
    private_dir(&bin)?;
    // Stream entries, never unpack paths. Exact names, regular file type,
    // and uniqueness are required even after checksum verification.
    let decoder = flate2::read::GzDecoder::new(File::open(&archive)?);
    let mut archive = tar::Archive::new(decoder);
    let names = ["tailscale", "tailscaled"];
    let members = names.map(|name| format!("tailscale_{version}_{arch}/{name}"));
    let mut staged: [Option<tempfile::NamedTempFile>; 2] = [None, None];
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path_bytes();
        let index = members
            .iter()
            .position(|member| path.as_ref() == member.as_bytes());
        let Some(index) = index else {
            continue;
        };
        if staged[index].is_some() || !entry.header().entry_type().is_file() || entry.size() == 0 {
            return Err(invalid(format!(
                "Tailscale archive contains an unsafe or duplicate {} member",
                names[index]
            )));
        }
        let mut file = tempfile::NamedTempFile::new_in(&bin)?;
        std::io::copy(&mut entry, file.as_file_mut())?;
        file.as_file().sync_all()?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
        staged[index] = Some(file);
    }
    if staged.iter().any(Option::is_none) {
        return Err(invalid(
            "Tailscale archive lacks the exact tailscale/tailscaled binary pair",
        ));
    }
    for (name, file) in names.into_iter().zip(staged) {
        file.unwrap().persist(bin.join(name)).map_err(|e| e.error)?;
    }
    Ok(())
}
