//! Subscription usage limits for the AI providers kanban launches.
//!
//! Each provider reports how much of a rate-limit window is already spent; the
//! board shows what is left. Three sources, all read-only and best effort — a
//! provider that is not installed, not signed in, or unreachable degrades to a
//! note instead of an error:
//!
//! - **claude**: two sources that top each other up window by window. The
//!   statusline bridge — Claude Code (>= 2.1.80) pipes `rate_limits` to its
//!   statusLine command on every turn, so a small shim (installed by
//!   `kanban limits bridge install`) records them to
//!   `<store>/claude-rate-limits.json` without spending any API budget. While
//!   *every* bridge window is still open the usage endpoint is not polled at
//!   all; once any of them has rolled over the bridge can no longer say what
//!   the new window holds, so `GET /api/oauth/usage` on the Anthropic API is
//!   polled on its own interval (see [`CLAUDE_USAGE_MIN_INTERVAL_SECS`]) and
//!   merged per window with whatever the bridge still knows. The OAuth access
//!   token comes from `~/.claude/.credentials.json`; when it has expired the
//!   stored refresh token is traded for a new one and the rotated pair is
//!   written back, so the board keeps working while Claude Code is idle.
//! - **codex**: no network at all. The newest `rollout-*.jsonl` under
//!   `~/.codex/sessions/` carries the `rate_limits` payload the server last
//!   sent, so the numbers are exactly as fresh as the last codex run — the age
//!   is surfaced with the value.
//! - **grok**: `GET /v1/billing` on the grok CLI proxy with the OIDC key from
//!   `~/.grok/auth.json`. Reports credit usage for the current billing period.
//! - **zai**: `GET /api/monitor/usage/quota/limit` on `api.z.ai` with the API
//!   key opencode stores for the GLM Coding Plan
//!   (`~/.local/share/opencode/auth.json`, `zai-coding-plan.key`). Reports a
//!   5-hour and a weekly credit window; percentages come from
//!   `currentValue`/`usage` with the integer `percentage` as fallback.
//! - **synthetic**: `GET /v2/quotas` on api.synthetic.new (documented as free —
//!   it never counts against the subscription). The key is `$SYNTHETIC_API_KEY`
//!   or the `synthetic.key` entry opencode's connect flow stores. Reports the
//!   rolling five-hour request window (`rollingFiveHourLimit` remaining/max,
//!   falling back to `subscription` requests/limit, rolling over at
//!   `subscription.renewsAt`) and the weekly credit window
//!   (`weeklyTokenLimit.percentRemaining`); both regenerate in small ticks, so
//!   the reset time is the next capacity gain, not a hard rollover.
//!
//! HTTPS is done by piping a config file into `curl -K -` rather than by
//! linking a TLS stack: it keeps the dependency set unchanged, and it keeps
//! bearer tokens out of the process command line where `ps` would show them.
//!
//! Results are cached in memory and in `<store>/limits.json` so restarts and
//! repeated CLI calls do not re-poll the providers — the claude endpoint is
//! documented to rate-limit polling callers. A 429 keeps the last good Claude
//! windows (so the row does not flip to n/a) and doubles the snapshot TTL
//! before the next background poll, capped at 64×.
//!
//! Clicking a provider segment in the TUI limits row can also refresh that
//! provider (see [`refresh_provider_async`]): claude force-polls the usage
//! endpoint (skipping the current-bridge short-circuit and the 15-minute
//! interval the background refresh honors), codex is asked for fresh rate
//! limits over its app-server JSON-RPC, and running the grok CLI renews the
//! short-lived token in `~/.grok/auth.json` that the billing fetch uses. Each
//! runs on a background thread and merges into the same cache, so the row
//! updates on the next tick.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
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
pub const PROVIDERS: [&str; 5] = ["claude", "codex", "grok", "zai", "synthetic"];

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

    /// Whether the window has rolled over since it was observed, which makes
    /// its percentage a reading of a window that no longer exists. A window
    /// without a known reset time is never treated as expired.
    pub fn is_expired(&self, now: i64) -> bool {
        self.resets_at.is_some_and(|at| at <= now)
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

    /// The windows still worth showing. A window whose reset time has passed
    /// describes a period that has since rolled over, so its percentage is
    /// stale by definition — callers drop it rather than freeze yesterday's
    /// number on the row until a fresh observation arrives.
    pub fn live_windows(&self, now: i64) -> Vec<&LimitWindow> {
        self.windows
            .iter()
            .filter(|window| !window.is_expired(now))
            .collect()
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

/// `GET url` through curl. See [`http_request_json`].
fn http_get_json(url: &str, headers: &[(&str, String)]) -> std::result::Result<Value, HttpError> {
    http_request_json(url, headers, None)
}

/// One HTTP request through curl, with headers and any request body passed on
/// stdin so secrets stay out of the command line (a refresh token in `ps`
/// output would be as good as the credential file itself). A body makes it a
/// POST. Returns the parsed JSON body, or the HTTP status when the request
/// completed with a non-2xx code.
fn http_request_json(
    url: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
) -> std::result::Result<Value, HttpError> {
    let mut config = String::new();
    config.push_str(&format!("url = {}\n", quote_curl(url)));
    for (name, value) in headers {
        config.push_str(&format!(
            "header = {}\n",
            quote_curl(&format!("{name}: {value}"))
        ));
    }
    if let Some(body) = body {
        config.push_str(&format!("data = {}\n", quote_curl(body)));
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

/// Where a Claude Code access token is renewed from its refresh token.
const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Claude Code's public OAuth client id — the same one its own login flow
/// sends; the refresh grant is rejected without it.
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Renew the access token this long before its stated expiry, so a poll never
/// races the boundary.
const CLAUDE_TOKEN_SKEW_SECS: i64 = 300;

fn claude_credentials_path() -> Option<PathBuf> {
    Some(home_dir()?.join(".claude").join(".credentials.json"))
}

/// The OAuth material Claude Code keeps in `~/.claude/.credentials.json`.
#[derive(Debug, Clone, PartialEq)]
struct ClaudeCredentials {
    access_token: String,
    refresh_token: Option<String>,
    /// `expiresAt`, converted from the file's milliseconds to Unix seconds.
    expires_at: Option<i64>,
}

/// Read the `claudeAiOauth` block of the Claude Code credential store.
fn read_claude_credentials(path: &Path) -> Option<ClaudeCredentials> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let oauth = value.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")?
        .as_str()
        .map(str::to_string)
        .filter(|token| !token.is_empty())?;
    Some(ClaudeCredentials {
        access_token,
        refresh_token: oauth
            .get("refreshToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string),
        expires_at: oauth
            .get("expiresAt")
            .and_then(Value::as_i64)
            .filter(|at| *at > 0)
            .map(|millis| millis / 1000),
    })
}

/// Whether the stored access token is spent (or about to be). Credentials
/// without a stated expiry are taken at face value — a 401 still triggers the
/// refresh below.
fn claude_token_expired(credentials: &ClaudeCredentials, now: i64) -> bool {
    credentials
        .expires_at
        .is_some_and(|at| at.saturating_sub(CLAUDE_TOKEN_SKEW_SECS) <= now)
}

/// Read an OAuth token response onto the current credentials. `refresh_token`
/// is only present when the server rotated it, so it falls back to the token
/// that was just spent; `expires_in` is seconds from now.
fn parse_claude_token_response(
    value: &Value,
    current: &ClaudeCredentials,
    now: i64,
) -> Option<ClaudeCredentials> {
    let access_token = value
        .get("access_token")?
        .as_str()
        .map(str::to_string)
        .filter(|token| !token.is_empty())?;
    Some(ClaudeCredentials {
        access_token,
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .or_else(|| current.refresh_token.clone()),
        expires_at: value
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|seconds| *seconds > 0)
            .map(|seconds| now.saturating_add(seconds)),
    })
}

/// Trade the stored refresh token for a fresh access token.
///
/// The new pair is written back to Claude Code's own credential store rather
/// than kept private here: the grant rotates the refresh token, so a copy that
/// only this process knows about would leave Claude Code holding one the
/// server has already retired.
fn refresh_claude_token(
    path: &Path,
    credentials: &ClaudeCredentials,
) -> std::result::Result<ClaudeCredentials, HttpError> {
    let Some(refresh_token) = credentials.refresh_token.clone() else {
        // Nothing to renew with — the user has to sign in to Claude Code again.
        return Err(HttpError::Status(401));
    };
    let body = json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLAUDE_CLIENT_ID,
    })
    .to_string();
    let headers = [
        ("Content-Type", "application/json".to_string()),
        ("Accept", "application/json".to_string()),
    ];
    let value = http_request_json(CLAUDE_TOKEN_URL, &headers, Some(&body))?;
    let refreshed = parse_claude_token_response(&value, credentials, now_secs())
        .ok_or_else(|| HttpError::Transport("no access token in refresh reply".to_string()))?;
    write_claude_credentials(path, &refreshed);
    Ok(refreshed)
}

/// Store renewed tokens, leaving every other field of the credential file — and
/// its owner-only permissions — as they were. Best effort: a store this process
/// cannot write still yields a usable token for this run.
fn write_claude_credentials(path: &Path, credentials: &ClaudeCredentials) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let Some(oauth) = value
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    oauth.insert(
        "accessToken".to_string(),
        Value::String(credentials.access_token.clone()),
    );
    if let Some(refresh_token) = credentials.refresh_token.clone() {
        oauth.insert("refreshToken".to_string(), Value::String(refresh_token));
    }
    if let Some(expires_at) = credentials.expires_at {
        oauth.insert(
            "expiresAt".to_string(),
            Value::from(expires_at.saturating_mul(1000)),
        );
    }
    let Ok(text) = serde_json::to_string(&value) else {
        return;
    };
    if atomic_write_text(path, &text).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

/// The usage windows the OAuth endpoint reports for one access token.
fn fetch_claude_usage(token: &str) -> std::result::Result<Vec<LimitWindow>, HttpError> {
    let headers = [
        ("Authorization", format!("Bearer {token}")),
        ("anthropic-beta", "oauth-2025-04-20".to_string()),
        ("Accept", "application/json".to_string()),
    ];
    let value = http_get_json(CLAUDE_USAGE_URL, &headers)?;
    let windows = parse_claude_usage(&value);
    if windows.is_empty() {
        return Err(HttpError::Transport("no usage windows".to_string()));
    }
    Ok(windows)
}

fn fetch_claude() -> ProviderLimits {
    let Some(path) = claude_credentials_path().filter(|path| path.exists()) else {
        return ProviderLimits::new("claude", ProviderState::NotConfigured);
    };
    let Some(credentials) = read_claude_credentials(&path) else {
        return ProviderLimits::new("claude", ProviderState::SignedOut);
    };
    // Claude Code renews the token on its next run, but the board is asking
    // precisely because Claude Code has been idle, so it renews the token too.
    let credentials = if claude_token_expired(&credentials, now_secs()) {
        match refresh_claude_token(&path, &credentials) {
            Ok(refreshed) => refreshed,
            Err(err) => return ProviderLimits::new("claude", err.into_state()),
        }
    } else {
        credentials
    };
    let windows = match fetch_claude_usage(&credentials.access_token) {
        // The stored expiry disagreed with the server: renew once and retry.
        Err(HttpError::Status(401)) => refresh_claude_token(&path, &credentials)
            .and_then(|refreshed| fetch_claude_usage(&refreshed.access_token)),
        other => other,
    };
    match windows {
        Ok(windows) => ProviderLimits {
            windows,
            observed_at: Some(now_secs()),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        },
        Err(err) => ProviderLimits::new("claude", err.into_state()),
    }
}

/// Consecutive Claude usage-endpoint 429s. Each one doubles the wait before
/// the next background poll; a successful / signed-out / unconfigured fetch
/// resets it. Not persisted — a restart is allowed one extra probe.
static CLAUDE_429_STREAK: AtomicU32 = AtomicU32::new(0);

/// Cap on TTL doublings after 429s (120s × 64 ≈ 2.1h).
const CLAUDE_429_BACKOFF_CAP: u32 = 6;

/// How long the TUI should wait before the next background poll after
/// `consecutive_429s` Claude usage-endpoint 429s.
const fn next_backoff_ttl(base_ttl: i64, consecutive_429s: u32) -> i64 {
    let base = if base_ttl < 1 { 1 } else { base_ttl };
    let shift = if consecutive_429s > CLAUDE_429_BACKOFF_CAP {
        CLAUDE_429_BACKOFF_CAP
    } else {
        consecutive_429s
    };
    base.saturating_mul(1_i64 << shift)
}

fn is_http_429(state: &ProviderState) -> bool {
    matches!(state, ProviderState::Unavailable(message) if message == "HTTP 429")
}

/// Keep the last good Claude windows when the usage endpoint 429s, so the
/// row does not flip to n/a. The bool is whether this response was a 429
/// (the caller doubles the poll TTL).
fn apply_claude_429_policy(
    previous: Option<&ProviderLimits>,
    fresh: ProviderLimits,
) -> (ProviderLimits, bool) {
    if !is_http_429(&fresh.state) {
        return (fresh, false);
    }
    match previous {
        Some(previous) if previous.is_ready() => (previous.clone(), true),
        _ => (fresh, true),
    }
}

/// How often the usage endpoint may be polled once the bridge has stopped
/// covering every window. Its per-access-token budget is a handful of requests
/// — polling it on the board's refresh tick is what made this row 429 — while
/// the shortest window it reports is five hours, so a quarter hour of lag costs
/// nothing that is visible on the row.
pub const CLAUDE_USAGE_MIN_INTERVAL_SECS: i64 = 900;

/// When the usage endpoint was last polled. Cached in memory and on disk,
/// because a run of `kanban limits` calls is a run of fresh processes and the
/// interval has to hold across all of them.
static CLAUDE_USAGE_POLLED_AT: AtomicI64 = AtomicI64::new(0);

fn claude_usage_poll_path() -> Option<PathBuf> {
    store_root().ok().map(|root| root.join("claude-usage-poll"))
}

fn claude_usage_polled_at() -> i64 {
    let cached = CLAUDE_USAGE_POLLED_AT.load(Ordering::SeqCst);
    if cached > 0 {
        return cached;
    }
    let stored = claude_usage_poll_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| text.trim().parse::<i64>().ok())
        .unwrap_or(0);
    CLAUDE_USAGE_POLLED_AT.store(stored, Ordering::SeqCst);
    stored
}

