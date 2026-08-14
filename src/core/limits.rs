//! Subscription usage limits for the AI providers kanban launches.
//!
//! Each provider reports how much of a rate-limit window is already spent; the
//! board shows what is left. Three sources, all read-only and best effort — a
//! provider that is not installed, not signed in, or unreachable degrades to a
//! note instead of an error:
//!
//! - **claude**: `GET /api/oauth/usage` on the Anthropic API with the OAuth
//!   access token from `~/.claude/.credentials.json`. Reports a 5-hour session
//!   window and a 7-day window.
//! - **codex**: no network at all. The newest `rollout-*.jsonl` under
//!   `~/.codex/sessions/` carries the `rate_limits` payload the server last
//!   sent, so the numbers are exactly as fresh as the last codex run — the age
//!   is surfaced with the value.
//! - **grok**: `GET /v1/billing` on the grok CLI proxy with the OIDC key from
//!   `~/.grok/auth.json`. Reports credit usage for the current billing period.
//!
//! HTTPS is done by piping a config file into `curl -K -` rather than by
//! linking a TLS stack: it keeps the dependency set unchanged, and it keeps
//! bearer tokens out of the process command line where `ps` would show them.
//!
//! Results are cached in memory and in `<store>/limits.json` so restarts and
//! repeated CLI calls do not re-poll the providers — the claude endpoint is
//! documented to rate-limit polling callers.
//!
//! Clicking a provider segment in the TUI limits row can also refresh that
//! provider through its own CLI (see [`refresh_provider_async`]): codex is
//! asked for fresh rate limits over its app-server JSON-RPC, and running the
//! grok CLI renews the short-lived token in `~/.grok/auth.json` that the
//! billing fetch uses. Both run on a background thread and merge into the
//! same cache, so the row updates on the next tick.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::core::project::store_root;
use crate::core::storage::atomic_write_text;

/// Providers rendered on the board, in display order.
pub const PROVIDERS: [&str; 3] = ["claude", "codex", "grok"];

/// Fallback refresh interval when no board config is available (the projects
/// screen has no project, so no `.kanban/config.yaml` to read).
pub const DEFAULT_REFRESH_INTERVAL: i64 = 120;

const HTTP_TIMEOUT_SECS: u32 = 15;

/// One rate-limit window of one provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    /// Short window name: `5h`, `7d`, `mon`.
    pub label: String,
    /// Percentage of the window still available (100 − used).
    pub remaining_percent: f64,
    /// When the window rolls over, as a Unix timestamp.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

impl LimitWindow {
    fn new(label: impl Into<String>, used_percent: f64, resets_at: Option<i64>) -> Self {
        Self {
            label: label.into(),
            remaining_percent: (100.0 - used_percent).clamp(0.0, 100.0),
            resets_at,
        }
    }

    /// Seconds until the window resets, or `None` when unknown or already past.
    pub fn resets_in(&self, now: i64) -> Option<i64> {
        self.resets_at
            .map(|at| at.saturating_sub(now))
            .filter(|remaining| *remaining > 0)
    }
}

/// Why a provider has no usable numbers, if it has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum ProviderState {
    /// Windows were read successfully.
    Ready,
    /// No credentials/session data on this machine — the provider is simply not
    /// used here, so the board hides it instead of reporting a problem.
    NotConfigured,
    /// Credentials exist but the provider rejected them.
    SignedOut,
    /// Everything else: no `curl`, network failure, unparseable payload.
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderLimits {
    pub provider: String,
    #[serde(flatten)]
    pub state: ProviderState,
    #[serde(default)]
    pub windows: Vec<LimitWindow>,
    /// When the underlying numbers were produced, when that differs from the
    /// fetch time (codex reads a past session's payload).
    #[serde(default)]
    pub observed_at: Option<i64>,
}

