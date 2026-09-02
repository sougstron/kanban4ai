//! Read-only update check against GitHub Releases: the "is there a newer
//! version?" path that every later updater piece (download, CLI command, TUI
//! banner) reads.
//!
//! One "provider", the public unauthenticated releases API, queried through
//! the same curl-config helper the provider-limits fetches use
//! ([`crate::core::http`]). The check is best effort everywhere: a network
//! failure, a missing curl, or a malformed remote tag can only ever mean "no
//! answer" / "no update" — never an error the board cannot draw.
//!
//! Runtime state (when we last checked, what we saw, what the user already
//! dismissed) lives in `<store>/update-status.json`, kept out of
//! `config.yaml` exactly like `limits.json`: settings vs. state. The check
//! cadence is machine-wide, so its settings live in [`GlobalConfig`]'s
//! `updates:` section, not in any board's config.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::global::DEFAULT_CHECK_INTERVAL_HOURS;
use crate::core::http::{HttpError, http_download, http_get_json};
use crate::core::notifier::which;
use crate::core::project::{ProjectStore, store_root};
use crate::core::storage::atomic_write_text;

/// The public, unauthenticated GitHub API endpoint for the newest release.
/// The anonymous 60-requests-per-hour rate limit is irrelevant at a
/// TTL-cached, once-a-day-per-machine cadence.
pub const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/sougstron/kanban4ai/releases/latest";

/// Runtime update state at the store root, next to `limits.json`.
pub const STATUS_FILE: &str = "update-status.json";

/// Why a check could not produce a status. Both variants are informational:
/// callers degrade to "no answer", they never surface as a hard error.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateError {
    /// curl is missing, the network failed, or the API answered non-2xx.
    Network(String),
    /// The API answered, but the payload is not a usable release.
    Parse(String),
}

/// The version compiled into this binary.
pub fn installed_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The Rust target triple for `(arch, os)`, or `None` when the release
/// workflow builds no archive for it. Only the two triples
/// `.github/workflows/release.yml` builds exist; anything else fails closed
/// rather than guessing a near-miss triple.
pub fn triple_for(arch: &str, os: &str) -> Option<&'static str> {
    match (arch, os) {
        ("x86_64", "linux") => Some("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// The release triple for the running platform, or `None` when there is no
/// release build for it ("no build for this platform").
pub fn target_triple() -> Option<&'static str> {
    triple_for(std::env::consts::ARCH, std::env::consts::OS)
}

/// Archive and checksum asset names a release publishes for `triple`, derived
/// from the release's `tag_name` exactly as release.yml names them — the tag
/// is the git ref name, so it carries the leading `v`
/// (`kanban4ai-v0.5.0-x86_64-unknown-linux-gnu.tar.gz`). The names are never
/// reconstructed from the crate version.
fn asset_names(tag: &str, triple: &str) -> (String, String) {
    let archive = format!("kanban4ai-{tag}-{triple}.tar.gz");
    (archive.clone(), format!("{archive}.sha256"))
}

/// The archive and checksum asset names for the running platform's release
/// build, or `None` when this platform has none.
pub fn target_asset_names(tag: &str) -> Option<(String, String)> {
    Some(asset_names(tag, target_triple()?))
}

/// What the last check learned. Persisted to `<store>/update-status.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// Unix seconds of the check that produced this status.
    pub checked_at: i64,
    /// `tag_name` without the leading `v`, for display and comparison.
    pub latest_version: String,
    /// The raw `tag_name`, including its leading `v`.
    pub tag: String,
    /// Download URL of this platform's archive; `None` when the running
    /// platform has no release build (fail closed — never guess a triple).
    pub asset_url: Option<String>,
    /// Download URL of the archive's `.sha256` sibling.
    pub checksum_url: Option<String>,
    /// The release page, where the notes render.
    pub notes_url: String,
    /// Unix seconds of the release's `published_at`, when the payload carried
    /// a parseable one; the Settings row renders its age. `None` for statuses
    /// written before the field existed and for unusable remote timestamps.
    #[serde(default)]
    pub published_at: Option<i64>,
    /// The newest version the user has already seen and dismissed, so the
    /// one-time banner does not repeat for it. Survives later checks.
    #[serde(default)]
    pub dismissed_version: Option<String>,
}

/// Parse `major.minor.patch`, tolerating a leading `v` and surrounding
/// whitespace. Anything else — fewer or more than three parts, non-numeric
/// parts, a pre-release suffix — is `None`, so a malformed remote tag can
/// only ever compare as "no update", never brick the check.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().strip_prefix('v').unwrap_or(text.trim()).trim();
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// Whether `candidate` is strictly newer than `current`. A `candidate` that
/// does not parse is never newer (fail closed); neither is a `current` that
/// does not, because the comparison would be meaningless.
pub fn version_is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// Whether the checked release is newer than the running binary.
pub fn is_update_available(status: &UpdateStatus) -> bool {
    version_is_newer(&status.latest_version, installed_version())
}

/// Whether the on-open banner should show for this status: an update is
/// available **and** this exact version has not been dismissed yet. A newer
/// release than the dismissed one reopens the banner.
pub fn banner_due(status: &UpdateStatus) -> bool {
    is_update_available(status)
        && status.dismissed_version.as_deref() != Some(status.latest_version.as_str())
}

/// Record that the user has seen (and the banner has shown) `version`, so it
/// never nags again but a newer tag reopens the banner. Best effort: without
/// a cached status there is nothing to mark, and the write never fails the
/// caller. Tests keep the change in memory only.
pub fn dismiss(version: &str) {
    let Some(status) = cached() else {
        return;
    };
    if status.dismissed_version.as_deref() == Some(version) {
        return;
    }
    let mut status = (*status).clone();
    status.dismissed_version = Some(version.to_string());
    store(Arc::new(status), !cfg!(test));
}

/// Whether a status checked more than `interval_hours` before `now` is due
/// for a re-check. A `checked_at` in the future (clock skew) stays fresh.
pub fn status_expired(status: &UpdateStatus, interval_hours: u64, now: i64) -> bool {
    let ttl_secs = i64::try_from(interval_hours)
        .unwrap_or(i64::MAX)
        .saturating_mul(3600);
    now.saturating_sub(status.checked_at) >= ttl_secs
}