fn record_claude_usage_poll(now: i64) {
    CLAUDE_USAGE_POLLED_AT.store(now, Ordering::SeqCst);
    if let Some(path) = claude_usage_poll_path() {
        let _ = fs::create_dir_all(path.parent().unwrap_or(Path::new("/")));
        let _ = atomic_write_text(&path, &now.to_string());
    }
}

/// Whether the usage endpoint may be polled now.
fn claude_usage_poll_due(now: i64) -> bool {
    now.saturating_sub(claude_usage_polled_at()) >= CLAUDE_USAGE_MIN_INTERVAL_SECS
}

/// Claude's windows from the bridge, the usage endpoint, or both.
///
/// `force` is a user asking for the numbers now (`kanban limits --refresh`,
/// or a tap on the claude segment): it skips both the current-bridge
/// short-circuit and the poll interval. The background refresh never does.
fn resolve_claude(previous: Option<&LimitsSnapshot>, force: bool) -> ProviderLimits {
    let previous_entry = previous.and_then(|snapshot| snapshot.get("claude"));
    let now = now_secs();
    let bridge = read_claude_bridge();
    // A bridge that still covers every window answers on its own — except
    // when the user asked for a refresh. "Current" only means no window has
    // reset yet, so a tap hours after the last Claude Code turn would
    // otherwise replay the same stale file. Background polls never force:
    // the usage endpoint's budget is a handful of requests per token.
    if !force
        && let Some(bridge) = bridge
            .clone()
            .filter(|bridge| claude_bridge_current(bridge, now))
    {
        CLAUDE_429_STREAK.store(0, Ordering::SeqCst);
        // Still merged with what is cached: a statusline payload that carried
        // only `five_hour` would otherwise drop the 7-day window off the row.
        return merge_claude_sources(
            Some(bridge),
            previous_entry
                .cloned()
                .unwrap_or_else(|| ProviderLimits::new("claude", ProviderState::Ready)),
            now,
        );
    }
    // Some window has rolled over since the last statusline tick and only the
    // endpoint can say what replaced it — but on its own interval, not the
    // board's. Until then the row keeps the windows that are still running and
    // drops the reset one (see [`ProviderLimits::live_windows`]).
    if let Some(previous_entry) = previous_entry
        && !force
        && !claude_usage_poll_due(now)
    {
        return merge_claude_sources(bridge, previous_entry.clone(), now);
    }
    record_claude_usage_poll(now);
    let (resolved, hit_429) = apply_claude_429_policy(previous_entry, fetch_claude());
    if hit_429 {
        CLAUDE_429_STREAK.fetch_add(1, Ordering::SeqCst);
    } else if matches!(
        resolved.state,
        ProviderState::Ready | ProviderState::SignedOut | ProviderState::NotConfigured
    ) {
        CLAUDE_429_STREAK.store(0, Ordering::SeqCst);
    }
    merge_claude_sources(bridge, resolved, now)
}