impl ProviderLimits {
    fn new(provider: &str, state: ProviderState) -> Self {
        Self {
            provider: provider.to_string(),
            state,
            windows: Vec::new(),
            observed_at: None,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.state == ProviderState::Ready && !self.windows.is_empty()
    }

    /// Age of the underlying data in seconds, when it predates the fetch.
    pub fn data_age(&self, now: i64) -> Option<i64> {
        self.observed_at
            .map(|at| now.saturating_sub(at))
            .filter(|age| *age > 0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitsSnapshot {
    pub fetched_at: i64,
    pub providers: Vec<ProviderLimits>,
}

impl LimitsSnapshot {
    pub fn get(&self, provider: &str) -> Option<&ProviderLimits> {
        self.providers
            .iter()
            .find(|entry| entry.provider == provider)
    }

    pub fn age(&self, now: i64) -> i64 {
        now.saturating_sub(self.fetched_at)
    }
}

fn now_secs() -> i64 {
    Utc::now().timestamp()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// `GET url` through curl, with headers passed on stdin so secrets stay out of
/// the command line. Returns the parsed JSON body, or the HTTP status when the
/// request completed with a non-2xx code.
fn http_get_json(url: &str, headers: &[(&str, String)]) -> std::result::Result<Value, HttpError> {
    let mut config = String::new();
    config.push_str(&format!("url = {}\n", quote_curl(url)));
    for (name, value) in headers {
        config.push_str(&format!(
            "header = {}\n",
            quote_curl(&format!("{name}: {value}"))
        ));
    }
    config.push_str("silent\n");
    config.push_str("show-error\n");
    config.push_str("location\n");
    config.push_str(&format!("max-time = {HTTP_TIMEOUT_SECS}\n"));
    config.push_str("write-out = \"\\n%{http_code}\"\n");

    let mut child = Command::new("curl")
        .arg("-K")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| HttpError::Transport(format!("curl unavailable: {err}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|err| HttpError::Transport(format!("curl input failed: {err}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| HttpError::Transport(format!("curl failed: {err}")))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(HttpError::Transport(
            message
                .lines()
                .next_back()
                .unwrap_or("request failed")
                .trim()
                .to_string(),
        ));
    }
    let body = String::from_utf8_lossy(&output.stdout).to_string();
    let (payload, status) = split_status(&body);
    if !(200..300).contains(&status) {
        return Err(HttpError::Status(status));
    }
    serde_json::from_str(payload).map_err(|err| HttpError::Transport(format!("bad JSON: {err}")))
}

#[derive(Debug)]
enum HttpError {
    Status(u16),
    Transport(String),
}

impl HttpError {
    fn into_state(self) -> ProviderState {
        match self {
            HttpError::Status(401 | 403) => ProviderState::SignedOut,
            HttpError::Status(code) => ProviderState::Unavailable(format!("HTTP {code}")),
            HttpError::Transport(message) => ProviderState::Unavailable(message),
        }
    }
}

/// curl's config parser reads double-quoted values with backslash escapes.
fn quote_curl(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Split a curl body written with a trailing `\n%{http_code}` into the payload
/// and the status code.
fn split_status(body: &str) -> (&str, u16) {
    match body.rsplit_once('\n') {
        Some((payload, status)) => (payload, status.trim().parse().unwrap_or(0)),
        None => (body, 0),
    }
}

// ---------------------------------------------------------------------------
// claude
// ---------------------------------------------------------------------------

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

fn claude_credentials_path() -> Option<PathBuf> {
    Some(home_dir()?.join(".claude").join(".credentials.json"))
}

/// OAuth access token from the Claude Code credential store.
fn claude_access_token(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .map(str::to_string)
        .filter(|token| !token.is_empty())
}

fn fetch_claude() -> ProviderLimits {
    let Some(path) = claude_credentials_path().filter(|path| path.exists()) else {
        return ProviderLimits::new("claude", ProviderState::NotConfigured);
    };
    let Some(token) = claude_access_token(&path) else {
        return ProviderLimits::new("claude", ProviderState::SignedOut);
    };
    let headers = [
        ("Authorization", format!("Bearer {token}")),
        ("anthropic-beta", "oauth-2025-04-20".to_string()),
        ("Accept", "application/json".to_string()),
    ];
    match http_get_json(CLAUDE_USAGE_URL, &headers) {
        Ok(value) => {
            let windows = parse_claude_usage(&value);
            if windows.is_empty() {
                ProviderLimits::new(
                    "claude",
                    ProviderState::Unavailable("no usage windows".to_string()),
                )
            } else {
                ProviderLimits {
                    windows,
                    ..ProviderLimits::new("claude", ProviderState::Ready)
                }
            }
        }
        Err(err) => ProviderLimits::new("claude", err.into_state()),
    }
}

/// Read the `five_hour` / `seven_day` objects of the OAuth usage response.
/// Each carries `utilization` (percent used) and an RFC 3339 `resets_at`.
pub fn parse_claude_usage(value: &Value) -> Vec<LimitWindow> {
    [("five_hour", "5h"), ("seven_day", "7d")]
        .into_iter()
        .filter_map(|(key, label)| {
            let window = value.get(key)?;
            let used = window.get("utilization")?.as_f64()?;
            let resets_at = window
                .get("resets_at")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339);
            Some(LimitWindow::new(label, used, resets_at))
        })
        .collect()
}

fn parse_rfc3339(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.timestamp())
}

// ---------------------------------------------------------------------------
// codex
// ---------------------------------------------------------------------------

fn codex_sessions_dir() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(home_dir()?.join(".codex")))?;
    Some(home.join("sessions"))
}

fn fetch_codex() -> ProviderLimits {
    let Some(dir) = codex_sessions_dir().filter(|dir| dir.is_dir()) else {
        return ProviderLimits::new("codex", ProviderState::NotConfigured);
    };
    let Some(rollout) = newest_rollout(&dir) else {
        return ProviderLimits::new("codex", ProviderState::NotConfigured);
    };
    let Some(line) = last_rate_limit_line(&rollout) else {
        return ProviderLimits::new(
            "codex",
            ProviderState::Unavailable("no rate limits recorded yet".to_string()),
        );
    };
    let windows = parse_codex_rate_limits(&line);
    if windows.is_empty() {
        return ProviderLimits::new(
            "codex",
            ProviderState::Unavailable("no rate limits recorded yet".to_string()),
        );
    }
    ProviderLimits {
        windows,
        observed_at: file_modified_secs(&rollout),
        ..ProviderLimits::new("codex", ProviderState::Ready)
    }
}

/// Newest `rollout-*.jsonl` under `sessions/YYYY/MM/DD/`. Both the day
/// directories and the file names are ISO-ordered, so lexicographic ordering is
/// chronological; the newest day that actually holds a rollout wins.
fn newest_rollout(sessions_dir: &Path) -> Option<PathBuf> {
    let mut days = Vec::new();
    for year in sorted_dirs(sessions_dir) {
        for month in sorted_dirs(&year) {
            days.extend(sorted_dirs(&month));
        }
    }
    days.sort();
    for day in days.into_iter().rev() {
        let mut rollouts = fs::read_dir(&day)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            })
            .collect::<Vec<_>>();
        rollouts.sort();
        if let Some(newest) = rollouts.pop() {
            return Some(newest);
        }
    }
    None
}