/// Read a `releases/latest` payload onto a status for `triple`'s platform.
/// `None` when there is no usable release in it (missing or empty
/// `tag_name`). Asset URLs resolve by asset name against `assets[]`, so they
/// come from the payload rather than being guessed; a platform without a
/// build still learns the version, just without download URLs.
pub fn parse_release(value: &Value, triple: Option<&str>, now: i64) -> Option<UpdateStatus> {
    let tag = value.get("tag_name")?.as_str()?.trim().to_string();
    if tag.is_empty() {
        return None;
    }
    let (asset_url, checksum_url) = triple
        .map(|triple| {
            let (archive, checksum) = asset_names(&tag, triple);
            (
                asset_download_url(value, &archive),
                asset_download_url(value, &checksum),
            )
        })
        .unwrap_or((None, None));
    Some(UpdateStatus {
        checked_at: now,
        latest_version: tag.strip_prefix('v').unwrap_or(&tag).to_string(),
        tag,
        asset_url,
        checksum_url,
        notes_url: value
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        published_at: value
            .get("published_at")
            .and_then(Value::as_str)
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.timestamp()),
        dismissed_version: None,
    })
}

fn asset_download_url(value: &Value, name: &str) -> Option<String> {
    value
        .get("assets")?
        .as_array()?
        .iter()
        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|asset| asset.get("browser_download_url"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// cache + persistence
// ---------------------------------------------------------------------------

fn status_file() -> Option<PathBuf> {
    store_root().ok().map(|root| root.join(STATUS_FILE))
}

/// The most recent check result: the in-memory one, or the one persisted by
/// an earlier run so the UI has something to draw before the first check
/// returns. UI reads only ever hit this, never the network.
pub fn cached() -> Option<Arc<UpdateStatus>> {
    if let Some(status) = cache().lock().ok().and_then(|value| value.clone()) {
        return Some(status);
    }
    if DISK_LOADED.swap(true, Ordering::SeqCst) {
        return None;
    }
    // TUI render paths call this, so under cfg(test) the disk is never read:
    // the developer's real update-status.json must not leak into snapshots.
    if cfg!(test) {
        return None;
    }
    let status = Arc::new(read_status_file()?);
    store(Arc::clone(&status), false);
    Some(status)
}

fn read_status_file() -> Option<UpdateStatus> {
    let text = fs::read_to_string(status_file()?).ok()?;
    serde_json::from_str(&text).ok()
}

fn store(status: Arc<UpdateStatus>, persist: bool) {
    if let Ok(mut value) = cache().lock() {
        *value = Some(Arc::clone(&status));
    }
    if persist
        && let Some(path) = status_file()
        && let Ok(text) = serde_json::to_string(status.as_ref())
    {
        let _ = atomic_write_text(&path, &text);
    }
}

static CACHE: LazyLock<Mutex<Option<Arc<UpdateStatus>>>> = LazyLock::new(|| Mutex::new(None));
static DISK_LOADED: AtomicBool = AtomicBool::new(false);
static CHECKING: AtomicBool = AtomicBool::new(false);

/// The tests that touch the process-wide cache serialize on this lock;
/// cargo test runs module tests in parallel threads of one process. TUI tests
/// that seed the cache take it too, so their seeded state cannot leak into
/// (or be wiped by) another test's tick or render.
#[cfg(test)]
pub(crate) static CACHE_LOCK: Mutex<()> = Mutex::new(());

fn cache() -> &'static Mutex<Option<Arc<UpdateStatus>>> {
    &CACHE
}

/// The check TTL from the store's global config; the default when the store
/// or the config has nothing usable. The CLI's cache-or-blocking split gates
/// on the same value `check_latest` applies internally.
pub fn configured_interval_hours() -> u64 {
    store_root()
        .ok()
        .and_then(|root| ProjectStore::at(root).load_global_config().ok())
        .map(|config| config.update_check_interval_hours())
        .unwrap_or(DEFAULT_CHECK_INTERVAL_HOURS)
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// Check GitHub Releases for a newer version. Blocking: one request. A
/// status cached within `updates.check_interval_hours` is returned as-is
/// unless `force` (a check the user explicitly asked for). Failures are
/// [`UpdateError`] — informational, never fatal to the caller.
pub fn check_latest(force: bool) -> Result<Arc<UpdateStatus>, UpdateError> {
    // Read the cache (and apply the TTL gate) before the cfg(test) guard so
    // the gating stays unit-testable; the guard only covers the network.
    let previous = cached();
    if !force
        && let Some(status) = previous.as_ref()
        && !status_expired(status, configured_interval_hours(), now_secs())
    {
        return Ok(Arc::clone(status));
    }
    // Unit tests must never dial out: cargo test --locked stays network-free.
    if cfg!(test) {
        return Err(UpdateError::Network(
            "update check skipped: network disabled under cfg(test)".to_string(),
        ));
    }
    let headers = [
        // GitHub rejects API calls without a User-Agent.
        ("User-Agent", format!("kanban4ai/{}", installed_version())),
        ("Accept", "application/vnd.github+json".to_string()),
    ];
    let value = http_get_json(RELEASES_LATEST_URL, &headers).map_err(|err| match err {
        HttpError::Status(code) => UpdateError::Network(format!("HTTP {code}")),
        HttpError::Transport(message) => UpdateError::Network(message),
    })?;
    let mut status = parse_release(&value, target_triple(), now_secs())
        .ok_or_else(|| UpdateError::Parse("no usable release in payload".to_string()))?;
    // A fresh check must not resurrect a version the user already dismissed.
    status.dismissed_version = previous
        .as_ref()
        .and_then(|previous| previous.dismissed_version.clone());
    let status = Arc::new(status);
    store(Arc::clone(&status), true);
    Ok(status)
}

// ---------------------------------------------------------------------------
// apply: install-kind gate, download, verify, atomic replace
// ---------------------------------------------------------------------------

/// How the running binary was installed, decided by [`install_path_kind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `pacman -Qo` owns the binary — one of our AUR packages
    /// (kanban4ai, kanban4ai-bin, kanban4ai-git) or any other. Overwriting
    /// the file would desync pacman's file database, so self-update refuses
    /// and the package manager stays the only writer.
    PackageManaged(String),
    /// `cargo install`, `scripts/install.sh`, or a hand-copied binary:
    /// nothing else tracks the file, so self-update may replace it.
    Unmanaged,
}

/// Why an update could not be applied. Every variant renders the text a
/// caller shows verbatim ([`std::fmt::Display`]); none of them is ever a
/// silent skip, and none of them leaves the old binary touched.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyError {
    /// pacman owns the binary. `upgrade_command` is the command that updates
    /// it properly; self-update never downloads or writes for this install.
    PackageManaged {
        package: String,
        upgrade_command: String,
    },
    /// The install directory is not writable by the current user. The
    /// guidance is manual action — self-update never escalates with sudo.
    NotWritable { path: PathBuf, detail: String },
    /// The platform has no release build, or the status carries no download
    /// URLs for it. Fails closed before anything is fetched.
    NoBuild,
    /// A helper the apply path needs (curl, sha256sum, tar) is not on PATH.
    /// The checksum is never skipped because a tool is missing.
    MissingTool(&'static str),
    /// curl could not fetch an asset.
    Download(String),
    /// The downloaded archive does not match the published SHA-256.
    ChecksumMismatch { expected: String, actual: String },
    /// The published `.sha256` payload is not a `<64 hex> <name>` line.
    BadChecksumFile(String),
    /// sha256sum failed, or its output was unusable.
    Verify(String),
    /// tar failed, or the archive lacks the expected member.
    Extract(String),
    /// Filesystem error outside the archive pipeline.
    Io(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PackageManaged {
                package,
                upgrade_command,
            } => write!(
                f,
                "kanban4ai is owned by the pacman package '{package}'; self-update would \
                 desync the package database — update with `{upgrade_command}` instead"
            ),
            Self::NotWritable { path, detail } => write!(
                f,
                "cannot write to the install directory {} ({detail}); fix its permissions \
                 or reinstall manually — never run kanban4ai with sudo to self-update",
                path.display()
            ),
            Self::NoBuild => write!(f, "no release build is published for this platform"),
            Self::MissingTool(tool) => write!(
                f,
                "{tool} is required to download and verify an update but was not found on \
                 PATH; install it and retry — the checksum is never skipped"
            ),
            Self::Download(detail) => write!(f, "download failed: {detail}"),
            Self::ChecksumMismatch { expected, actual } => write!(
                f,
                "checksum mismatch: expected {expected}, got {actual}; the download was \
                 discarded and nothing was installed"
            ),
            Self::BadChecksumFile(detail) => write!(f, "unusable .sha256 payload: {detail}"),
            Self::Verify(detail) => write!(f, "checksum verification failed: {detail}"),
            Self::Extract(detail) => write!(f, "archive extraction failed: {detail}"),
            Self::Io(detail) => write!(f, "{detail}"),
        }
    }
}