/// Combine the two Claude sources window by window, because neither is
/// complete on its own: the bridge only learns a window when Claude Code takes
/// a turn, and the endpoint may only be polled every so often. For each label
/// the fresher observation wins, except that a window which has already reset
/// never displaces one that is still running. Stale bridge windows still beat
/// an endpoint that refuses to answer at all.
///
/// `observed_at` becomes the oldest observation that survived the merge, so the
/// row never claims to be fresher than the stalest number on it.
fn merge_claude_sources(
    bridge: Option<ProviderLimits>,
    http: ProviderLimits,
    now: i64,
) -> ProviderLimits {
    let Some(bridge) = bridge.filter(ProviderLimits::is_ready) else {
        return http;
    };
    if !http.is_ready() {
        return bridge;
    }
    let http_at = http.observed_at.unwrap_or(0);
    let bridge_at = bridge.observed_at.unwrap_or(0);
    let mut merged: Vec<(LimitWindow, i64)> = Vec::new();
    for (window, observed_at) in http
        .windows
        .iter()
        .map(|window| (window, http_at))
        .chain(bridge.windows.iter().map(|window| (window, bridge_at)))
    {
        match merged
            .iter_mut()
            .find(|(existing, _)| existing.label == window.label)
        {
            Some(entry) if prefers_window(window, observed_at, &entry.0, entry.1, now) => {
                *entry = (window.clone(), observed_at);
            }
            Some(_) => {}
            None => merged.push((window.clone(), observed_at)),
        }
    }
    merged.sort_by_key(|(window, _)| claude_window_rank(&window.label));
    let observed_at = merged.iter().map(|(_, at)| *at).min().filter(|at| *at > 0);
    ProviderLimits {
        windows: merged.into_iter().map(|(window, _)| window).collect(),
        observed_at,
        ..ProviderLimits::new("claude", ProviderState::Ready)
    }
}

/// Whether `candidate` describes a window's current state better than
/// `current` does: a running window beats a reset one whatever their ages,
/// otherwise the later observation wins.
fn prefers_window(
    candidate: &LimitWindow,
    candidate_at: i64,
    current: &LimitWindow,
    current_at: i64,
    now: i64,
) -> bool {
    if candidate.is_expired(now) != current.is_expired(now) {
        return !candidate.is_expired(now);
    }
    candidate_at > current_at
}