fn sorted_dirs(parent: &Path) -> Vec<PathBuf> {
    let mut dirs = fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    dirs
}

/// Last line of a rollout that carries a `rate_limits` payload. Rollouts grow
/// to megabytes, so the file is streamed and only the matching line kept.
fn last_rate_limit_line(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut latest = None;
    for line in BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
    {
        if line.contains("\"rate_limits\"") {
            latest = Some(line);
        }
    }
    latest
}

fn file_modified_secs(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified).timestamp())
}

/// Read the `rate_limits` object codex records in its rollout transcript:
/// `primary`/`secondary` windows with `used_percent`, `window_minutes`, and a
/// Unix `resets_at`.
pub fn parse_codex_rate_limits(line: &str) -> Vec<LimitWindow> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(limits) = find_key(&value, "rate_limits") else {
        return Vec::new();
    };
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| {
            let window = limits.get(key)?;
            let used = window.get("used_percent")?.as_f64()?;
            let minutes = window
                .get("window_minutes")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let resets_at = window.get("resets_at").and_then(Value::as_i64);
            Some(LimitWindow::new(window_label(minutes), used, resets_at))
        })
        .collect()
}

/// Depth-first search for an object key, used to locate `rate_limits` without
/// depending on where codex nests it inside the rollout event.
fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key).filter(|found| found.is_object()) {
                return Some(found);
            }
            map.values().find_map(|nested| find_key(nested, key))
        }
        Value::Array(items) => items.iter().find_map(|nested| find_key(nested, key)),
        _ => None,
    }
}