/// What a successful [`apply_update`] did. Linux replaced the directory
/// entry under the running process, which keeps executing the old code from
/// its old inode until it exits — the caller must surface the restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedUpdate {
    /// The version now on disk (`latest_version` of the applied status).
    pub version: String,
    /// The binary file that was replaced.
    pub binary: PathBuf,
}

/// Whether pacman owns the running binary. `current_exe()` resolves through
/// the `kanban`/`kb` symlinks to the real file, so the probe sees what
/// pacman's database tracks. The probe is a hard gate, not a hint: a
/// package-managed binary must never be overwritten by self-update, even
/// when the directory happens to be writable.
pub fn install_path_kind() -> Result<InstallKind, ApplyError> {
    let exe = current_exe_path()?;
    if which("pacman").is_some()
        && let Some(package) = pacman_owner(&exe)
    {
        return Ok(InstallKind::PackageManaged(package));
    }
    Ok(InstallKind::Unmanaged)
}

/// The command that updates this install through its package manager, or
/// `None` when self-update may replace the binary directly (and when the
/// install could not be probed — the caller falls back to letting
/// [`apply_update`] report the exact problem). UIs show this instead of an
/// "update now" button that would only refuse.
pub fn package_upgrade_command() -> Option<String> {
    match install_path_kind() {
        Ok(InstallKind::PackageManaged(package)) => Some(upgrade_command_for(&package)),
        _ => None,
    }
}

fn current_exe_path() -> Result<PathBuf, ApplyError> {
    std::env::current_exe()
        .map_err(|err| ApplyError::Io(format!("cannot locate the running binary: {err}")))
}

fn pacman_owner(exe: &Path) -> Option<String> {
    pacman_owner_with("pacman", exe)
}

fn pacman_owner_with(program: &str, exe: &Path) -> Option<String> {
    let mut command = Command::new(program);
    command.arg("-Qo").arg(exe);
    // pacman localizes "-Qo" ("принадлежит" on a ru locale); parse_owner
    // reads the English form, so pin the child's locale.
    command.env("LC_ALL", "C");
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_owner(&String::from_utf8_lossy(&output.stdout))
}

/// `/usr/bin/kanban4ai is owned by kanban4ai-bin 0.5.1-1` → `kanban4ai-bin`.
fn parse_owner(text: &str) -> Option<String> {
    text.split(" is owned by ")
        .nth(1)?
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// The three release channels that ship the binary as AUR packages; they are
/// not in the official repos, so their upgrade goes through an AUR helper.
fn is_aur_package(package: &str) -> bool {
    matches!(package, "kanban4ai" | "kanban4ai-bin" | "kanban4ai-git")
}

fn aur_helper() -> &'static str {
    if which("yay").is_some() {
        "yay"
    } else if which("paru").is_some() {
        "paru"
    } else {
        "yay"
    }
}

/// The upgrade command that replaces self-update for a package-managed
/// install: an AUR helper for our AUR packages, pacman for anything else.
fn upgrade_command_for(package: &str) -> String {
    if is_aur_package(package) {
        format!("{} -S {package}", aur_helper())
    } else {
        format!("sudo pacman -Syu {package}")
    }
}

/// The single payload member inside a release archive, exactly where
/// release.yml packs it: a top-level `kanban4ai-<tag>-<target>/` directory
/// holding the binary next to README.md, LICENSE, and the unit file.
fn archive_member_path(tag: &str, triple: &str) -> String {
    format!("kanban4ai-{tag}-{triple}/kanban4ai")
}