/// Display order of the claude windows, so a merge cannot reorder the row.
fn claude_window_rank(label: &str) -> u8 {
    match label {
        "5h" => 0,
        "7d" => 1,
        _ => 2,
    }
}

// ---------------------------------------------------------------------------
// claude statusline bridge
// ---------------------------------------------------------------------------

/// How long bridge windows stay trusted after their last statusline tick when
/// no window still has a future reset time.
const CLAUDE_BRIDGE_GRACE_SECS: i64 = 1800;

/// Shape of `<store>/claude-rate-limits.json`, written by the hidden
/// `kanban statusline-bridge` command and read by [`read_claude_bridge`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaudeBridgeFile {
    updated_at: i64,
    windows: Vec<LimitWindow>,
}

/// The machine-wide bridge file, next to the snapshot cache.
fn claude_bridge_path() -> Option<PathBuf> {
    store_root()
        .ok()
        .map(|root| root.join("claude-rate-limits.json"))
}

/// Read the `rate_limits` object Claude Code (>= 2.1.80) pipes to statusline
/// commands on every turn: `five_hour` / `seven_day` carry `used_percentage`
/// (0-100) and `resets_at` as Unix seconds. The OAuth usage response's
/// spellings (`utilization`, RFC 3339 `resets_at`) are tolerated so a field
/// rename degrades instead of going dark. Per-model `seven_day_*` buckets are
/// ignored — the row lists the account-level windows only.
pub fn parse_claude_statusline(value: &Value) -> Vec<LimitWindow> {
    let Some(limits) = value.get("rate_limits") else {
        return Vec::new();
    };
    [("five_hour", "5h"), ("seven_day", "7d")]
        .into_iter()
        .filter_map(|(key, label)| {
            let window = limits.get(key)?;
            let used = window
                .get("used_percentage")
                .and_then(Value::as_f64)
                .or_else(|| window.get("utilization").and_then(Value::as_f64))?;
            let resets_at = window
                .get("resets_at")
                .and_then(|at| match at {
                    Value::Number(_) => at.as_i64(),
                    Value::String(text) => parse_rfc3339(text),
                    _ => None,
                })
                .filter(|at| *at > 0);
            Some(LimitWindow::new(label, used, resets_at))
        })
        .collect()
}

/// Record fresh windows from the statusline shim. Best effort by design: a
/// failure here must never break the statusline that called in.
pub fn store_claude_bridge(windows: &[LimitWindow]) -> bool {
    let Some(path) = claude_bridge_path() else {
        return false;
    };
    if windows.is_empty() {
        return false;
    }
    if fs::create_dir_all(path.parent().unwrap_or(Path::new("/"))).is_err() {
        return false;
    }
    let file = ClaudeBridgeFile {
        updated_at: now_secs(),
        windows: windows.to_vec(),
    };
    let Ok(text) = serde_json::to_string(&file) else {
        return false;
    };
    atomic_write_text(&path, &text).is_ok()
}

/// The recorded statusline bridge windows, when any exist. `observed_at`
/// carries the last statusline tick so the row can age the numbers honestly.
fn read_claude_bridge() -> Option<ProviderLimits> {
    let text = fs::read_to_string(claude_bridge_path()?).ok()?;
    let file: ClaudeBridgeFile = serde_json::from_str(&text).ok()?;
    if file.updated_at <= 0 || file.windows.is_empty() {
        return None;
    }
    Some(ProviderLimits {
        observed_at: Some(file.updated_at),
        windows: file.windows,
        ..ProviderLimits::new("claude", ProviderState::Ready)
    })
}

/// Bridge windows are current while *every* window has yet to reset; unknown
/// or already-past resets get a grace period after the last statusline tick.
///
/// One window still running is not enough: the 7-day window resets days out, so
/// an `any` test would keep a bridge file whose 5-hour window rolled over hours
/// ago on the row, and the usage endpoint would never be asked what the new
/// five-hour window holds.
fn claude_bridge_current(bridge: &ProviderLimits, now: i64) -> bool {
    bridge
        .windows
        .iter()
        .all(|window| window.resets_in(now).is_some())
        || now.saturating_sub(bridge.observed_at.unwrap_or(0)) < CLAUDE_BRIDGE_GRACE_SECS
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
// zai
// ---------------------------------------------------------------------------

const ZAI_QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";

/// opencode's credential store, shared by the providers connected through it.
fn opencode_auth_path() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".local").join("share")))?;
    Some(data.join("opencode").join("auth.json"))
}

/// The GLM Coding Plan API key from opencode's credential store. An auth.json
/// without a `zai-coding-plan` entry simply has no plan here.
fn zai_api_key(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let key = value
        .get("zai-coding-plan")?
        .get("key")?
        .as_str()?
        .to_string();
    (!key.is_empty()).then_some(key)
}

fn fetch_zai() -> ProviderLimits {
    let Some(path) = opencode_auth_path().filter(|path| path.exists()) else {
        return ProviderLimits::new("zai", ProviderState::NotConfigured);
    };
    let Some(key) = zai_api_key(&path) else {
        return ProviderLimits::new("zai", ProviderState::NotConfigured);
    };
    let headers = [
        ("Authorization", format!("Bearer {key}")),
        ("Accept", "application/json".to_string()),
    ];
    match http_get_json(ZAI_QUOTA_URL, &headers) {
        Ok(value) => {
            let windows = parse_zai_quota(&value);
            if windows.is_empty() {
                ProviderLimits::new("zai", ProviderState::Unavailable("no quota".to_string()))
            } else {
                ProviderLimits {
                    windows,
                    ..ProviderLimits::new("zai", ProviderState::Ready)
                }
            }
        }
        Err(err) => ProviderLimits::new("zai", err.into_state()),
    }
}

/// Window length of one z.ai limit entry in minutes. `unit` is the API's time
/// unit enum (day/hour/minute/week); the `TIME_LIMIT` pair `(unit 5, number 1)`
/// is a sentinel meaning the monthly MCP allowance, not a real 1-minute window.
fn zai_window_minutes(limit: &Value) -> i64 {
    let number = limit
        .get("number")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let unit = limit.get("unit").and_then(Value::as_i64).unwrap_or(0);
    if limit.get("type").and_then(Value::as_str) == Some("TIME_LIMIT") && unit == 5 && number == 1 {
        return 43_200;
    }
    let per_unit = match unit {
        1 => 1_440,
        3 => 60,
        5 => 1,
        6 => 10_080,
        _ => 0,
    };
    number.saturating_mul(per_unit)
}