/// Window length in minutes as the board's short label.
pub fn window_label(minutes: i64) -> String {
    match minutes {
        m if m <= 0 => "window".to_string(),
        m if m < 60 => format!("{m}m"),
        m if m < 1440 => format!("{}h", m / 60),
        43200 => "mon".to_string(),
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m => format!("{}h", m / 60),
    }
}

/// Compact time span for the board and the CLI: `<1m`, `48m`, `3h12m`,
/// `6d4h`, `23d`.
pub fn format_span(seconds: i64) -> String {
    match seconds {
        s if s < 60 => "<1m".to_string(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s if s < 7 * 86_400 => format!("{}d{}h", s / 86_400, (s % 86_400) / 3600),
        s => format!("{}d", s / 86_400),
    }
}

// ---------------------------------------------------------------------------
// grok
// ---------------------------------------------------------------------------

const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";

fn grok_auth_path() -> Option<PathBuf> {
    Some(home_dir()?.join(".grok").join("auth.json"))
}

/// The stored grok CLI session: bearer key and user id. `auth.json` is keyed by
/// `<issuer>::<client id>`, so the single entry is taken as-is.
fn grok_session(path: &Path) -> Option<(String, String)> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let entry = value.as_object()?.values().next()?;
    let key = entry.get("key")?.as_str()?.to_string();
    let user_id = entry
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (!key.is_empty()).then_some((key, user_id))
}

fn fetch_grok() -> ProviderLimits {
    let Some(path) = grok_auth_path().filter(|path| path.exists()) else {
        return ProviderLimits::new("grok", ProviderState::NotConfigured);
    };
    let Some((key, user_id)) = grok_session(&path) else {
        return ProviderLimits::new("grok", ProviderState::SignedOut);
    };
    let headers = [
        ("Authorization", format!("Bearer {key}")),
        ("X-XAI-Token-Auth", "xai-grok-cli".to_string()),
        ("x-userid", user_id),
        ("Accept", "application/json".to_string()),
    ];
    match http_get_json(GROK_BILLING_URL, &headers) {
        Ok(value) => match parse_grok_billing(&value) {
            Some(window) => ProviderLimits {
                windows: vec![window],
                ..ProviderLimits::new("grok", ProviderState::Ready)
            },
            None => ProviderLimits::new(
                "grok",
                ProviderState::Unavailable("no billing period".to_string()),
            ),
        },
        Err(err) => ProviderLimits::new("grok", err.into_state()),
    }
}

/// Read the grok billing response: one window for the current billing period,
/// labelled by its period type and reset at the period end.
pub fn parse_grok_billing(value: &Value) -> Option<LimitWindow> {
    let config = value.get("config")?;
    let used = config.get("creditUsagePercent")?.as_f64()?;
    let period = config.get("currentPeriod");
    let label = period
        .and_then(|period| period.get("type"))
        .and_then(Value::as_str)
        .map(grok_period_label)
        .unwrap_or("period");
    let resets_at = period
        .and_then(|period| period.get("end"))
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .or_else(|| {
            config
                .get("billingPeriodEnd")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339)
        });
    Some(LimitWindow::new(label, used, resets_at))
}

fn grok_period_label(period_type: &str) -> &'static str {
    match period_type {
        "USAGE_PERIOD_TYPE_DAILY" => "24h",
        "USAGE_PERIOD_TYPE_WEEKLY" => "7d",
        "USAGE_PERIOD_TYPE_MONTHLY" => "mon",
        _ => "period",
    }
}

// ---------------------------------------------------------------------------
// CLI-driven refresh (TUI limits-row click)
// ---------------------------------------------------------------------------

const CODEX_RPC_TIMEOUT: Duration = Duration::from_secs(15);
const GROK_CLI_TIMEOUT: Duration = Duration::from_secs(30);