/// [`archive_member_path`] for the running platform, rejecting tags that
/// would move the member out of the top-level directory. The tag comes from
/// the releases API, so it is untrusted input.
fn archive_member_for(tag: &str) -> Result<String, ApplyError> {
    let triple = target_triple().ok_or(ApplyError::NoBuild)?;
    let member = archive_member_path(tag, triple);
    let safe = !member.starts_with('/')
        && member
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if !safe {
        return Err(ApplyError::Extract(format!(
            "unsafe archive member path: {member}"
        )));
    }
    Ok(member)
}

/// The hex digest from a `sha256sum`-format line (`<64 hex>␠␠<name>`); the
/// file name is ignored because the download is staged under a temp name.
fn parse_expected_checksum(text: &str) -> Option<String> {
    let token = text.lines().next()?.split_whitespace().next()?;
    (token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| token.to_ascii_lowercase())
}

fn sha256_of(path: &Path) -> Result<String, ApplyError> {
    sha256_with("sha256sum", path)
}

fn sha256_with(program: &str, path: &Path) -> Result<String, ApplyError> {
    let output = Command::new(program)
        .arg(path)
        .output()
        .map_err(|err| ApplyError::Verify(format!("{program} unavailable: {err}")))?;
    if !output.status.success() {
        return Err(ApplyError::Verify(stderr_tail(&output.stderr, program)));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_string)
        .ok_or_else(|| ApplyError::Verify(format!("{program} produced no digest")))
}

/// Extract one member to memory (`tar -xO`): no archive path ever touches
/// the filesystem, so even a hostile member name cannot write anywhere.
fn extract_member(archive: &Path, member: &str) -> Result<Vec<u8>, ApplyError> {
    extract_member_with("tar", archive, member)
}

fn extract_member_with(program: &str, archive: &Path, member: &str) -> Result<Vec<u8>, ApplyError> {
    let output = Command::new(program)
        .args(["-xzf"])
        .arg(archive)
        .arg("-O")
        .arg(member)
        .output()
        .map_err(|err| ApplyError::Extract(format!("{program} unavailable: {err}")))?;
    if !output.status.success() {
        return Err(ApplyError::Extract(stderr_tail(&output.stderr, program)));
    }
    if output.stdout.is_empty() {
        return Err(ApplyError::Extract(format!(
            "archive member {member:?} is empty"
        )));
    }
    Ok(output.stdout)
}

fn stderr_tail(stderr: &[u8], program: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    let last = text.lines().next_back().unwrap_or_default().trim();
    if last.is_empty() {
        format!("{program} failed")
    } else {
        last.to_string()
    }
}

/// The apply path writes only where it already may. The probe is creating a
/// temp file in the directory, which answers "can this uid write here" more
/// honestly than permission bits (group bits, ACLs, read-only mounts); any
/// failure becomes [`ApplyError::NotWritable`] — guidance, never sudo.
fn ensure_dir_writable(dir: &Path) -> Result<(), ApplyError> {
    match staged_temp(dir) {
        Ok(temp) => {
            drop(temp);
            Ok(())
        }
        Err(err) => Err(ApplyError::NotWritable {
            path: dir.to_path_buf(),
            detail: err,
        }),
    }
}

fn staged_temp(dir: &Path) -> Result<tempfile::NamedTempFile, String> {
    tempfile::Builder::new()
        .prefix(".tmp_")
        .tempfile_in(dir)
        .map_err(|err| format!("{}: {err}", dir.display()))
}

/// Verify → extract → stage → atomic rename. `expected` is the hex digest
/// parsed from the published `.sha256`; nothing is touched until the
/// downloaded archive matches it. The staged binary is a temp file in `dir`
/// — the same filesystem as `exe`, so the final rename is atomic, matching
/// the atomic-write convention the board data paths already follow. Every
/// failure path drops its temp file and leaves the existing binary alone.
fn install_from_archive(
    dir: &Path,
    exe: &Path,
    archive: &Path,
    expected: &str,
    member: &str,
) -> Result<(), ApplyError> {
    let actual = sha256_of(archive)?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(ApplyError::ChecksumMismatch {
            expected: expected.to_string(),
            actual,
        });
    }
    let payload = extract_member(archive, member)?;
    let staged = staged_temp(dir).map_err(ApplyError::Io)?;
    staged
        .as_file()
        .write_all(&payload)
        .map_err(|err| ApplyError::Io(format!("cannot write the staged binary: {err}")))?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))
        .map_err(|err| ApplyError::Io(format!("cannot chmod the staged binary: {err}")))?;
    // rename(2) over the running binary: Linux swaps the directory entry and
    // the old process keeps its inode, so no restart trick is needed — but
    // the old code runs until the process exits, hence the restart note.
    staged
        .persist(exe)
        .map_err(|err| ApplyError::Io(format!("cannot replace {}: {err}", exe.display())))?;
    Ok(())
}

/// Download, verify, and atomically replace the running binary with the
/// release described by `status`. Only unmanaged installs proceed: a
/// package-managed binary is refused with its owning package and upgrade
/// command, and an unwritable install directory stops the apply with
/// guidance instead of reaching for sudo. Both assets download to temp files
/// next to `current_exe()` so the final rename stays on one filesystem; on
/// any failure the temps are gone and the old binary is untouched.
pub fn apply_update(status: &UpdateStatus) -> Result<AppliedUpdate, ApplyError> {
    if let InstallKind::PackageManaged(package) = install_path_kind()? {
        return Err(ApplyError::PackageManaged {
            upgrade_command: upgrade_command_for(&package),
            package,
        });
    }
    let member = archive_member_for(&status.tag)?;
    let asset_url = status.asset_url.as_deref().ok_or(ApplyError::NoBuild)?;
    let checksum_url = status.checksum_url.as_deref().ok_or(ApplyError::NoBuild)?;
    let exe = current_exe_path()?;
    let dir = exe
        .parent()
        .ok_or_else(|| {
            ApplyError::Io(format!("binary has no parent directory: {}", exe.display()))
        })?
        .to_path_buf();
    ensure_dir_writable(&dir)?;
    for tool in ["curl", "sha256sum", "tar"] {
        if which(tool).is_none() {
            return Err(ApplyError::MissingTool(tool));
        }
    }
    let archive = staged_temp(&dir).map_err(ApplyError::Io)?;
    let checksum = staged_temp(&dir).map_err(ApplyError::Io)?;
    http_download(asset_url, archive.path())
        .map_err(|err| ApplyError::Download(format!("{asset_url}: {err}")))?;
    http_download(checksum_url, checksum.path())
        .map_err(|err| ApplyError::Download(format!("{checksum_url}: {err}")))?;
    let checksum_text = fs::read_to_string(checksum.path())
        .map_err(|err| ApplyError::BadChecksumFile(err.to_string()))?;
    let expected = parse_expected_checksum(&checksum_text)
        .ok_or_else(|| ApplyError::BadChecksumFile(checksum_text.trim().to_string()))?;
    install_from_archive(&dir, &exe, archive.path(), &expected, &member)?;
    Ok(AppliedUpdate {
        version: status.latest_version.clone(),
        binary: exe,
    })
}