/// Used percentage of one z.ai limit entry: `currentValue / usage` is exact,
/// the integer `percentage` is the rounded fallback.
fn zai_used_percent(limit: &Value) -> Option<f64> {
    if let Some(total) = limit
        .get("usage")
        .and_then(Value::as_f64)
        .filter(|t| *t > 0.0)
    {
        let used = limit
            .get("currentValue")
            .and_then(Value::as_f64)
            .or_else(|| {
                limit
                    .get("remaining")
                    .and_then(Value::as_f64)
                    .map(|remaining| (total - remaining).max(0.0))
            });
        if let Some(used) = used {
            return Some(used / total * 100.0);
        }
    }
    limit.get("percentage").and_then(Value::as_f64)
}

/// Read the z.ai quota response: one window per `data.limits[]` entry that
/// carries a usage percentage, labelled by its window length. `nextResetTime`
/// is Unix milliseconds.
pub fn parse_zai_quota(value: &Value) -> Vec<LimitWindow> {
    let Some(limits) = value.get("data").and_then(|data| data.get("limits")) else {
        return Vec::new();
    };
    limits
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|limit| {
            let used = zai_used_percent(limit)?;
            let resets_at = limit
                .get("nextResetTime")
                .and_then(Value::as_i64)
                .filter(|millis| *millis > 0)
                .map(|millis| millis / 1000);
            Some(LimitWindow::new(
                window_label(zai_window_minutes(limit)),
                used,
                resets_at,
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// synthetic
// ---------------------------------------------------------------------------

const SYNTHETIC_QUOTAS_URL: &str = "https://api.synthetic.new/v2/quotas";

/// The synthetic API key from opencode's credential store, written by its
/// connect flow. An auth.json without the entry simply has no synthetic
/// subscription here; `$SYNTHETIC_API_KEY` is honored first in
/// `fetch_synthetic`.
fn synthetic_key_from_store(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let key = value.get("synthetic")?.get("key")?.as_str()?.to_string();
    (!key.is_empty()).then_some(key)
}

fn fetch_synthetic() -> ProviderLimits {
    let Some(key) = std::env::var_os("SYNTHETIC_API_KEY")
        .filter(|key| !key.is_empty())
        .map(|key| key.to_string_lossy().to_string())
        .or_else(|| {
            opencode_auth_path()
                .filter(|path| path.exists())
                .and_then(|path| synthetic_key_from_store(&path))
        })
    else {
        return ProviderLimits::new("synthetic", ProviderState::NotConfigured);
    };
    let headers = [
        ("Authorization", format!("Bearer {key}")),
        ("Accept", "application/json".to_string()),
    ];
    match http_get_json(SYNTHETIC_QUOTAS_URL, &headers) {
        Ok(value) => {
            let windows = parse_synthetic_quotas(&value);
            if windows.is_empty() {
                ProviderLimits::new(
                    "synthetic",
                    ProviderState::Unavailable("no quota".to_string()),
                )
            } else {
                ProviderLimits {
                    windows,
                    ..ProviderLimits::new("synthetic", ProviderState::Ready)
                }
            }
        }
        Err(err) => ProviderLimits::new("synthetic", err.into_state()),
    }
}

/// Read the quotas response: the five-hour request window — the rolling
/// regeneration state is the live number, `subscription` requests/limit the
/// fallback, its `renewsAt` the window rollover — and the weekly credit
/// window from `percentRemaining`, regenerating at `nextRegenAt`.
pub fn parse_synthetic_quotas(value: &Value) -> Vec<LimitWindow> {
    let mut windows = Vec::new();
    let subscription = value.get("subscription");
    let used = value
        .get("rollingFiveHourLimit")
        .and_then(|rolling| {
            let max = rolling.get("max")?.as_f64()?;
            let remaining = rolling.get("remaining")?.as_f64()?;
            (max > 0.0).then(|| 100.0 - remaining / max * 100.0)
        })
        .or_else(|| {
            let limit = subscription?.get("limit")?.as_f64()?;
            let requests = subscription?
                .get("requests")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            (limit > 0.0).then(|| requests / limit * 100.0)
        });
    if let Some(used) = used {
        let resets_at = subscription
            .and_then(|subscription| subscription.get("renewsAt"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        windows.push(LimitWindow::new("5h", used, resets_at));
    }
    let weekly = value.get("weeklyTokenLimit");
    if let Some(remaining) = weekly
        .and_then(|weekly| weekly.get("percentRemaining"))
        .and_then(Value::as_f64)
    {
        let resets_at = weekly
            .and_then(|weekly| weekly.get("nextRegenAt"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        windows.push(LimitWindow::new("7d", 100.0 - remaining, resets_at));
    }
    windows
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
        "claude" => resolve_claude(cached().as_deref(), true),
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
/// `force` marks a refresh the user asked for, which skips the claude usage
/// endpoint's poll interval.
pub fn fetch_all(force: bool) -> LimitsSnapshot {
    let previous = cached();
    LimitsSnapshot {
        fetched_at: now_secs(),
        providers: vec![
            resolve_claude(previous.as_deref(), force),
            fetch_codex(),
            fetch_grok(),
            fetch_zai(),
            fetch_synthetic(),
        ],
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
/// `force` is `kanban limits --refresh`, as opposed to a snapshot that merely
/// aged out.
pub fn refresh_blocking(force: bool) -> Arc<LimitsSnapshot> {
    let snapshot = Arc::new(fetch_all(force));
    store(Arc::clone(&snapshot), true);
    snapshot
}

/// Start a background refresh when the cached snapshot is older than `ttl`
/// seconds. At most one fetch is in flight; the caller never blocks and keeps
/// drawing the previous snapshot until the new one lands.
pub fn refresh_if_stale(ttl: i64) {
    let ttl = next_backoff_ttl(ttl, CLAUDE_429_STREAK.load(Ordering::SeqCst)).max(1);
    if cached().is_some_and(|snapshot| snapshot.age(now_secs()) < ttl) {
        return;
    }
    if REFRESHING.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(|| {
        let snapshot = Arc::new(fetch_all(false));
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
    fn zai_quota_maps_credit_windows() {
        let value: Value = serde_json::from_str(
            r#"{"code":200,"msg":"Operation successful","data":{"limits":[
                {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":2000,"currentValue":115,"remaining":1884,"percentage":5,"nextResetTime":1786725207626},
                {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":10000,"currentValue":115,"remaining":9884,"percentage":1,"nextResetTime":1787311039997}],"level":"lite"},"success":true}"#,
        )
        .unwrap();

        let windows = parse_zai_quota(&value);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].remaining_percent, 94.25);
        assert_eq!(windows[0].resets_at, Some(1_786_725_207));
        assert_eq!(windows[1].label, "7d");
        assert_eq!(windows[1].remaining_percent, 98.85);
        assert_eq!(windows[1].resets_at, Some(1_787_311_039));
    }

    #[test]
    fn zai_quota_falls_back_to_percentage_and_remaining() {
        let value: Value = serde_json::from_str(
            r#"{"data":{"limits":[
                {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":34,"nextResetTime":0},
                {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":10000,"remaining":2500}]}}"#,
        )
        .unwrap();

        let windows = parse_zai_quota(&value);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].remaining_percent, 66.0);
        assert_eq!(windows[0].resets_at, None);
        assert_eq!(windows[1].remaining_percent, 25.0);
    }

    #[test]
    fn zai_quota_marks_the_monthly_mcp_sentinel_and_skips_empty_entries() {
        let value: Value = serde_json::from_str(
            r#"{"data":{"limits":[
                {"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":50,"percentage":5},
                {"type":"TIME_LIMIT","unit":2,"number":1,"percentage":10},
                {"type":"CREDIT_LIMIT","unit":3,"number":5}]}}"#,
        )
        .unwrap();

        let windows = parse_zai_quota(&value);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "mon");
        assert_eq!(windows[0].remaining_percent, 95.0);
        assert_eq!(windows[1].label, "window");
        assert_eq!(windows[1].remaining_percent, 90.0);
    }

    #[test]
    fn zai_quota_without_limits_is_empty() {
        let value: Value =
            serde_json::from_str(r#"{"code":200,"success":true,"data":{}}"#).unwrap();

        assert!(parse_zai_quota(&value).is_empty());
        let error: Value =
            serde_json::from_str(r#"{"code":1001,"msg":"unauthorized","success":false}"#).unwrap();
        assert!(parse_zai_quota(&error).is_empty());
    }

    #[test]
    fn zai_key_is_read_from_the_opencode_auth_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{"openai":{"type":"oauth"},"zai-coding-plan":{"type":"api","key":"e21665353cfd"}}"#,
        )
        .unwrap();

        assert_eq!(zai_api_key(&path), Some("e21665353cfd".to_string()));

        fs::write(&path, r#"{"openai":{"type":"oauth"}}"#).unwrap();
        assert_eq!(zai_api_key(&path), None);
        assert!(zai_api_key(&dir.path().join("nope.json")).is_none());
    }

    #[test]
    fn synthetic_quota_maps_both_windows() {
        let value: Value = serde_json::from_str(
            r#"{"subscription":{"limit":500,"requests":0,"renewsAt":"2026-08-14T16:51:44.399Z"},
                "search":{"hourly":{"limit":250,"requests":0,"renewsAt":"2026-08-14T12:51:44.400Z"}},
                "freeToolCalls":{"limit":0,"requests":0,"renewsAt":"2026-08-15T11:51:44.402Z"},
                "weeklyTokenLimit":{"nextRegenAt":"2026-08-14T12:10:54.000Z","percentRemaining":0,"maxCredits":"$24.00","remainingCredits":"$0.00","nextRegenCredits":"$0.48"},
                "rollingFiveHourLimit":{"nextTickAt":"2026-08-14T12:06:36.000Z","tickPercent":0.05,"remaining":455,"max":500,"limited":false}}"#,
        )
        .unwrap();

        let windows = parse_synthetic_quotas(&value);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].remaining_percent, 91.0);
        assert_eq!(windows[0].resets_at, Some(1_786_726_304));
        assert_eq!(windows[1].label, "7d");
        assert_eq!(windows[1].remaining_percent, 0.0);
        assert_eq!(windows[1].resets_at, Some(1_786_709_454));
    }

    #[test]
    fn synthetic_quota_falls_back_to_the_subscription_block() {
        let value: Value = serde_json::from_str(
            r#"{"subscription":{"limit":135,"requests":27,"renewsAt":"2025-09-21T14:36:14.288Z"}}"#,
        )
        .unwrap();

        let windows = parse_synthetic_quotas(&value);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].remaining_percent, 80.0);
        assert_eq!(windows[0].resets_at, Some(1_758_465_374));
    }

    #[test]
    fn synthetic_quota_reports_the_weekly_window_alone() {
        let value: Value = serde_json::from_str(
            r#"{"weeklyTokenLimit":{"percentRemaining":52.5,"nextRegenAt":"2026-08-20T00:00:00Z"},
                "rollingFiveHourLimit":{"remaining":500,"max":0}}"#,
        )
        .unwrap();

        let windows = parse_synthetic_quotas(&value);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "7d");
        assert_eq!(windows[0].remaining_percent, 52.5);
        assert_eq!(windows[0].resets_at, Some(1_787_184_000));
    }

    #[test]
    fn synthetic_quota_without_usable_windows_is_empty() {
        let empty: Value = serde_json::from_str(r#"{}"#).unwrap();
        assert!(parse_synthetic_quotas(&empty).is_empty());

        let zero_limits: Value = serde_json::from_str(
            r#"{"subscription":{"limit":0,"requests":0,"renewsAt":"2026-08-14T16:51:44.399Z"},
                "rollingFiveHourLimit":{"remaining":0,"max":0}}"#,
        )
        .unwrap();
        assert!(parse_synthetic_quotas(&zero_limits).is_empty());
    }

    #[test]
    fn synthetic_key_is_read_from_the_opencode_auth_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{"openai":{"type":"oauth"},"synthetic":{"type":"api","key":"sk-syn-9d1"}}"#,
        )
        .unwrap();

        assert_eq!(
            synthetic_key_from_store(&path),
            Some("sk-syn-9d1".to_string())
        );

        fs::write(&path, r#"{"synthetic":{"type":"api","key":""}}"#).unwrap();
        assert_eq!(synthetic_key_from_store(&path), None);
        fs::write(&path, r#"{"openai":{"type":"oauth"}}"#).unwrap();
        assert_eq!(synthetic_key_from_store(&path), None);
        assert!(synthetic_key_from_store(&dir.path().join("nope.json")).is_none());
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
            HttpError::Status(429).into_state(),
            ProviderState::Unavailable("HTTP 429".to_string())
        );
        assert_eq!(
            HttpError::Status(500).into_state(),
            ProviderState::Unavailable("HTTP 500".to_string())
        );
    }

    #[test]
    fn claude_backoff_ttl_doubles_per_429_then_caps() {
        assert_eq!(next_backoff_ttl(120, 0), 120);
        assert_eq!(next_backoff_ttl(120, 1), 240);
        assert_eq!(next_backoff_ttl(120, 2), 480);
        assert_eq!(next_backoff_ttl(120, 6), 7680);
        assert_eq!(next_backoff_ttl(120, 10), 7680);
        assert_eq!(next_backoff_ttl(0, 1), 2);
    }

    #[test]
    fn claude_429_keeps_the_last_ready_windows() {
        let previous = ProviderLimits {
            windows: vec![LimitWindow::new("5h", 34.0, Some(1_700_001_000))],
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };
        let fresh = ProviderLimits::new("claude", ProviderState::Unavailable("HTTP 429".into()));

        let (resolved, hit_429) = apply_claude_429_policy(Some(&previous), fresh);

        assert!(hit_429);
        assert_eq!(resolved, previous);
    }

    #[test]
    fn claude_429_without_prior_windows_stays_unavailable() {
        let fresh = ProviderLimits::new("claude", ProviderState::Unavailable("HTTP 429".into()));

        let (resolved, hit_429) = apply_claude_429_policy(None, fresh.clone());

        assert!(hit_429);
        assert_eq!(resolved, fresh);
    }

    #[test]
    fn claude_success_after_429_uses_the_fresh_windows() {
        let previous = ProviderLimits::new("claude", ProviderState::Unavailable("HTTP 429".into()));
        let fresh = ProviderLimits {
            windows: vec![LimitWindow::new("5h", 10.0, None)],
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };

        let (resolved, hit_429) = apply_claude_429_policy(Some(&previous), fresh.clone());

        assert!(!hit_429);
        assert_eq!(resolved, fresh);
    }

    #[test]
    fn claude_statusline_payload_maps_both_windows() {
        let value: Value = serde_json::from_str(
            r#"{"session_id":"ses","model":{"id":"claude-opus-5"},
                "rate_limits":{
                    "five_hour":{"used_percentage":34.5,"resets_at":1786708199},
                    "seven_day":{"used_percentage":3.0,"resets_at":1787312399},
                    "seven_day_sonnet":{"used_percentage":9.0,"resets_at":1787312399}}}"#,
        )
        .unwrap();

        let windows = parse_claude_statusline(&value);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].remaining_percent, 65.5);
        assert_eq!(windows[0].resets_at, Some(1_786_708_199));
        assert_eq!(windows[1].label, "7d");
        assert_eq!(windows[1].remaining_percent, 97.0);
    }

    #[test]
    fn claude_statusline_payload_tolerates_the_oauth_spellings() {
        let value: Value = serde_json::from_str(
            r#"{"rate_limits":{
                    "five_hour":{"utilization":50.0,"resets_at":"2026-08-14T11:49:59+00:00"},
                    "seven_day":null}}"#,
        )
        .unwrap();

        let windows = parse_claude_statusline(&value);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].remaining_percent, 50.0);
        assert_eq!(windows[0].resets_at, Some(1_786_708_199));
    }

    #[test]
    fn claude_statusline_payload_without_limits_is_empty() {
        let value: Value = serde_json::from_str(r#"{"session_id":"ses"}"#).unwrap();
        assert!(parse_claude_statusline(&value).is_empty());

        let nulls: Value =
            serde_json::from_str(r#"{"rate_limits":{"five_hour":null,"seven_day":null}}"#).unwrap();
        assert!(parse_claude_statusline(&nulls).is_empty());
    }

    #[test]
    fn claude_bridge_is_current_only_while_every_window_runs() {
        let fresh = ProviderLimits {
            windows: vec![LimitWindow::new("5h", 10.0, Some(2_000_000_000))],
            observed_at: Some(1_000_000_000),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };
        let past_reset_in_grace = ProviderLimits {
            windows: vec![LimitWindow::new("5h", 10.0, Some(1_000_000_100))],
            observed_at: Some(1_000_000_000),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };
        let past_reset_past_grace = ProviderLimits {
            observed_at: Some(1_000_000_000),
            ..past_reset_in_grace.clone()
        };
        // The regression this row shipped with: a spent 5h window next to a 7d
        // window that resets days out kept the whole file "current" forever.
        let spent_five_hour = ProviderLimits {
            windows: vec![
                LimitWindow::new("5h", 99.0, Some(1_000_010_000)),
                LimitWindow::new("7d", 5.0, Some(1_000_500_000)),
            ],
            observed_at: Some(1_000_000_000),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };

        assert!(claude_bridge_current(&fresh, 1_000_050_000));
        assert!(claude_bridge_current(&past_reset_in_grace, 1_000_001_000));
        assert!(!claude_bridge_current(
            &past_reset_past_grace,
            1_000_050_000
        ));
        assert!(claude_bridge_current(&spent_five_hour, 1_000_001_000));
        assert!(!claude_bridge_current(&spent_five_hour, 1_000_050_000));
    }

    #[test]
    fn claude_bridge_wins_over_unavailable_and_older_http() {
        let bridge = ProviderLimits {
            windows: vec![LimitWindow::new("5h", 30.0, Some(2_000_000_000))],
            observed_at: Some(1_500_000_000),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };
        let unavailable =
            ProviderLimits::new("claude", ProviderState::Unavailable("HTTP 429".into()));
        let older_http = ProviderLimits {
            windows: vec![LimitWindow::new("5h", 20.0, Some(2_000_000_000))],
            observed_at: Some(1_000_000_000),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };
        let newer_http = ProviderLimits {
            observed_at: Some(1_600_000_000),
            ..older_http.clone()
        };
        let now = 1_700_000_000;

        assert_eq!(
            merge_claude_sources(None, unavailable.clone(), now),
            unavailable
        );
        assert_eq!(
            merge_claude_sources(Some(bridge.clone()), unavailable, now),
            bridge
        );
        assert_eq!(
            merge_claude_sources(Some(bridge.clone()), older_http, now),
            bridge
        );
        assert_eq!(
            merge_claude_sources(Some(bridge), newer_http.clone(), now),
            newer_http
        );
    }

    #[test]
    fn claude_sources_merge_window_by_window() {
        let now = 1_700_000_000;
        // The bridge saw both windows 13 hours ago; its 5h window has since
        // rolled over, its 7d window is the only 7d reading anyone has.
        let bridge = ProviderLimits {
            windows: vec![
                LimitWindow::new("5h", 99.0, Some(now - 3_600)),
                LimitWindow::new("7d", 40.0, Some(now + 400_000)),
            ],
            observed_at: Some(now - 46_800),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };
        let http = ProviderLimits {
            windows: vec![LimitWindow::new("5h", 17.0, Some(now + 9_000))],
            observed_at: Some(now),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };

        let merged = merge_claude_sources(Some(bridge), http, now);

        assert_eq!(merged.windows.len(), 2);
        assert_eq!(merged.windows[0].label, "5h");
        assert_eq!(merged.windows[0].remaining_percent, 83.0);
        assert_eq!(merged.windows[1].label, "7d");
        assert_eq!(merged.windows[1].remaining_percent, 60.0);
        // The row is only as fresh as the 7d number it is still borrowing.
        assert_eq!(merged.observed_at, Some(now - 46_800));
    }

    #[test]
    fn a_bridge_window_supplements_rather_than_replaces_the_cached_ones() {
        let now = 1_700_000_000;
        // A statusline payload that only carried `five_hour`.
        let bridge = ProviderLimits {
            windows: vec![LimitWindow::new("5h", 12.0, Some(now + 9_000))],
            observed_at: Some(now - 60),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };
        let cached = ProviderLimits {
            windows: vec![
                LimitWindow::new("5h", 30.0, Some(now + 3_000)),
                LimitWindow::new("7d", 18.0, Some(now + 400_000)),
            ],
            observed_at: Some(now - 3_600),
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };

        let merged = merge_claude_sources(Some(bridge), cached, now);

        assert_eq!(merged.windows.len(), 2);
        assert_eq!(merged.windows[0].label, "5h");
        assert_eq!(merged.windows[0].remaining_percent, 88.0);
        assert_eq!(merged.windows[1].label, "7d");
        assert_eq!(merged.windows[1].remaining_percent, 82.0);
        assert_eq!(merged.observed_at, Some(now - 3_600));
    }

    #[test]
    fn a_running_window_beats_a_newer_reading_of_a_reset_one() {
        let now = 1_700_000_000;
        let running = LimitWindow::new("5h", 20.0, Some(now + 600));
        let reset = LimitWindow::new("5h", 90.0, Some(now - 600));

        assert!(prefers_window(&running, now - 9_000, &reset, now, now));
        assert!(!prefers_window(&reset, now, &running, now - 9_000, now));
        assert!(prefers_window(&running, now, &running, now - 60, now));
        assert!(!prefers_window(&running, now - 60, &running, now, now));
    }

    #[test]
    fn reset_windows_are_dropped_from_the_row() {
        let now = 1_700_000_000;
        let entry = ProviderLimits {
            windows: vec![
                LimitWindow::new("5h", 99.0, Some(now - 1)),
                LimitWindow::new("7d", 5.0, Some(now + 400_000)),
                LimitWindow::new("mon", 10.0, None),
            ],
            ..ProviderLimits::new("claude", ProviderState::Ready)
        };

        let live = entry.live_windows(now);

        assert_eq!(live.len(), 2);
        assert_eq!(live[0].label, "7d");
        // A window with no known reset time is not evidence of being stale.
        assert_eq!(live[1].label, "mon");
        assert!(entry.windows[0].is_expired(now));
        assert!(!entry.windows[2].is_expired(now));
    }

    #[test]
    fn claude_credentials_carry_the_refresh_token_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"tok-1","refreshToken":"ref-1",
                "expiresAt":1700000000000,"subscriptionType":"pro"}}"#,
        )
        .unwrap();

        let credentials = read_claude_credentials(&path).expect("credentials");

        assert_eq!(credentials.access_token, "tok-1");
        assert_eq!(credentials.refresh_token.as_deref(), Some("ref-1"));
        assert_eq!(credentials.expires_at, Some(1_700_000_000));
        assert!(claude_token_expired(&credentials, 1_700_000_000));
        assert!(claude_token_expired(&credentials, 1_699_999_800));
        assert!(!claude_token_expired(&credentials, 1_699_999_000));

        fs::write(&path, r#"{"claudeAiOauth":{"accessToken":""}}"#).unwrap();
        assert!(read_claude_credentials(&path).is_none());
        assert!(read_claude_credentials(&dir.path().join("nope.json")).is_none());
    }

    #[test]
    fn a_token_without_an_expiry_is_only_expired_when_the_server_says_so() {
        let credentials = ClaudeCredentials {
            access_token: "tok-1".to_string(),
            refresh_token: None,
            expires_at: None,
        };

        assert!(!claude_token_expired(&credentials, 1_700_000_000));
    }

    #[test]
    fn refresh_reply_keeps_the_refresh_token_the_server_left_out() {
        let current = ClaudeCredentials {
            access_token: "old".to_string(),
            refresh_token: Some("ref-1".to_string()),
            expires_at: Some(1_700_000_000),
        };
        let rotated: Value = serde_json::from_str(
            r#"{"access_token":"new","refresh_token":"ref-2","expires_in":28800}"#,
        )
        .unwrap();
        let kept: Value =
            serde_json::from_str(r#"{"access_token":"new","expires_in":28800}"#).unwrap();

        let rotated = parse_claude_token_response(&rotated, &current, 1_700_000_000).unwrap();
        assert_eq!(rotated.access_token, "new");
        assert_eq!(rotated.refresh_token.as_deref(), Some("ref-2"));
        assert_eq!(rotated.expires_at, Some(1_700_028_800));

        let kept = parse_claude_token_response(&kept, &current, 1_700_000_000).unwrap();
        assert_eq!(kept.refresh_token.as_deref(), Some("ref-1"));

        let refused: Value = serde_json::from_str(r#"{"error":"invalid_grant"}"#).unwrap();
        assert!(parse_claude_token_response(&refused, &current, 1_700_000_000).is_none());
    }

    #[test]
    fn renewed_tokens_are_written_back_without_losing_the_other_fields() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"ref-1","expiresAt":1700000000000,
                "scopes":["user:inference"],"subscriptionType":"pro"},"other":{"keep":true}}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        write_claude_credentials(
            &path,
            &ClaudeCredentials {
                access_token: "new".to_string(),
                refresh_token: Some("ref-2".to_string()),
                expires_at: Some(1_700_028_800),
            },
        );

        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let oauth = value.get("claudeAiOauth").unwrap();
        assert_eq!(oauth.get("accessToken").unwrap(), "new");
        assert_eq!(oauth.get("refreshToken").unwrap(), "ref-2");
        assert_eq!(oauth.get("expiresAt").unwrap(), 1_700_028_800_000_i64);
        assert_eq!(oauth.get("subscriptionType").unwrap(), "pro");
        assert_eq!(oauth.get("scopes").unwrap().as_array().unwrap().len(), 1);
        assert_eq!(value.get("other").unwrap().get("keep").unwrap(), true);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(read_claude_credentials(&path).unwrap().access_token, "new");
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
        assert!(read_claude_credentials(&dir.path().join("nope.json")).is_none());
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

        let credentials = read_claude_credentials(&path).expect("credentials");

        assert_eq!(credentials.access_token, "tok-1");
        assert_eq!(credentials.refresh_token, None);
        assert_eq!(credentials.expires_at, None);
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