/// Scratch cwd for the provider CLIs, which otherwise treat the current
/// directory as a project; machine-wide, next to the cache file.
fn cli_scratch_dir() -> Option<PathBuf> {
    let dir = store_root().ok()?.join("limits-refresh-cwd");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Wait for a child, killing it past the deadline. Returns `None` on timeout.
fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(Some(status)),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Ask codex itself for fresh rate limits over the app-server JSON-RPC the
/// Codex IDE extension speaks (`account/rateLimits/read` after `initialize`).
/// The exchange costs no usage and answers inside a second; the caller falls
/// back to the rollout files when codex is missing or never replies.
fn fetch_codex_rpc() -> std::result::Result<ProviderLimits, String> {
    let cwd = cli_scratch_dir().ok_or_else(|| "no scratch directory".to_string())?;
    let mut child = Command::new("codex")
        .args(["-s", "read-only", "-a", "untrusted", "app-server"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("codex unavailable: {err}"))?;
    let result = codex_rpc_exchange(&mut child);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn codex_rpc_exchange(child: &mut Child) -> std::result::Result<ProviderLimits, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "codex without stdout".to_string())?;
    let lines = spawn_line_reader(stdout);
    write_rpc(
        child,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"clientInfo": {"name": "kanban4ai",
                                          "version": env!("CARGO_PKG_VERSION")}}}),
    )?;
    let deadline = Instant::now() + CODEX_RPC_TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "codex RPC timeout".to_string())?;
        let line = lines
            .recv_timeout(remaining)
            .map_err(|_| "codex RPC timeout".to_string())?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications (no id) interleave freely with responses.
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if id == 1 {
            write_rpc(
                child,
                &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
            )?;
            write_rpc(
                child,
                &json!({"jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read"}),
            )?;
            continue;
        }
        if id == 2 {
            if let Some(error) = message.get("error") {
                return Err(format!("rateLimits: {error}"));
            }
            let windows = message
                .get("result")
                .map(parse_codex_rpc_rate_limits)
                .unwrap_or_default();
            if windows.is_empty() {
                return Err("no rate limits in reply".to_string());
            }
            return Ok(ProviderLimits {
                windows,
                observed_at: Some(now_secs()),
                ..ProviderLimits::new("codex", ProviderState::Ready)
            });
        }
    }
}

fn spawn_line_reader(stdout: std::process::ChildStdout) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn write_rpc(child: &mut Child, message: &Value) -> std::result::Result<(), String> {
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| "codex without stdin".to_string())?;
    let mut text = serde_json::to_string(message).map_err(|err| err.to_string())?;
    text.push('\n');
    stdin
        .write_all(text.as_bytes())
        .and_then(|()| stdin.flush())
        .map_err(|err| err.to_string())
}

/// Map the app-server `account/rateLimits/read` result onto windows. Field
/// names are camelCase here, unlike the rollout payload.
pub fn parse_codex_rpc_rate_limits(result: &Value) -> Vec<LimitWindow> {
    let Some(limits) = result.get("rateLimits") else {
        return Vec::new();
    };
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| {
            let window = limits.get(key)?;
            let used = window.get("usedPercent")?.as_f64()?;
            let minutes = window
                .get("windowDurationMins")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let resets_at = window.get("resetsAt").and_then(Value::as_i64);
            Some(LimitWindow::new(window_label(minutes), used, resets_at))
        })
        .collect()
}

/// Running the grok CLI renews the short-lived OIDC token in
/// `~/.grok/auth.json` when it is near expiry; `models` is its cheapest
/// non-interactive command. The exit status does not matter — the billing
/// fetch afterwards reports whatever state the token is in.
fn refresh_grok_cli() -> std::result::Result<(), String> {
    let cwd = cli_scratch_dir().ok_or_else(|| "no scratch directory".to_string())?;
    let mut child = Command::new("grok")
        .arg("models")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("grok unavailable: {err}"))?;
    wait_with_timeout(&mut child, GROK_CLI_TIMEOUT).map_err(|err| err.to_string())?;
    Ok(())
}

/// Fetch one provider now, through its CLI where that improves freshness, and
/// merge the result into the cached snapshot (the other providers keep their
/// numbers). Runs on the calling thread; the TUI uses
/// [`refresh_provider_async`].
pub fn refresh_provider_now(provider: &str) {
    let fresh = match provider {
        "codex" => fetch_codex_rpc().unwrap_or_else(|_| fetch_codex()),
        "grok" => {
            let _ = refresh_grok_cli();
            fetch_grok()
        }
        "claude" => fetch_claude(),
        _ => return,
    };
    let mut providers = cached()
        .map(|snapshot| snapshot.as_ref().clone())
        .map(|base: LimitsSnapshot| base.providers)
        .unwrap_or_default();
    match providers
        .iter_mut()
        .find(|entry| entry.provider == fresh.provider)
    {
        Some(entry) => *entry = fresh,
        None => providers.push(fresh),
    }
    let snapshot = Arc::new(LimitsSnapshot {
        fetched_at: now_secs(),
        providers,
    });
    store(snapshot, true);
}