#[cfg(test)]
use std::sync::atomic::AtomicU32;

/// Background checks ever started; exists only so the test suite can prove
/// none spawn under `cfg(test)`.
#[cfg(test)]
static SPAWNED_CHECKS: AtomicU32 = AtomicU32::new(0);

/// Kick `check_latest(false)` off on a background thread: spawn only when
/// nothing fresh is cached and no check is in flight, so repeated calls (TUI
/// startup, every screen open) stay free. The caller never blocks and keeps
/// reading [`cached`]`()` until the new status lands. Honors the store's
/// `updates.check_on_open` (default on): the TUI's on-open hook calls this
/// unconditionally, and this is where the switch no-ops it.
pub fn warm_check() {
    // Unit tests must never spawn the check thread: cargo test never dials
    // out, and no monitor launched from a test can outlive it usefully.
    if cfg!(test) {
        return;
    }
    if !check_on_open_enabled() {
        return;
    }
    if let Some(status) = cached()
        && !status_expired(&status, configured_interval_hours(), now_secs())
    {
        return;
    }
    if CHECKING.swap(true, Ordering::SeqCst) {
        return;
    }
    #[cfg(test)]
    SPAWNED_CHECKS.fetch_add(1, Ordering::SeqCst);
    thread::spawn(|| {
        let _ = check_latest(false);
        CHECKING.store(false, Ordering::SeqCst);
    });
}

/// `updates.check_on_open` from the store's global config; the default
/// (enabled) when the store or the config has nothing usable.
fn check_on_open_enabled() -> bool {
    store_root()
        .ok()
        .and_then(|root| ProjectStore::at(root).load_global_config().ok())
        .map(|config| config.update_check_on_open())
        .unwrap_or(true)
}