static CLI_REFRESHING: AtomicBool = AtomicBool::new(false);

/// Kick the CLI-driven refresh for one provider off the calling thread.
/// Returns false when one is already running, so the caller can say so.
pub fn refresh_provider_async(provider: &'static str) -> bool {
    // Unit tests must never spawn the provider CLIs: a click under cargo test
    // would otherwise launch codex/grok on the developer's machine.
    if cfg!(test) {
        return true;
    }
    if CLI_REFRESHING.swap(true, Ordering::SeqCst) {
        return false;
    }
    thread::spawn(move || {
        refresh_provider_now(provider);
        CLI_REFRESHING.store(false, Ordering::SeqCst);
    });
    true
}

// ---------------------------------------------------------------------------
// fetch + cache
// ---------------------------------------------------------------------------

/// Poll every provider. Blocking: callers on a UI path use [`refresh_if_stale`].
pub fn fetch_all() -> LimitsSnapshot {
    LimitsSnapshot {
        fetched_at: now_secs(),
        providers: vec![fetch_claude(), fetch_codex(), fetch_grok()],
    }
}

static CACHE: OnceLock<Mutex<Option<Arc<LimitsSnapshot>>>> = OnceLock::new();
static DISK_LOADED: AtomicBool = AtomicBool::new(false);
static REFRESHING: AtomicBool = AtomicBool::new(false);

fn cache() -> &'static Mutex<Option<Arc<LimitsSnapshot>>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

fn cache_file() -> Option<PathBuf> {
    store_root().ok().map(|root| root.join("limits.json"))
}

/// The most recent snapshot: the in-memory one, or the one persisted by an
/// earlier run so the board has numbers to draw before the first fetch returns.
pub fn cached() -> Option<Arc<LimitsSnapshot>> {
    if let Some(snapshot) = cache().lock().ok().and_then(|value| value.clone()) {
        return Some(snapshot);
    }
    if DISK_LOADED.swap(true, Ordering::SeqCst) {
        return None;
    }
    let snapshot = Arc::new(read_cache_file()?);
    store(Arc::clone(&snapshot), false);
    Some(snapshot)
}

fn read_cache_file() -> Option<LimitsSnapshot> {
    let text = fs::read_to_string(cache_file()?).ok()?;
    serde_json::from_str(&text).ok()
}

fn store(snapshot: Arc<LimitsSnapshot>, persist: bool) {
    if let Ok(mut value) = cache().lock() {
        *value = Some(Arc::clone(&snapshot));
    }
    if persist
        && let Some(path) = cache_file()
        && let Ok(text) = serde_json::to_string(snapshot.as_ref())
    {
        let _ = atomic_write_text(&path, &text);
    }
}

/// Fetch now, updating both caches. Used by the CLI, where blocking is fine.
pub fn refresh_blocking() -> Arc<LimitsSnapshot> {
    let snapshot = Arc::new(fetch_all());
    store(Arc::clone(&snapshot), true);
    snapshot
}