/// Whether a background check is running.
pub fn check_in_flight() -> bool {
    CHECKING.load(Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn spawned_check_threads() -> u32 {
    SPAWNED_CHECKS.load(Ordering::SeqCst)
}

/// Replace the in-memory status cache and suppress disk loads, so tests that
/// exercise the cache paths stay hermetic (they never read the developer's
/// real store) and never depend on each other's leftovers.
#[cfg(test)]
pub(crate) fn force_cache(status: Option<Arc<UpdateStatus>>) {
    *cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = status;
    DISK_LOADED.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RELEASE_JSON: &str = r###"{
  "tag_name": "v0.6.0",
  "name": "v0.6.0",
  "body": "## Changes\n- sample body, parsed but not stored",
  "html_url": "https://github.com/sougstron/kanban4ai/releases/tag/v0.6.0",
  "assets": [
    {"name": "kanban4ai-v0.6.0-x86_64-unknown-linux-gnu.tar.gz",
     "browser_download_url": "https://github.com/sougstron/kanban4ai/releases/download/v0.6.0/kanban4ai-v0.6.0-x86_64-unknown-linux-gnu.tar.gz"},
    {"name": "kanban4ai-v0.6.0-x86_64-unknown-linux-gnu.tar.gz.sha256",
     "browser_download_url": "https://github.com/sougstron/kanban4ai/releases/download/v0.6.0/kanban4ai-v0.6.0-x86_64-unknown-linux-gnu.tar.gz.sha256"},
    {"name": "kanban4ai-v0.6.0-aarch64-unknown-linux-gnu.tar.gz",
     "browser_download_url": "https://github.com/sougstron/kanban4ai/releases/download/v0.6.0/kanban4ai-v0.6.0-aarch64-unknown-linux-gnu.tar.gz"},
    {"name": "kanban4ai-v0.6.0-aarch64-unknown-linux-gnu.tar.gz.sha256",
     "browser_download_url": "https://github.com/sougstron/kanban4ai/releases/download/v0.6.0/kanban4ai-v0.6.0-aarch64-unknown-linux-gnu.tar.gz.sha256"}
  ]
}"###;

    fn sample_status() -> UpdateStatus {
        UpdateStatus {
            checked_at: now_secs(),
            latest_version: "9.9.9".to_string(),
            tag: "v9.9.9".to_string(),
            asset_url: None,
            checksum_url: None,
            notes_url: "https://example.com/release".to_string(),
            published_at: None,
            dismissed_version: None,
        }
    }

    #[test]
    fn parse_release_reads_tag_version_urls_and_platform_assets() {
        let value: Value = serde_json::from_str(SAMPLE_RELEASE_JSON).unwrap();

        let status = parse_release(&value, Some("x86_64-unknown-linux-gnu"), 1_000).unwrap();
        assert_eq!(status.tag, "v0.6.0");
        assert_eq!(status.latest_version, "0.6.0");
        assert_eq!(status.checked_at, 1_000);
        assert_eq!(
            status.notes_url,
            "https://github.com/sougstron/kanban4ai/releases/tag/v0.6.0"
        );
        assert_eq!(
            status.asset_url.as_deref(),
            Some(
                "https://github.com/sougstron/kanban4ai/releases/download/v0.6.0/kanban4ai-v0.6.0-x86_64-unknown-linux-gnu.tar.gz"
            )
        );
        assert_eq!(
            status.checksum_url.as_deref(),
            Some(
                "https://github.com/sougstron/kanban4ai/releases/download/v0.6.0/kanban4ai-v0.6.0-x86_64-unknown-linux-gnu.tar.gz.sha256"
            )
        );

        let status = parse_release(&value, Some("aarch64-unknown-linux-gnu"), 1_000).unwrap();
        assert!(
            status
                .asset_url
                .as_deref()
                .is_some_and(|url| url.ends_with("aarch64-unknown-linux-gnu.tar.gz"))
        );
        assert!(
            status
                .checksum_url
                .as_deref()
                .is_some_and(|url| url.ends_with("aarch64-unknown-linux-gnu.tar.gz.sha256"))
        );
    }

    #[test]
    fn parse_release_without_supported_triple_has_no_assets() {
        let value: Value = serde_json::from_str(SAMPLE_RELEASE_JSON).unwrap();
        let status = parse_release(&value, None, 1_000).unwrap();
        // Fail closed: version news still arrives, download targets do not.
        assert_eq!(status.latest_version, "0.6.0");
        assert_eq!(status.asset_url, None);
        assert_eq!(status.checksum_url, None);
    }

    #[test]
    fn parse_release_survives_unusable_payloads() {
        for payload in [
            r#"{}"#,
            r#"{"tag_name": null}"#,
            r#"{"tag_name": ""}"#,
            r#"{"tag_name": "  "}"#,
            r#"{"tag_name": "v0.6.0"}"#, // no assets section: still a status
        ] {
            let value: Value = serde_json::from_str(payload).unwrap();
            let status = parse_release(&value, Some("x86_64-unknown-linux-gnu"), 0);
            match payload {
                r#"{"tag_name": "v0.6.0"}"# => {
                    let status = status.expect("tag alone is a status");
                    assert_eq!(status.asset_url, None);
                    assert_eq!(status.notes_url, "");
                }
                _ => assert!(status.is_none(), "payload {payload} must not parse"),
            }
        }
    }

    #[test]
    fn version_compare_is_three_part_numeric() {
        assert!(version_is_newer("0.6.0", "0.5.1"));
        assert!(version_is_newer("v0.6.0", "0.5.1"));
        assert!(version_is_newer("0.5.1", "v0.5.0"));
        assert!(version_is_newer("1.0.0", "0.9.9"));
        assert!(version_is_newer("0.5.10", "0.5.9")); // numeric, not lexicographic
        assert!(version_is_newer(" 0.6.0 ", "0.5.1")); // surrounding whitespace
        assert!(!version_is_newer("0.5.1", "0.5.1")); // equal is not newer
        assert!(!version_is_newer("0.5.0", "0.5.1"));
        assert!(!version_is_newer("0.4.9", "0.5.1"));
    }

    #[test]
    fn malformed_remote_tags_compare_as_no_update() {
        for bad in [
            "",
            "v",
            "0.5",
            "0.5.0.1",
            "0.5.0-beta",
            "v0.6.x",
            "release-2026",
            "version-3",
            "0.5.0-beta.1",
        ] {
            assert!(!version_is_newer(bad, "0.5.1"), "{bad:?} must not be newer");
            let mut status = sample_status();
            status.latest_version = bad.to_string();
            status.tag = bad.to_string();
            assert!(
                !is_update_available(&status),
                "{bad:?} must not report an update"
            );
        }
    }

    #[test]
    fn update_availability_compares_against_the_installed_version() {
        let newer = sample_status(); // 9.9.9 vs 0.5.1
        assert!(is_update_available(&newer));

        let mut older = sample_status();
        older.latest_version = installed_version().to_string();
        assert!(!is_update_available(&older));
    }

    #[test]
    fn triple_mapping_covers_only_the_built_platforms() {
        assert_eq!(
            triple_for("x86_64", "linux"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            triple_for("aarch64", "linux"),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(triple_for("x86_64", "macos"), None);
        assert_eq!(triple_for("aarch64", "darwin"), None);
        assert_eq!(triple_for("x86_64", "windows"), None);
        assert_eq!(triple_for("riscv64", "linux"), None);
        // The running platform maps through the same table, never a guess.
        assert_eq!(
            target_triple(),
            triple_for(std::env::consts::ARCH, std::env::consts::OS)
        );
    }

    #[test]
    fn asset_names_match_the_release_workflow_naming() {
        let (archive, checksum) = asset_names("v0.5.0", "x86_64-unknown-linux-gnu");
        assert_eq!(archive, "kanban4ai-v0.5.0-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(
            checksum,
            "kanban4ai-v0.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256"
        );
        // Derived from the tag, never the crate version.
        if let Some((archive, checksum)) = target_asset_names("v9.9.9") {
            assert!(archive.starts_with("kanban4ai-v9.9.9-"));
            assert_eq!(checksum, format!("{archive}.sha256"));
        } else {
            assert_eq!(target_triple(), None, "no names without a build");
        }
    }

    #[test]
    fn status_expires_after_the_configured_interval() {
        let status = UpdateStatus {
            checked_at: 1_000_000,
            ..sample_status()
        };
        assert!(!status_expired(&status, 24, 1_000_000 + 24 * 3600 - 1));
        assert!(status_expired(&status, 24, 1_000_000 + 24 * 3600));
        assert!(status_expired(&status, 1, 1_000_000 + 3600));
        // Clock skew into the future keeps a status fresh.
        assert!(!status_expired(&status, 24, 999_999));
    }

    #[test]
    fn check_latest_honors_the_ttl_and_never_dials_out_under_test() {
        let _guard = CACHE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Fresh cache answers without touching the network.
        let fresh = Arc::new(sample_status());
        force_cache(Some(Arc::clone(&fresh)));
        let status = check_latest(false).expect("fresh cache must answer");
        assert_eq!(*status, *fresh);

        // Force bypasses the TTL and reaches for the network, which the
        // cfg(test) guard turns into an error instead of a request.
        let err = check_latest(true).expect_err("force must reach the network path");
        assert!(matches!(err, UpdateError::Network(_)), "{err:?}");

        // A cache older than the interval also falls through to the network.
        let mut stale = sample_status();
        stale.checked_at = now_secs() - 48 * 3600 - 1; // past the 24h default
        force_cache(Some(Arc::new(stale)));
        assert!(
            check_latest(false).is_err(),
            "stale cache must reach the network path"
        );

        force_cache(None);
    }

    #[test]
    fn warm_check_never_spawns_a_network_thread_under_test() {
        let _guard = CACHE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // A stale cache means warm_check would want to spawn a check; the
        // cfg(test) guard must stop it before any thread exists. The counter
        // is incremented on the calling thread, so this is race-free.
        let mut stale = sample_status();
        stale.checked_at = 0;
        force_cache(Some(Arc::new(stale)));

        let before = spawned_check_threads();
        warm_check();
        warm_check();
        assert_eq!(
            spawned_check_threads(),
            before,
            "warm_check must not spawn under cfg(test)"
        );
        assert!(!check_in_flight());

        force_cache(None);
    }

    #[test]
    fn status_json_round_trip_keeps_every_field() {
        let status = UpdateStatus {
            checked_at: 1_787_000_000,
            latest_version: "0.6.0".to_string(),
            tag: "v0.6.0".to_string(),
            asset_url: Some(
                "https://example.com/kanban4ai-v0.6.0-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            ),
            checksum_url: Some(
                "https://example.com/kanban4ai-v0.6.0-x86_64-unknown-linux-gnu.tar.gz.sha256"
                    .to_string(),
            ),
            notes_url: "https://github.com/sougstron/kanban4ai/releases/tag/v0.6.0".to_string(),
            published_at: Some(1_787_000_000),
            dismissed_version: Some("0.6.0".to_string()),
        };
        let text = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<UpdateStatus>(&text).unwrap(), status);

        // A file written before dismissed_version/published_at existed still
        // loads.
        let legacy = r#"{"checked_at":1,"latest_version":"0.6.0","tag":"v0.6.0","asset_url":null,"checksum_url":null,"notes_url":"n"}"#;
        let parsed: UpdateStatus = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.dismissed_version, None);
        assert_eq!(parsed.published_at, None);
        assert_eq!(parsed.latest_version, "0.6.0");
    }

    #[test]
    fn parse_release_reads_published_at_and_tolerates_junk() {
        let mut payload: Value = serde_json::from_str(SAMPLE_RELEASE_JSON).unwrap();
        payload["published_at"] = Value::String("2026-06-01T10:00:00Z".to_string());
        let status = parse_release(&payload, None, 0).unwrap();
        assert_eq!(
            status.published_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-06-01T10:00:00Z")
                    .unwrap()
                    .timestamp()
            )
        );

        payload["published_at"] = Value::String("not a timestamp".to_string());
        assert_eq!(parse_release(&payload, None, 0).unwrap().published_at, None);
        payload.as_object_mut().unwrap().remove("published_at");
        assert_eq!(parse_release(&payload, None, 0).unwrap().published_at, None);
    }

    #[test]
    fn dismiss_marks_the_version_and_a_newer_tag_reopens_the_banner() {
        let _guard = CACHE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut status = sample_status();
        status.latest_version = "9.9.9".to_string();
        status.tag = "v9.9.9".to_string();
        force_cache(Some(Arc::new(status)));

        assert!(
            cached().as_deref().map(banner_due).unwrap_or(false),
            "undismissed update is due"
        );
        dismiss("9.9.9");
        let marked = cached().expect("status after dismiss");
        assert_eq!(marked.dismissed_version.as_deref(), Some("9.9.9"));
        assert!(!banner_due(&marked), "dismissed version is not due");

        // A newer release than the dismissed one reopens the banner.
        let mut newer = (*cached().unwrap()).clone();
        newer.latest_version = "9.9.10".to_string();
        newer.tag = "v9.9.10".to_string();
        force_cache(Some(Arc::new(newer)));
        assert!(banner_due(cached().as_deref().unwrap()));

        force_cache(None);
        dismiss("9.9.10");
        assert!(cached().is_none());

        force_cache(None);
    }

    #[test]
    fn cached_stays_none_under_test_without_a_seeded_cache() {
        let _guard = CACHE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        force_cache(None);
        // The cfg(test) guard means the developer's real store status file
        // can never leak into a test run.
        assert!(cached().is_none());
    }

    // --- apply path ---------------------------------------------------------

    #[test]
    fn archive_member_matches_the_release_layout() {
        assert_eq!(
            archive_member_path("v0.6.0", "x86_64-unknown-linux-gnu"),
            "kanban4ai-v0.6.0-x86_64-unknown-linux-gnu/kanban4ai"
        );
        // The running platform resolves through the same table as the check.
        if let Some(triple) = target_triple() {
            let member = archive_member_for("v0.6.0").unwrap();
            assert_eq!(member, format!("kanban4ai-v0.6.0-{triple}/kanban4ai"));
        } else {
            assert_eq!(
                archive_member_for("v0.6.0").unwrap_err(),
                ApplyError::NoBuild
            );
        }
    }

    #[test]
    fn hostile_tags_cannot_move_the_archive_member() {
        // The `kanban4ai-` prefix and the `-<triple>` suffix neutralize bare
        // and trailing `..`; only an interior `/../` or `//` can escape.
        for tag in ["../../..", "a/../b", "..//..", "x/../../y"] {
            let err = archive_member_for(tag).expect_err(tag);
            assert!(
                matches!(err, ApplyError::Extract(_)),
                "tag {tag:?} must be rejected, got {err:?}"
            );
        }
        // A plain tag with punctuation the format itself allows stays safe.
        archive_member_for("v0.6.0-rc.1").unwrap();
    }

    #[test]
    fn pacman_owner_parsing_covers_the_probe_output() {
        assert_eq!(
            parse_owner("/usr/bin/kanban4ai is owned by kanban4ai-bin 0.5.1-1"),
            Some("kanban4ai-bin".to_string())
        );
        assert_eq!(parse_owner("error: No package owns /x"), None);
        assert_eq!(parse_owner(""), None);
        // Exit-code gate: a probe that fails or says nothing owns nothing.
        assert_eq!(pacman_owner_with("false", Path::new("/usr/bin/x")), None);
        assert_eq!(pacman_owner_with("true", Path::new("/usr/bin/x")), None);
    }

    #[test]
    fn upgrade_commands_point_at_the_package_manager() {
        for package in ["kanban4ai", "kanban4ai-bin", "kanban4ai-git"] {
            let command = upgrade_command_for(package);
            assert!(command.ends_with(&format!(" -S {package}")), "{command}");
            assert!(
                command.starts_with("yay ") || command.starts_with("paru "),
                "{command}"
            );
        }
        assert_eq!(upgrade_command_for("ripgrep"), "sudo pacman -Syu ripgrep");
    }

    #[test]
    fn checksum_lines_parse_to_a_lowercase_hex_digest() {
        let digest = "a".repeat(64);
        let parsed = parse_expected_checksum(&format!(
            "{digest}  kanban4ai-v0.6.0-x86_64-unknown-linux-gnu.tar.gz\n"
        ))
        .unwrap();
        assert_eq!(parsed, digest);
        let upper = parse_expected_checksum(&format!("{}\n", digest.to_uppercase())).unwrap();
        assert_eq!(upper, digest);
        for bad in ["", "nothex", &"g".repeat(64), &"a".repeat(63)] {
            assert_eq!(parse_expected_checksum(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn error_display_carries_the_guidance_a_caller_shows() {
        let err = ApplyError::PackageManaged {
            package: "kanban4ai-bin".to_string(),
            upgrade_command: "yay -S kanban4ai-bin".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("kanban4ai-bin"), "{text}");
        assert!(text.contains("yay -S kanban4ai-bin"), "{text}");

        let err = ApplyError::NotWritable {
            path: PathBuf::from("/usr/bin"),
            detail: "read-only".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("/usr/bin"), "{text}");
        assert!(text.contains("never run kanban4ai with sudo"), "{text}");

        let text = ApplyError::MissingTool("sha256sum").to_string();
        assert!(text.contains("sha256sum"), "{text}");
        assert!(text.contains("never skipped"), "{text}");

        let text = ApplyError::NoBuild.to_string();
        assert!(text.contains("no release build"), "{text}");
    }

    #[test]
    fn missing_sha256sum_or_tar_is_a_clear_error_not_a_skip() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let err = sha256_with("kanban4ai-no-such-sha256sum", file.path()).unwrap_err();
        assert!(matches!(err, ApplyError::Verify(_)), "{err:?}");
        assert!(err.to_string().contains("checksum verification failed"));

        let err =
            extract_member_with("kanban4ai-no-such-tar", file.path(), "x/kanban4ai").unwrap_err();
        assert!(matches!(err, ApplyError::Extract(_)), "{err:?}");
    }

    #[test]
    fn writable_directory_probes_ok_and_readonly_refuses() {
        let dir = tempfile::tempdir().unwrap();
        ensure_dir_writable(dir.path()).expect("a fresh tempdir is writable");

        // Read-only directory: the temp-file probe must fail with guidance.
        let readonly_mode = std::fs::Permissions::from_mode(0o555);
        let normal_mode = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(dir.path(), readonly_mode.clone()).unwrap();
        // Root bypasses permission bits; skip where the branch cannot bite.
        if std::fs::write(dir.path().join(".probe"), b"x").is_ok() {
            std::fs::set_permissions(dir.path(), normal_mode).unwrap();
            return;
        }
        let err = ensure_dir_writable(dir.path()).expect_err("read-only dir must refuse");
        match err {
            ApplyError::NotWritable { path, detail } => {
                assert_eq!(path, dir.path());
                assert!(!detail.is_empty());
            }
            other => panic!("expected NotWritable, got {other:?}"),
        }
        // Restore so the tempdir can be removed.
        std::fs::set_permissions(dir.path(), normal_mode).unwrap();
    }

    /// True when `dir` holds no `.tmp_*` leftovers after a failed install.
    fn no_temp_leftovers(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .all(|entry| !entry.file_name().to_string_lossy().starts_with(".tmp_"))
            })
            .unwrap_or(false)
    }

    /// Build a release-shaped archive (top-level dir holding the binary) and
    /// return `(archive path, its sha256)`. Skips the caller when tar or
    /// sha256sum are absent — the failure tests below do not need them.
    fn build_release_archive(dir: &Path, tag: &str, payload: &[u8]) -> Option<(PathBuf, String)> {
        if which("tar").is_none() || which("sha256sum").is_none() {
            return None;
        }
        let triple = target_triple()?;
        let top = format!("kanban4ai-{tag}-{triple}");
        let staging = tempfile::tempdir().unwrap();
        let member_dir = staging.path().join(&top);
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(member_dir.join("kanban4ai"), payload).unwrap();
        let archive = dir.join("update.tar.gz");
        let status = Command::new("tar")
            .args(["czf"])
            .arg(&archive)
            .arg("-C")
            .arg(staging.path())
            .arg(&top)
            .status()
            .expect("tar spawn");
        assert!(status.success(), "tar czf failed");
        let digest = sha256_of(&archive).unwrap();
        Some((archive, digest))
    }

    #[test]
    fn install_from_archive_swaps_the_binary_and_leaves_no_temps() {
        let tag = "v0.6.0";
        let Some(triple) = target_triple() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let payload = b"#!/bin/sh\necho brand-new binary\n";
        let Some((archive, expected)) = build_release_archive(dir.path(), tag, payload) else {
            return;
        };
        let exe = dir.path().join("kanban4ai");
        std::fs::write(&exe, b"old binary").unwrap();

        let member = archive_member_path(tag, triple);
        install_from_archive(dir.path(), &exe, &archive, &expected, &member).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), payload);
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "the swapped binary must stay executable"
        );
        assert!(no_temp_leftovers(dir.path()), "temp files must be gone");
    }

    #[test]
    fn checksum_mismatch_touches_nothing_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("update.tar.gz");
        std::fs::write(&archive, b"not even a gzip stream").unwrap();
        let exe = dir.path().join("kanban4ai");
        std::fs::write(&exe, b"old binary").unwrap();

        let err = install_from_archive(dir.path(), &exe, &archive, "deadbeef", "x/kanban4ai")
            .unwrap_err();
        match err {
            ApplyError::ChecksumMismatch { expected, actual } => {
                assert_eq!(expected, "deadbeef");
                assert!(actual.chars().all(|c| c.is_ascii_hexdigit()));
            }
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
        assert_eq!(std::fs::read(&exe).unwrap(), b"old binary");
        assert!(no_temp_leftovers(dir.path()));
    }

    #[test]
    fn failed_extraction_touches_nothing_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("update.tar.gz");
        std::fs::write(&archive, b"valid bytes, invalid gzip").unwrap();
        // Guarded: computing the matching digest needs sha256sum.
        let Some(expected) = (which("sha256sum").is_some()).then(|| sha256_of(&archive).unwrap())
        else {
            return;
        };
        let exe = dir.path().join("kanban4ai");
        std::fs::write(&exe, b"old binary").unwrap();

        let err =
            install_from_archive(dir.path(), &exe, &archive, &expected, "x/kanban4ai").unwrap_err();
        assert!(matches!(err, ApplyError::Extract(_)), "{err:?}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old binary");
        assert!(no_temp_leftovers(dir.path()));
    }
}