/// Start a background refresh when the cached snapshot is older than `ttl`
/// seconds. At most one fetch is in flight; the caller never blocks and keeps
/// drawing the previous snapshot until the new one lands.
pub fn refresh_if_stale(ttl: i64) {
    let ttl = ttl.max(1);
    if cached().is_some_and(|snapshot| snapshot.age(now_secs()) < ttl) {
        return;
    }
    if REFRESHING.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(|| {
        let snapshot = Arc::new(fetch_all());
        store(snapshot, true);
        REFRESHING.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_usage_maps_utilization_to_remaining_percent() {
        let value: Value = serde_json::from_str(
            r#"{"five_hour":{"utilization":34.0,"resets_at":"2026-08-14T11:49:59.994269+00:00"},
                "seven_day":{"utilization":3.0,"resets_at":"2026-08-20T19:59:59.994300+00:00"}}"#,
        )
        .unwrap();

        let windows = parse_claude_usage(&value);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].remaining_percent, 66.0);
        assert_eq!(windows[0].resets_at, Some(1786708199));
        assert_eq!(windows[1].label, "7d");
        assert_eq!(windows[1].remaining_percent, 97.0);
    }

    #[test]
    fn claude_usage_without_windows_is_empty() {
        let value: Value = serde_json::from_str(r#"{"five_hour":null}"#).unwrap();

        assert!(parse_claude_usage(&value).is_empty());
    }

    #[test]
    fn codex_rate_limits_read_both_windows_from_a_nested_event() {
        let line = r#"{"timestamp":"2026-08-06T13:48:20","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":25.0,"window_minutes":43200,"resets_at":1788287121},"secondary":{"used_percent":10.0,"window_minutes":300,"resets_at":1788280000},"plan_type":"free"}}}"#;

        let windows = parse_codex_rate_limits(line);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "mon");
        assert_eq!(windows[0].remaining_percent, 75.0);
        assert_eq!(windows[0].resets_at, Some(1788287121));
        assert_eq!(windows[1].label, "5h");
        assert_eq!(windows[1].remaining_percent, 90.0);
    }

    #[test]
    fn codex_rate_limits_skip_null_and_unparseable_payloads() {
        let line = r#"{"payload":{"rate_limits":{"primary":{"used_percent":7.5,"window_minutes":10080,"resets_at":null},"secondary":null}}}"#;

        let windows = parse_codex_rate_limits(line);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "7d");
        assert_eq!(windows[0].remaining_percent, 92.5);
        assert_eq!(windows[0].resets_at, None);
        assert!(parse_codex_rate_limits("not json").is_empty());
    }

    #[test]
    fn codex_rpc_rate_limits_read_camel_case_windows() {
        let result: Value = serde_json::from_str(
            r#"{"rateLimits":{"limitId":"codex","primary":{"usedPercent":39,"windowDurationMins":43200,"resetsAt":1789045089},"secondary":{"usedPercent":3.0,"windowDurationMins":300,"resetsAt":1786698694},"planType":"free"},"rateLimitsByLimitId":{},"rateLimitResetCredits":{"availableCount":0}}"#,
        )
        .unwrap();

        let windows = parse_codex_rpc_rate_limits(&result);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "mon");
        assert_eq!(windows[0].remaining_percent, 61.0);
        assert_eq!(windows[0].resets_at, Some(1789045089));
        assert_eq!(windows[1].label, "5h");
        assert_eq!(windows[1].remaining_percent, 97.0);

        let without_limits: Value = serde_json::from_str(r#"{"other":true}"#).unwrap();
        assert!(parse_codex_rpc_rate_limits(&without_limits).is_empty());
        let null_window: Value =
            serde_json::from_str(r#"{"rateLimits":{"primary":null,"secondary":null}}"#).unwrap();
        assert!(parse_codex_rpc_rate_limits(&null_window).is_empty());
    }

    #[test]
    fn grok_billing_uses_the_current_period() {
        let value: Value = serde_json::from_str(
            r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-08-12T07:11:32.007632+00:00","end":"2026-08-19T07:11:32.007632+00:00"},"creditUsagePercent":7.0}}"#,
        )
        .unwrap();

        let window = parse_grok_billing(&value).expect("window");

        assert_eq!(window.label, "7d");
        assert_eq!(window.remaining_percent, 93.0);
        assert_eq!(window.resets_at, Some(1787123492));
    }

    #[test]
    fn grok_billing_without_usage_is_none() {
        let value: Value = serde_json::from_str(r#"{"config":{}}"#).unwrap();

        assert!(parse_grok_billing(&value).is_none());
    }

    #[test]
    fn window_labels_cover_the_documented_windows() {
        assert_eq!(window_label(300), "5h");
        assert_eq!(window_label(10080), "7d");
        assert_eq!(window_label(43200), "mon");
        assert_eq!(window_label(30), "30m");
        assert_eq!(window_label(0), "window");
    }

    #[test]
    fn remaining_percent_is_clamped_and_reset_is_relative() {
        let window = LimitWindow::new("5h", 140.0, Some(1_000));
        assert_eq!(window.remaining_percent, 0.0);
        assert_eq!(window.resets_in(900), Some(100));
        assert_eq!(window.resets_in(1_000), None);
    }

    #[test]
    fn spans_are_compact_at_every_magnitude() {
        assert_eq!(format_span(30), "<1m");
        assert_eq!(format_span(2_880), "48m");
        assert_eq!(format_span(11_520), "3h12m");
        assert_eq!(format_span(536_400), "6d5h");
        assert_eq!(format_span(2_000_000), "23d");
    }

    #[test]
    fn curl_status_and_quoting_survive_odd_payloads() {
        assert_eq!(split_status("{\"a\":1}\n200"), ("{\"a\":1}", 200));
        assert_eq!(split_status("no status"), ("no status", 0));
        assert_eq!(quote_curl("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn http_status_codes_map_to_provider_states() {
        assert_eq!(
            HttpError::Status(401).into_state(),
            ProviderState::SignedOut
        );
        assert_eq!(
            HttpError::Status(500).into_state(),
            ProviderState::Unavailable("HTTP 500".to_string())
        );
    }

    #[test]
    fn newest_rollout_picks_the_latest_day_holding_a_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let older = sessions.join("2026/08/06");
        let newer = sessions.join("2026/08/13");
        let empty = sessions.join("2026/08/14");
        for path in [&older, &newer, &empty] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(older.join("rollout-2026-08-06T13-48-20-a.jsonl"), "").unwrap();
        fs::write(newer.join("rollout-2026-08-13T09-00-00-b.jsonl"), "").unwrap();
        fs::write(newer.join("rollout-2026-08-13T21-00-00-c.jsonl"), "").unwrap();
        fs::write(empty.join("notes.txt"), "").unwrap();

        let newest = newest_rollout(&sessions).expect("rollout");

        assert!(newest.ends_with("rollout-2026-08-13T21-00-00-c.jsonl"));
        assert!(newest_rollout(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn last_rate_limit_line_wins_over_earlier_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        fs::write(
            &path,
            "{\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":10.0,\"window_minutes\":300}}}}\n\
             {\"other\":true}\n\
             {\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":40.0,\"window_minutes\":300}}}}\n",
        )
        .unwrap();

        let line = last_rate_limit_line(&path).expect("line");

        assert_eq!(parse_codex_rate_limits(&line)[0].remaining_percent, 60.0);
    }

    #[test]
    fn missing_credentials_report_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        assert!(claude_access_token(&dir.path().join("nope.json")).is_none());
        assert!(grok_session(&dir.path().join("nope.json")).is_none());
    }

    #[test]
    fn grok_session_reads_the_single_issuer_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{"https://auth.x.ai::client":{"key":"abc","user_id":"user-1"}}"#,
        )
        .unwrap();

        assert_eq!(
            grok_session(&path),
            Some(("abc".to_string(), "user-1".to_string()))
        );
    }

    #[test]
    fn claude_token_is_read_from_the_oauth_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"tok-1"}}"#).unwrap();

        assert_eq!(claude_access_token(&path), Some("tok-1".to_string()));
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = LimitsSnapshot {
            fetched_at: 1_700_000_000,
            providers: vec![
                ProviderLimits {
                    windows: vec![LimitWindow::new("5h", 34.0, Some(1_700_001_000))],
                    ..ProviderLimits::new("claude", ProviderState::Ready)
                },
                ProviderLimits::new("codex", ProviderState::NotConfigured),
                ProviderLimits::new("grok", ProviderState::Unavailable("HTTP 500".to_string())),
            ],
        };

        let text = serde_json::to_string(&snapshot).unwrap();
        let parsed: LimitsSnapshot = serde_json::from_str(&text).unwrap();

        assert_eq!(parsed, snapshot);
        assert!(parsed.get("claude").unwrap().is_ready());
        assert!(!parsed.get("codex").unwrap().is_ready());
        assert_eq!(parsed.age(1_700_000_060), 60);
    }

    #[test]
    fn data_age_reports_only_past_observations() {
        let stale = ProviderLimits {
            observed_at: Some(1_000),
            ..ProviderLimits::new("codex", ProviderState::Ready)
        };

        assert_eq!(stale.data_age(1_600), Some(600));
        assert_eq!(stale.data_age(900), None);
    }
}
