//! Input-provenance manifests (`.kanban/provenance/<session>.yaml`).
//!
//! The board records **what a delegated agent actually consumed**, harvested
//! from the backend's own machine transcript rather than from any prose the
//! agent chooses to write. Each agent run leaves an immutable manifest listing
//! the external inputs it pulled — files read *into* context (including those
//! opened through Bash, not only via the structured `Read` tool), files it
//! wrote, URLs fetched, MCP tools called — so a poisoned supply-chain link is
//! attributable to a concrete source. The emphasis is deliberately on *files*
//! (what fed the model), not on the shell command strings themselves. This is
//! telemetry, deliberately kept out of the task thread (which is what the
//! *next* prompt is built from).
//!
//! Every supported backend emits a parseable JSONL transcript on stdout —
//! claude via `--output-format stream-json`, opencode via `run --format json`,
//! and the pi family (pi/omp) via `--mode json` — captured to
//! `.kanban/logs/<session>.transcript.jsonl` by the launch wrapper. Their event
//! shapes differ but never collide on the top-level `type`, so one
//! [`render_stream_event`] renders all of them and each backend has its own
//! harvester. A backend with no parseable transcript simply gets no manifest.

use std::path::{Path, PathBuf};

use chrono::{NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::error::Result;
use crate::core::models::{Message, MessageKind};
use crate::core::storage::atomic_write_text;
use crate::core::timefmt;

/// The external inputs a single agent session consumed, in first-seen order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputManifest {
    pub session_id: String,
    pub backend: String,
    /// The backend's own session id (e.g. claude's `session_id`), when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_session_id: Option<String>,
    /// Repo-relative path of the deterministic prompt dump this run started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_dump: Option<String>,
    /// Files taken **into** context — the primary provenance signal. Read/Glob/
    /// Grep tools plus files named by read-class shell commands (`cat`, `sed -n`,
    /// `grep <pat> file`, …), so files pulled in through Bash count too, not just
    /// the ones opened via a structured tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<String>,
    /// Files the run **produced**: Edit/Write/patch tools and shell redirect
    /// targets (`> out`, `tee out`, `sed -i file`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writes: Vec<String>,
    /// External network inputs: WebFetch URLs and `search:<query>` for WebSearch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    /// MCP tools invoked, as `server:tool`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<String>,
    pub generated_at: String,
}

impl InputManifest {
    /// Compact `reads=.. writes=.. urls=.. mcp=..` summary for audit lines.
    pub fn summary(&self) -> String {
        format!(
            "reads={} writes={} urls={} mcp={}",
            self.reads.len(),
            self.writes.len(),
            self.urls.len(),
            self.mcp.len(),
        )
    }
}

/// Turn a backend transcript into an [`InputManifest`].
pub trait TranscriptHarvester {
    fn harvest(&self, transcript: &Path) -> Result<InputManifest>;
}

/// Harvester for claude's `--output-format stream-json` JSONL transcript.
pub struct ClaudeHarvester {
    pub session_id: String,
    pub prompt_dump: Option<String>,
    /// Repo root, used to canonicalize recorded paths to repo-relative form.
    pub root: PathBuf,
}

impl TranscriptHarvester for ClaudeHarvester {
    fn harvest(&self, transcript: &Path) -> Result<InputManifest> {
        let raw = std::fs::read_to_string(transcript)?;
        let mut manifest = InputManifest {
            session_id: self.session_id.clone(),
            backend: "claude".to_string(),
            prompt_dump: self.prompt_dump.clone(),
            generated_at: timefmt::format(&timefmt::now()),
            ..InputManifest::default()
        };
        for line in raw.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue; // stderr noise or partial lines — ignore for the manifest
            };
            match value.get("type").and_then(Value::as_str) {
                Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
                    manifest.backend_session_id = value
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                Some("assistant") => {
                    if let Some(content) = value
                        .get("message")
                        .and_then(|message| message.get("content"))
                        .and_then(Value::as_array)
                    {
                        for block in content {
                            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                                record_claude_tool_use(&mut manifest, block);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        canonicalize_paths(&mut manifest.reads, &self.root);
        canonicalize_paths(&mut manifest.writes, &self.root);
        Ok(manifest)
    }
}

/// Harvester for opencode's `run --format json` JSONL transcript. Tool calls
/// arrive as `{"type":"tool_use","sessionID":..,"part":{"tool":..,"state":{"input":..}}}`;
/// there is no init event, so the backend session id is read from the first
/// event that carries `sessionID`.
pub struct OpencodeHarvester {
    pub session_id: String,
    pub prompt_dump: Option<String>,
    /// Repo root, used to canonicalize recorded paths to repo-relative form.
    pub root: PathBuf,
}

impl TranscriptHarvester for OpencodeHarvester {
    fn harvest(&self, transcript: &Path) -> Result<InputManifest> {
        let raw = std::fs::read_to_string(transcript)?;
        let mut manifest = InputManifest {
            session_id: self.session_id.clone(),
            backend: "opencode".to_string(),
            prompt_dump: self.prompt_dump.clone(),
            generated_at: timefmt::format(&timefmt::now()),
            ..InputManifest::default()
        };
        for line in raw.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if manifest.backend_session_id.is_none()
                && let Some(session) = value.get("sessionID").and_then(Value::as_str)
            {
                manifest.backend_session_id = Some(session.to_string());
            }
            if value.get("type").and_then(Value::as_str) == Some("tool_use")
                && let Some(part) = value.get("part")
            {
                record_opencode_tool_use(&mut manifest, part);
            }
        }
        canonicalize_paths(&mut manifest.reads, &self.root);
        canonicalize_paths(&mut manifest.writes, &self.root);
        Ok(manifest)
    }
}

/// Harvester for the pi family (pi/omp) `--mode json` NDJSON stream. The backend
/// session id is the `session` event's `id`; tool calls arrive as `toolCall`
/// blocks inside each assistant `message_end`. `backend` is carried through so
/// the manifest names the actual engine (`pi` or `omp`).
pub struct PiFamilyHarvester {
    pub session_id: String,
    pub backend: String,
    pub prompt_dump: Option<String>,
    /// Repo root, used to canonicalize recorded paths to repo-relative form.
    pub root: PathBuf,
}

impl TranscriptHarvester for PiFamilyHarvester {
    fn harvest(&self, transcript: &Path) -> Result<InputManifest> {
        let raw = std::fs::read_to_string(transcript)?;
        let mut manifest = InputManifest {
            session_id: self.session_id.clone(),
            backend: self.backend.clone(),
            prompt_dump: self.prompt_dump.clone(),
            generated_at: timefmt::format(&timefmt::now()),
            ..InputManifest::default()
        };
        for line in raw.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            match value.get("type").and_then(Value::as_str) {
                Some("session") if manifest.backend_session_id.is_none() => {
                    manifest.backend_session_id =
                        value.get("id").and_then(Value::as_str).map(str::to_string);
                }
                Some("message_end")
                    if value
                        .get("message")
                        .and_then(|message| message.get("role"))
                        .and_then(Value::as_str)
                        == Some("assistant") =>
                {
                    if let Some(content) = value
                        .get("message")
                        .and_then(|message| message.get("content"))
                        .and_then(Value::as_array)
                    {
                        for block in content {
                            if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                                record_pi_tool_use(&mut manifest, block);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        canonicalize_paths(&mut manifest.reads, &self.root);
        canonicalize_paths(&mut manifest.writes, &self.root);
        Ok(manifest)
    }
}

/// Persist a manifest to `provenance_dir/<session>.yaml`.
pub fn write_manifest(provenance_dir: &Path, manifest: &InputManifest) -> Result<()> {
    std::fs::create_dir_all(provenance_dir)?;
    let path = provenance_dir.join(format!("{}.yaml", manifest.session_id));
    atomic_write_text(&path, &serde_yaml_ng::to_string(manifest)?)
}

/// Load a session's manifest if it exists (the TUI provenance panel).
pub fn load_manifest(provenance_dir: &Path, session_id: &str) -> Option<InputManifest> {
    let raw = std::fs::read_to_string(provenance_dir.join(format!("{session_id}.yaml"))).ok()?;
    serde_yaml_ng::from_str(&raw).ok()
}

/// Every input manifest referenced by a task's thread, in first-seen order.
/// `agent_step` audit lines carry `session=<id>`; each distinct session's
/// manifest is loaded once. Sessions without a manifest (non-claude backends,
/// crashes before any tool call) are simply absent.
pub fn collect_for_thread(provenance_dir: &Path, messages: &[Message]) -> Vec<InputManifest> {
    let mut seen = std::collections::HashSet::new();
    let mut manifests = Vec::new();
    for message in messages {
        if message.kind != MessageKind::AgentStep {
            continue;
        }
        for session in message
            .body
            .split_whitespace()
            .filter_map(|token| token.strip_prefix("session="))
        {
            if seen.insert(session.to_string())
                && let Some(manifest) = load_manifest(provenance_dir, session)
            {
                manifests.push(manifest);
            }
        }
    }
    manifests
}

/// Render harvested manifests as the plain text shown in the TUI inputs popup
/// (opened with `v`): one block per agent run listing the files it read and
/// wrote, URLs fetched, and MCP calls. Terminal-safety sanitization happens at
/// render time in the text pager, so this stays plain text. Empty when no run
/// left a manifest.
pub fn render_manifests(manifests: &[InputManifest]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for manifest in manifests {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!(
            "session {} [{}] {}",
            manifest.session_id,
            manifest.backend,
            manifest.summary()
        ));
        for (label, values) in [
            ("read", &manifest.reads),
            ("wrote", &manifest.writes),
            ("url", &manifest.urls),
            ("mcp", &manifest.mcp),
        ] {
            for value in values {
                lines.push(format!("  {label:<5} {value}"));
            }
        }
    }
    lines.join("\n")
}

/// A harvested manifest joined with its session's task ownership and lifetime
/// window — the inputs of the concurrent-write comparison.
pub struct SessionWrites<'a> {
    pub manifest: &'a InputManifest,
    pub task_id: &'a str,
    /// `[started_at, end)` — a still-active session's end is the current time.
    pub window: (NaiveDateTime, NaiveDateTime),
}

/// One concurrently-clobbered path: two sessions from **different** tasks,
/// whose lifetimes overlapped, both recorded the file in their `writes`.
/// Sessions of the same task are excluded — a task's own re-runs are expected
/// to touch the same files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOverlap {
    pub path: String,
    pub session_a: String,
    pub task_a: String,
    pub session_b: String,
    pub task_b: String,
}

/// Compare `writes` across sessions whose lifetimes overlapped. A finding
/// means the later writer silently clobbered the earlier one (last writer
/// wins), which until now was invisible. Pure detection: callers decide how
/// to report it.
pub fn overlapping_writes(sessions: &[SessionWrites<'_>]) -> Vec<WriteOverlap> {
    let mut out = Vec::new();
    for (i, a) in sessions.iter().enumerate() {
        for b in &sessions[i + 1..] {
            if a.manifest.session_id == b.manifest.session_id
                || a.task_id == b.task_id
                // Half-open windows: one session ending exactly when the
                // other starts is succession, not concurrency.
                || !(a.window.0 < b.window.1 && b.window.0 < a.window.1)
            {
                continue;
            }
            for path in &a.manifest.writes {
                if b.manifest.writes.contains(path) {
                    out.push(WriteOverlap {
                        path: path.clone(),
                        session_a: a.manifest.session_id.clone(),
                        task_a: a.task_id.to_string(),
                        session_b: b.manifest.session_id.clone(),
                        task_b: b.task_id.to_string(),
                    });
                }
            }
        }
    }
    out
}

/// A backend `type: error` event: the message shown in the log / thread,
/// whether crash-restart should still fire, and when a spent subscription
/// quota comes back. `isRetryable: false` (OpenCode credits/401) is a hard
/// failure — retrying it only disguises the cause as a queue backoff. Missing
/// flag defaults to retryable, matching unknown crashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamError {
    pub message: String,
    pub retryable: bool,
    /// Unix time the provider's exhausted usage window rolls over, when the
    /// error says so (HTTP 429 `usage_limit_reached`, as OpenAI answers an
    /// `openai/*` opencode run on a spent ChatGPT plan). Retrying before then
    /// can only fail again, so the restart waits for it instead of walking the
    /// blind backoff ladder.
    pub retry_at: Option<i64>,
}

/// Parse a backend `type: error` event. `None` for any other event, or an
/// error with no usable message.
pub fn stream_error(value: &Value) -> Option<StreamError> {
    if value.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let message = error_event_message(value)?;
    Some(StreamError {
        message,
        retryable: error_event_retryable(value),
        retry_at: error_event_retry_at(value),
    })
}

/// Error payload as the provider sent it (`error.data` on an opencode event),
/// which is where the HTTP status, the response body, and the response headers
/// live. Callers read provider-specific detail off it — the codex usage
/// headers, for one.
pub fn stream_error_data(value: &Value) -> Option<&Value> {
    value.get("error").and_then(|err| err.get("data"))
}

/// When the exhausted quota resets, from whichever field the provider used:
/// the `usage_limit_reached` body (`resets_at`, `resets_in_seconds`), the
/// codex usage headers, or a plain `retry-after`. Only a 429 answers — a
/// reset time on any other failure says nothing about when to retry.
fn error_event_retry_at(value: &Value) -> Option<i64> {
    let data = stream_error_data(value)?;
    let status = data
        .get("statusCode")
        .and_then(Value::as_i64)
        .or_else(|| data.get("status").and_then(Value::as_i64));
    if status != Some(429) {
        return None;
    }
    let now = Utc::now().timestamp();
    let body = data
        .get("responseBody")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let body_error = body.as_ref().and_then(|body| body.get("error"));
    let from_body = body_error.and_then(|error| {
        error.get("resets_at").and_then(Value::as_i64).or_else(|| {
            error
                .get("resets_in_seconds")
                .and_then(Value::as_i64)
                .map(|after| now + after)
        })
    });
    let headers = data.get("responseHeaders");
    from_body
        .or_else(|| {
            headers.and_then(|headers| {
                header_number(headers, "x-codex-primary-reset-at").or_else(|| {
                    header_number(headers, "x-codex-primary-reset-after-seconds")
                        .map(|after| now + after)
                })
            })
        })
        .or_else(|| {
            headers
                .and_then(|headers| header_number(headers, "retry-after"))
                .map(|after| now + after)
        })
        .filter(|at| *at > now)
}

/// A header value as a number. Headers arrive as strings; a backend that
/// pre-parsed them is accepted too.
fn header_number(headers: &Value, name: &str) -> Option<i64> {
    let value = headers.get(name)?;
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
}

fn error_event_message(value: &Value) -> Option<String> {
    let error = value.get("error");
    error
        .and_then(|err| err.get("data"))
        .and_then(|data| data.get("message"))
        .and_then(Value::as_str)
        .or_else(|| {
            error
                .and_then(|err| err.get("message"))
                .and_then(Value::as_str)
        })
        .or_else(|| value.get("message").and_then(Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_string)
}

fn error_event_retryable(value: &Value) -> bool {
    let error = value.get("error");
    error
        .and_then(|err| err.get("data"))
        .and_then(|data| data.get("isRetryable"))
        .and_then(Value::as_bool)
        .or_else(|| {
            error
                .and_then(|err| err.get("isRetryable"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(true)
}

/// Render one backend stream event (claude or opencode) as human-readable log
/// text, or `None` for events with nothing worth showing. Callers pass non-JSON
/// lines through untouched; this only decides recognized JSON events. The two
/// backends' `type` values are disjoint, so a single match handles both.
pub fn render_stream_event(value: &Value) -> Option<String> {
    match value.get("type").and_then(Value::as_str)? {
        // claude
        "assistant" => {
            let content = value.get("message")?.get("content")?.as_array()?;
            let mut out = String::new();
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            let text = text.trim();
                            if !text.is_empty() {
                                out.push_str(text);
                                out.push('\n');
                            }
                        }
                    }
                    Some("tool_use") => {
                        out.push_str("  → ");
                        out.push_str(&claude_tool_summary(block));
                        out.push('\n');
                    }
                    _ => {}
                }
            }
            let out = out.trim_end();
            (!out.is_empty()).then(|| out.to_string())
        }
        "result" => value
            .get("result")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|result| !result.is_empty())
            .map(str::to_string),
        // opencode
        "text" => value
            .get("part")?
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        "tool_use" => value
            .get("part")
            .map(|part| format!("  → {}", opencode_tool_summary(part))),
        "error" => stream_error(value).map(|err| format!("error: {}", err.message)),
        // pi family (pi/omp). Each assistant turn is finalized in one `message_end`
        // carrying its text and tool calls; `message_start` (placeholder) and
        // `turn_end` (a duplicate of the last message) are skipped to avoid noise.
        "message_end" => {
            let message = value.get("message")?;
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            let content = message.get("content")?.as_array()?;
            let mut out = String::new();
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            let text = text.trim();
                            if !text.is_empty() {
                                out.push_str(text);
                                out.push('\n');
                            }
                        }
                    }
                    Some("toolCall") => {
                        out.push_str("  → ");
                        out.push_str(&pi_tool_summary(block));
                        out.push('\n');
                    }
                    _ => {}
                }
            }
            let out = out.trim_end();
            (!out.is_empty()).then(|| out.to_string())
        }
        _ => None,
    }
}

/// `Read src/x.rs`, `Bash cargo test`, `mcp github:list_prs`, …
pub(crate) fn claude_tool_summary(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    if let Some(server_tool) = name.strip_prefix("mcp__") {
        return format!("mcp {}", server_tool.replacen("__", ":", 1));
    }
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    match str_field(
        &input,
        &[
            "file_path",
            "notebook_path",
            "path",
            "url",
            "query",
            "command",
            "pattern",
        ],
    ) {
        Some(detail) => format!("{name} {detail}"),
        None => name.to_string(),
    }
}

/// `read src/x.rs`, `bash cargo test`, `webfetch https://…`, …
pub(crate) fn opencode_tool_summary(part: &Value) -> String {
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
    let input = part
        .get("state")
        .and_then(|state| state.get("input"))
        .cloned()
        .unwrap_or(Value::Null);
    match str_field(
        &input,
        &["filePath", "file_path", "path", "url", "command", "pattern"],
    ) {
        Some(detail) => format!("{tool} {detail}"),
        None => tool.to_string(),
    }
}

/// `read src/x.rs`, `bash cargo test`, `todo Track release steps`, … for the pi
/// family (pi/omp). Tool inputs live under `arguments` and use `path`; when no
/// concrete target is present the short `i` intent label (e.g. omp's
/// `"Read release notes"`) is used before falling back to the bare tool name.
pub(crate) fn pi_tool_summary(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    let args = block.get("arguments").cloned().unwrap_or(Value::Null);
    if let Some(detail) = str_field(
        &args,
        &["file_path", "path", "filePath", "url", "pattern", "command"],
    ) {
        return format!("{name} {detail}");
    }
    match str_field(&args, &["i"]) {
        Some(intent) => format!("{name} {intent}"),
        None => name.to_string(),
    }
}

fn record_claude_tool_use(manifest: &mut InputManifest, block: &Value) {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
    if let Some(server_tool) = name.strip_prefix("mcp__") {
        push_unique(&mut manifest.mcp, server_tool.replacen("__", ":", 1));
        return;
    }
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    match name {
        "Read" | "NotebookRead" => {
            if let Some(path) = str_field(&input, &["file_path", "notebook_path", "path"]) {
                push_unique(&mut manifest.reads, path);
            }
        }
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            if let Some(path) = str_field(&input, &["file_path", "notebook_path", "path"]) {
                push_unique(&mut manifest.writes, path);
            }
        }
        "Glob" | "Grep" => {
            if let Some(path) = str_field(&input, &["path"]) {
                push_unique(&mut manifest.reads, path);
            } else if let Some(pattern) = str_field(&input, &["pattern"]) {
                push_unique(&mut manifest.reads, format!("pattern:{pattern}"));
            }
        }
        "WebFetch" => {
            if let Some(url) = str_field(&input, &["url"]) {
                push_unique(&mut manifest.urls, url);
            }
        }
        "WebSearch" => {
            if let Some(query) = str_field(&input, &["query"]) {
                push_unique(&mut manifest.urls, format!("search:{query}"));
            }
        }
        "Bash" => {
            if let Some(command) = str_field(&input, &["command"]) {
                record_bash_files(manifest, &command);
            }
        }
        _ => {}
    }
}

/// opencode tool names are lowercase and its inputs use `filePath`. MCP/plugin
/// tools are not namespaced (`mcp__…`) as in claude — they surface as flat
/// names (`websearch_web_search_exa`, `playwright_browser_navigate`,
/// `ssh-mcp_exec`, …). So the built-in toolset is classified explicitly and
/// anything outside it is treated as an external capability recorded under
/// `mcp`, which is exactly the supply-chain link worth auditing.
fn record_opencode_tool_use(manifest: &mut InputManifest, part: &Value) {
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or("");
    let input = part
        .get("state")
        .and_then(|state| state.get("input"))
        .cloned()
        .unwrap_or(Value::Null);
    match tool {
        "read" => {
            if let Some(path) = str_field(&input, &["filePath", "file_path", "path"]) {
                push_unique(&mut manifest.reads, path);
            }
        }
        "edit" | "write" | "patch" | "apply_patch" => {
            if let Some(path) = str_field(&input, &["filePath", "file_path", "path"]) {
                push_unique(&mut manifest.writes, path);
            }
        }
        "glob" | "grep" | "list" => {
            if let Some(path) = str_field(&input, &["path"]) {
                push_unique(&mut manifest.reads, path);
            } else if let Some(pattern) = str_field(&input, &["pattern"]) {
                push_unique(&mut manifest.reads, format!("pattern:{pattern}"));
            }
        }
        "webfetch" => {
            if let Some(url) = str_field(&input, &["url"]) {
                push_unique(&mut manifest.urls, url);
            }
        }
        "bash" => {
            if let Some(command) = str_field(&input, &["command"]) {
                record_bash_files(manifest, &command);
            }
        }
        // Built-in tools that consume no external supply-chain input.
        "todowrite" | "todoread" | "task" | "delegate_task" | "lsp_diagnostics"
        | "background_output" | "question" | "skill" | "invalid" | "webfetch_format" => {}
        // Any other tool is an external plugin/MCP capability.
        other if !other.is_empty() => push_unique(&mut manifest.mcp, other.to_string()),
        _ => {}
    }
}

/// pi-family (pi/omp) tool names are lowercase and their inputs live under
/// `arguments` keyed by `path`. Built-in tools that consume no external
/// supply-chain input are ignored; anything unrecognized is recorded under `mcp`
/// as an external capability, mirroring the opencode classifier.
fn record_pi_tool_use(manifest: &mut InputManifest, block: &Value) {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
    let input = block.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "read" => {
            if let Some(path) = str_field(&input, &["path", "file_path", "filePath"]) {
                push_unique(&mut manifest.reads, path);
            }
        }
        "edit" | "write" | "patch" | "apply_patch" => {
            if let Some(path) = str_field(&input, &["path", "file_path", "filePath"]) {
                push_unique(&mut manifest.writes, path);
            }
        }
        "glob" | "grep" | "list" => {
            if let Some(path) = str_field(&input, &["path"]) {
                push_unique(&mut manifest.reads, path);
            } else if let Some(pattern) = str_field(&input, &["pattern"]) {
                push_unique(&mut manifest.reads, format!("pattern:{pattern}"));
            }
        }
        "web_search" | "websearch" => {
            if let Some(query) = str_field(&input, &["query", "q"]) {
                push_unique(&mut manifest.urls, format!("search:{query}"));
            }
        }
        "webfetch" | "fetch" => {
            if let Some(url) = str_field(&input, &["url"]) {
                push_unique(&mut manifest.urls, url);
            }
        }
        "bash" => {
            if let Some(command) = str_field(&input, &["command"]) {
                record_bash_files(manifest, &command);
            }
        }
        // Built-in tools with no external supply-chain input.
        "todo" | "ask" | "subagent" | "question" | "eval" => {}
        other if !other.is_empty() => push_unique(&mut manifest.mcp, other.to_string()),
        _ => {}
    }
}

/// Shell commands whose file operands enter the model's context. The named
/// files are recorded as `reads`, putting Bash-driven file access on the same
/// footing as the structured `Read` tool.
const BASH_READ_CMDS: &[&str] = &[
    "cat", "head", "tail", "tac", "nl", "bat", "less", "more", "grep", "egrep", "fgrep", "rg",
    "ag", "ack", "sed", "awk", "diff", "xxd", "od", "hexdump", "strings",
];

/// Commands whose first non-flag operand is a pattern/script, not a file
/// (`grep <pat> file`, `sed <script> file`, `awk <prog> file`).
const BASH_PATTERN_FIRST: &[&str] = &["grep", "egrep", "fgrep", "rg", "ag", "ack", "sed", "awk"];

/// Mine a shell command line for the concrete files it touches: read-class
/// commands (`cat`, `sed -n`, `grep <pat> file`, …) contribute `reads`, and
/// redirect / in-place targets (`> out`, `tee out`, `sed -i file`) contribute
/// `writes`. Best-effort: this is a heuristic tokenizer, not a shell parser, so
/// it favours precision (skip anything ambiguous) over catching every case. The
/// env-guard segment opencode prepends (`export CI=…`) is not read-class and so
/// contributes nothing on its own.
fn record_bash_files(manifest: &mut InputManifest, command: &str) {
    // Split the line into pipeline/sequence segments; each is one simple command.
    let normalized = command
        .replace("&&", "\n")
        .replace("||", "\n")
        .replace([';', '|'], "\n");
    for segment in normalized.lines() {
        record_bash_segment(manifest, segment);
    }
}

fn record_bash_segment(manifest: &mut InputManifest, segment: &str) {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let Some((&head, rest)) = tokens.split_first() else {
        return;
    };
    let cmd = head.rsplit('/').next().unwrap_or(head);
    let read_class = BASH_READ_CMDS.contains(&cmd);
    let is_tee = cmd == "tee";
    // `sed -i` / `--in-place` rewrites its operand rather than reading it.
    let in_place = cmd == "sed"
        && rest
            .iter()
            .any(|t| *t == "-i" || t.starts_with("--in-place"));
    let mut pattern_pending = BASH_PATTERN_FIRST.contains(&cmd);

    let mut i = 0;
    while i < rest.len() {
        let tok = rest[i];
        match tok {
            ">" | ">>" | "1>" | "2>" | "&>" => {
                if let Some(path) = rest.get(i + 1).and_then(|t| clean_path(t)) {
                    push_unique(&mut manifest.writes, path);
                }
                i += 2;
                continue;
            }
            "<" => {
                if let Some(path) = rest.get(i + 1).and_then(|t| clean_path(t)) {
                    push_unique(&mut manifest.reads, path);
                }
                i += 2;
                continue;
            }
            _ if tok.starts_with('-') => {
                i += 1;
                continue;
            }
            _ => {}
        }
        // A bare operand of a read-class command.
        if pattern_pending {
            pattern_pending = false; // this operand is the search pattern/script
            i += 1;
            continue;
        }
        if let Some(path) = clean_path(tok) {
            if is_tee || in_place {
                push_unique(&mut manifest.writes, path);
            } else if read_class {
                push_unique(&mut manifest.reads, path);
            }
        }
        i += 1;
    }
}

/// Accept a token as a concrete file path, or reject it. Rejects flags, shell
/// expansions/globs/subshells, and non-path words, so only real filenames land
/// in the manifest.
fn clean_path(token: &str) -> Option<String> {
    let token = token.trim_matches(|c| c == '"' || c == '\'');
    if token.is_empty()
        || token == "."
        || token == ".."
        || token == "/dev/null"
        || token.contains(['$', '*', '`', '?'])
    {
        return None;
    }
    // Something path-shaped: a directory separator or a filename with an extension.
    (token.contains('/') || token.contains('.')).then(|| token.to_string())
}

pub(crate) fn str_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|found| !found.is_empty())
            .map(str::to_string)
    })
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// Reduce a recorded read/write path to a single canonical repo-relative
/// spelling. Different tools name the same file differently — the structured
/// `Read` tool emits absolute paths while Bash-mined operands (`sed`, `grep`,
/// `cat`) are repo-relative — so without this the same file lands twice under
/// two spellings. Absolute paths under `root` are made relative; a leading
/// `./` and any trailing `/` (e.g. a recursive-grep directory root like `src/`)
/// are stripped. `pattern:…` sentinels are not paths and pass through unchanged.
fn normalize_read_write_path(raw: &str, root: &Path) -> String {
    if raw.starts_with("pattern:") {
        return raw.to_string();
    }
    let path = Path::new(raw);
    let rel = path.strip_prefix(root).unwrap_or(path);
    let text = rel.to_string_lossy();
    let text = text.strip_prefix("./").unwrap_or(&text);
    text.trim_end_matches('/').to_string()
}

/// Normalize every entry to its canonical repo-relative form, then dedupe while
/// preserving first-seen order (same contract as [`push_unique`]).
fn canonicalize_paths(list: &mut Vec<String>, root: &Path) {
    let mut seen = std::collections::HashSet::new();
    let mut canonical = Vec::with_capacity(list.len());
    for entry in list.drain(..) {
        let normalized = normalize_read_write_path(&entry, root);
        if seen.insert(normalized.clone()) {
            canonical.push(normalized);
        }
    }
    *list = canonical;
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = r#"
{"type":"system","subtype":"init","session_id":"claude-abc-123","tools":["Read"],"mcp_servers":[{"name":"github"}]}
{"type":"assistant","message":{"content":[{"type":"text","text":"Looking at the code."},{"type":"tool_use","name":"Read","input":{"file_path":"src/core/operations.rs"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"..."}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"WebFetch","input":{"url":"https://example.com/doc"}},{"type":"tool_use","name":"Read","input":{"file_path":"src/core/operations.rs"}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/core/config.rs"}},{"type":"tool_use","name":"Bash","input":{"command":"sed -n '1,40p' src/agent/prompt.rs && cat README.md > out.txt"}},{"type":"tool_use","name":"mcp__github__list_prs","input":{"state":"open"}}]}}
not json at all
{"type":"result","subtype":"success","result":"done"}
"#;

    fn write_transcript(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ses.transcript.jsonl"), text).unwrap();
        dir
    }

    #[test]
    fn harvests_files_urls_commands_mcp() {
        let dir = write_transcript(TRANSCRIPT);
        let harvester = ClaudeHarvester {
            session_id: "ses-x".to_string(),
            prompt_dump: Some(".kanban/logs/ses-x.prompt.txt".to_string()),
            root: PathBuf::from("/repo"),
        };
        let manifest = harvester
            .harvest(&dir.path().join("ses.transcript.jsonl"))
            .unwrap();

        assert_eq!(manifest.session_id, "ses-x");
        assert_eq!(manifest.backend, "claude");
        assert_eq!(
            manifest.backend_session_id.as_deref(),
            Some("claude-abc-123")
        );
        // Read appears twice but is deduped and order-preserving; the Bash
        // `sed -n … prompt.rs` file is mined into reads (its `1,40p` script is
        // skipped), while the `> out.txt` redirect and the Edit are writes.
        assert_eq!(
            manifest.reads,
            vec!["src/core/operations.rs", "src/agent/prompt.rs", "README.md"]
        );
        assert_eq!(manifest.writes, vec!["src/core/config.rs", "out.txt"]);
        assert_eq!(manifest.urls, vec!["https://example.com/doc"]);
        assert_eq!(manifest.mcp, vec!["github:list_prs"]);
        assert_eq!(manifest.summary(), "reads=3 writes=2 urls=1 mcp=1");
    }

    #[test]
    fn bash_file_mining_classifies_reads_writes_and_skips_noise() {
        let mut manifest = InputManifest::default();
        // env-guard preamble contributes nothing; cat/grep operands are reads;
        // the grep pattern and bare flags are skipped; redirects are writes.
        record_bash_files(
            &mut manifest,
            "export CI=true EDITOR=:; cat src/a.rs | grep -n TODO src/b.rs",
        );
        record_bash_files(&mut manifest, "sed -i 's/x/y/' src/c.rs");
        record_bash_files(&mut manifest, "cargo build --release --locked");
        record_bash_files(&mut manifest, "git status --short");

        assert_eq!(manifest.reads, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(manifest.writes, vec!["src/c.rs"]);
        // Pure build/VCS commands name no files, so they leave no trace.
        assert!(manifest.urls.is_empty() && manifest.mcp.is_empty());
    }

    #[test]
    fn absolute_and_relative_spellings_of_one_file_collapse() {
        // Reproduces the TASK-145 defect: the Read tool names files by absolute
        // path while Bash-mined `sed`/`grep` operands are repo-relative, so the
        // same file was counted twice. After canonicalization it appears once as
        // repo-relative, and a recursive-grep directory root (`src/`) loses its
        // trailing slash rather than masquerading as a distinct file.
        let transcript = r#"
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"grep -rni drag src/"}},{"type":"tool_use","name":"Read","input":{"file_path":"/repo/src/tui/board.rs"}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"sed -n '1,20p' src/tui/board.rs"}},{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/src/tui/board.rs"}}]}}
"#;
        let dir = write_transcript(transcript);
        let manifest = ClaudeHarvester {
            session_id: "ses-dup".to_string(),
            prompt_dump: None,
            root: PathBuf::from("/repo"),
        }
        .harvest(&dir.path().join("ses.transcript.jsonl"))
        .unwrap();

        assert_eq!(manifest.reads, vec!["src", "src/tui/board.rs"]);
        assert_eq!(manifest.writes, vec!["src/tui/board.rs"]);
        assert_eq!(manifest.summary(), "reads=2 writes=1 urls=0 mcp=0");
    }

    #[test]
    fn manifest_round_trips_through_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = InputManifest {
            session_id: "ses-y".to_string(),
            backend: "claude".to_string(),
            reads: vec!["a.rs".to_string()],
            generated_at: "2026-07-21T00:00:00".to_string(),
            ..InputManifest::default()
        };
        write_manifest(dir.path(), &manifest).unwrap();
        let loaded = load_manifest(dir.path(), "ses-y").unwrap();
        assert_eq!(loaded, manifest);
        assert!(loaded.urls.is_empty());
    }

    #[test]
    fn renders_assistant_text_and_tool_lines() {
        let value: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi"},{"type":"tool_use","name":"Read","input":{"file_path":"a.rs"}}]}}"#,
        )
        .unwrap();
        let rendered = render_stream_event(&value).unwrap();
        assert!(rendered.contains("Hi"));
        assert!(rendered.contains("→ Read a.rs"));
    }

    #[test]
    fn drops_uninteresting_events() {
        let value: Value = serde_json::from_str(r#"{"type":"system","subtype":"init"}"#).unwrap();
        assert_eq!(render_stream_event(&value), None);
    }

    const OPENCODE_TRANSCRIPT: &str = r#"
{"type":"step_start","sessionID":"ses_abc","part":{"type":"step-start"}}
{"type":"text","sessionID":"ses_abc","part":{"type":"text","text":"Reading files."}}
{"type":"tool_use","sessionID":"ses_abc","part":{"type":"tool","tool":"read","state":{"status":"completed","input":{"filePath":"src/main.rs"}}}}
{"type":"tool_use","sessionID":"ses_abc","part":{"type":"tool","tool":"webfetch","state":{"input":{"url":"https://example.com/doc"}}}}
{"type":"tool_use","sessionID":"ses_abc","part":{"type":"tool","tool":"bash","state":{"input":{"command":"cat docs/spec.md"}}}}
{"type":"tool_use","sessionID":"ses_abc","part":{"type":"tool","tool":"write","state":{"input":{"filePath":"src/out.rs"}}}}
{"type":"tool_use","sessionID":"ses_abc","part":{"type":"tool","tool":"todowrite","state":{"input":{"todos":[]}}}}
{"type":"tool_use","sessionID":"ses_abc","part":{"type":"tool","tool":"playwright_browser_navigate","state":{"input":{"url":"https://ex.com"}}}}
{"type":"step_finish","sessionID":"ses_abc","part":{"type":"step-finish"}}
"#;

    #[test]
    fn opencode_harvester_classifies_builtin_and_plugin_tools() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ses.transcript.jsonl"), OPENCODE_TRANSCRIPT).unwrap();
        let manifest = OpencodeHarvester {
            session_id: "ses-oc".to_string(),
            prompt_dump: None,
            root: PathBuf::from("/repo"),
        }
        .harvest(&dir.path().join("ses.transcript.jsonl"))
        .unwrap();

        assert_eq!(manifest.backend, "opencode");
        assert_eq!(manifest.backend_session_id.as_deref(), Some("ses_abc"));
        // read tool + the `cat docs/spec.md` bash file are reads; write is a write.
        assert_eq!(manifest.reads, vec!["src/main.rs", "docs/spec.md"]);
        assert_eq!(manifest.writes, vec!["src/out.rs"]);
        assert_eq!(manifest.urls, vec!["https://example.com/doc"]);
        // todowrite is a no-op input; the unknown plugin tool is an external link.
        assert_eq!(manifest.mcp, vec!["playwright_browser_navigate"]);
    }

    #[test]
    fn renders_opencode_text_and_tool_events() {
        let text: Value =
            serde_json::from_str(r#"{"type":"text","part":{"type":"text","text":"Hi there"}}"#)
                .unwrap();
        assert_eq!(render_stream_event(&text).as_deref(), Some("Hi there"));

        let tool: Value = serde_json::from_str(
            r#"{"type":"tool_use","part":{"type":"tool","tool":"read","state":{"input":{"filePath":"a.rs"}}}}"#,
        )
        .unwrap();
        assert_eq!(render_stream_event(&tool).as_deref(), Some("  → read a.rs"));

        let step: Value = serde_json::from_str(r#"{"type":"step_finish","part":{}}"#).unwrap();
        assert_eq!(render_stream_event(&step), None);
    }

    #[test]
    fn renders_opencode_error_and_parses_retryable_flag() {
        let fatal: Value = serde_json::from_str(
            r#"{"type":"error","error":{"name":"APIError","data":{"message":"Insufficient balance.","statusCode":401,"isRetryable":false}}}"#,
        )
        .unwrap();
        assert_eq!(
            render_stream_event(&fatal).as_deref(),
            Some("error: Insufficient balance.")
        );
        assert_eq!(
            stream_error(&fatal),
            Some(StreamError {
                message: "Insufficient balance.".to_string(),
                retryable: false,
                retry_at: None,
            })
        );

        let retryable: Value =
            serde_json::from_str(r#"{"type":"error","error":{"message":"rate limited"}}"#).unwrap();
        assert_eq!(
            stream_error(&retryable),
            Some(StreamError {
                message: "rate limited".to_string(),
                retryable: true,
                retry_at: None,
            })
        );
    }

    /// A 429 from an `openai/*` opencode run: the body names when the spent
    /// ChatGPT window rolls over, and the run must wait for exactly that.
    #[test]
    fn usage_limit_error_carries_the_quota_reset_time() {
        let resets_at = Utc::now().timestamp() + 7_335;
        let event: Value = serde_json::from_str(&format!(
            r#"{{"type":"error","error":{{"name":"APIError","data":{{"message":"The usage limit has been reached","statusCode":429,"isRetryable":true,"responseBody":"{{\"error\":{{\"type\":\"usage_limit_reached\",\"message\":\"The usage limit has been reached\",\"resets_at\":{resets_at},\"resets_in_seconds\":7335}}}}"}}}}}}"#
        ))
        .unwrap();

        let err = stream_error(&event).expect("error event");
        assert!(err.retryable);
        assert_eq!(err.retry_at, Some(resets_at));
    }

    /// No body: the codex usage headers carry the same reset, and a bare
    /// `retry-after` is the last fallback.
    #[test]
    fn usage_limit_reset_falls_back_to_the_response_headers() {
        let resets_at = Utc::now().timestamp() + 6_751;
        let from_headers: Value = serde_json::from_str(&format!(
            r#"{{"type":"error","error":{{"data":{{"message":"The usage limit has been reached","statusCode":429,"responseHeaders":{{"x-codex-primary-reset-at":"{resets_at}","x-codex-primary-used-percent":"100"}}}}}}}}"#
        ))
        .unwrap();
        assert_eq!(
            stream_error(&from_headers).and_then(|err| err.retry_at),
            Some(resets_at)
        );

        let retry_after: Value = serde_json::from_str(
            r#"{"type":"error","error":{"data":{"message":"slow down","statusCode":429,"responseHeaders":{"retry-after":"600"}}}}"#,
        )
        .unwrap();
        let at = stream_error(&retry_after)
            .and_then(|err| err.retry_at)
            .expect("retry-after");
        assert!((at - Utc::now().timestamp() - 600).abs() <= 2, "{at}");
    }

    /// Only a 429 names a retry moment. A reset time on any other failure
    /// says nothing about when the failure clears, so the normal backoff runs.
    #[test]
    fn non_rate_limit_errors_have_no_retry_time() {
        let server_error: Value = serde_json::from_str(
            r#"{"type":"error","error":{"data":{"message":"boom","statusCode":500,"responseHeaders":{"x-codex-primary-reset-at":"9999999999"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            stream_error(&server_error).and_then(|err| err.retry_at),
            None
        );

        // A reset already in the past is a clock disagreement, not a retry.
        let stale: Value = serde_json::from_str(
            r#"{"type":"error","error":{"data":{"message":"limit","statusCode":429,"responseHeaders":{"x-codex-primary-reset-at":"1000"}}}}"#,
        )
        .unwrap();
        assert_eq!(stream_error(&stale).and_then(|err| err.retry_at), None);
    }

    // pi family (pi/omp) `--mode json` stream: tool calls live under
    // `arguments`; the backend session id is the `session` event's `id`.
    const PI_FAMILY_TRANSCRIPT: &str = r#"
{"type":"session","version":3,"id":"019f-omp-1","cwd":"/repo"}
{"type":"message_start","message":{"role":"assistant","content":[]}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","name":"read","arguments":{"path":"src/main.rs"}},{"type":"toolCall","name":"bash","arguments":{"command":"cat docs/spec.md"}}]}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","name":"write","arguments":{"path":"src/out.rs"}},{"type":"toolCall","name":"todo","arguments":{"op":"done","task":"a"}},{"type":"toolCall","name":"web_search","arguments":{"query":"ratatui"}},{"type":"toolCall","name":"playwright_navigate","arguments":{"url":"https://ex.com"}}]}}
"#;

    #[test]
    fn pi_family_harvester_classifies_tools_and_session_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ses.transcript.jsonl"),
            PI_FAMILY_TRANSCRIPT,
        )
        .unwrap();
        let manifest = PiFamilyHarvester {
            session_id: "ses-omp".to_string(),
            backend: "omp".to_string(),
            prompt_dump: None,
            root: PathBuf::from("/repo"),
        }
        .harvest(&dir.path().join("ses.transcript.jsonl"))
        .unwrap();

        assert_eq!(manifest.backend, "omp");
        assert_eq!(manifest.backend_session_id.as_deref(), Some("019f-omp-1"));
        assert_eq!(manifest.reads, vec!["src/main.rs", "docs/spec.md"]);
        assert_eq!(manifest.writes, vec!["src/out.rs"]);
        assert_eq!(manifest.urls, vec!["search:ratatui"]);
        // todo is a built-in no-op; the unknown tool is an external link.
        assert_eq!(manifest.mcp, vec!["playwright_navigate"]);
    }

    #[test]
    fn renders_pi_family_message_end() {
        let value: Value = serde_json::from_str(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Done"},{"type":"toolCall","name":"edit","arguments":{"path":"a.rs"}}]}}"#,
        )
        .unwrap();
        let rendered = render_stream_event(&value).unwrap();
        assert!(rendered.contains("Done"));
        assert!(rendered.contains("→ edit a.rs"));

        // A user echo and the duplicate turn_end must render nothing.
        let user: Value = serde_json::from_str(
            r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
        )
        .unwrap();
        assert_eq!(render_stream_event(&user), None);
    }

    #[test]
    fn collect_for_thread_loads_referenced_manifests() {
        use crate::core::models::{Message, MessageKind, MessageRole};

        let dir = tempfile::tempdir().unwrap();
        write_manifest(
            dir.path(),
            &InputManifest {
                session_id: "ses-1".to_string(),
                backend: "claude".to_string(),
                reads: vec!["a.rs".to_string()],
                generated_at: "t".to_string(),
                ..InputManifest::default()
            },
        )
        .unwrap();

        let step = Message::new(
            "MSG-001",
            MessageRole::System,
            MessageKind::AgentStep,
            "■ exit session=ses-1 code=0 outcome=Closed",
        );
        // A non-step message referencing a session must be ignored, and a step
        // for a session with no manifest on disk must be skipped silently.
        let noise = Message::new(
            "MSG-002",
            MessageRole::Agent,
            MessageKind::Context,
            "worked on session=ses-999",
        );
        let missing = Message::new(
            "MSG-003",
            MessageRole::System,
            MessageKind::AgentStep,
            "▶ launch session=ses-2 backend=opencode",
        );

        let manifests = collect_for_thread(dir.path(), &[step, noise, missing]);
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].session_id, "ses-1");
    }

    fn overlap_manifest(id: &str, writes: &[&str]) -> InputManifest {
        InputManifest {
            session_id: id.to_string(),
            backend: "claude".to_string(),
            writes: writes.iter().map(|w| w.to_string()).collect(),
            generated_at: "t".to_string(),
            ..InputManifest::default()
        }
    }

    fn at(secs: i64) -> NaiveDateTime {
        chrono::DateTime::from_timestamp(secs, 0)
            .unwrap()
            .naive_utc()
    }

    fn view<'a>(
        manifest: &'a InputManifest,
        task_id: &'a str,
        window: (NaiveDateTime, NaiveDateTime),
    ) -> SessionWrites<'a> {
        SessionWrites {
            manifest,
            task_id,
            window,
        }
    }

    #[test]
    fn overlapping_writes_flags_cross_task_concurrent_clobber() {
        let a = overlap_manifest("ses-a", &["src/lib.rs", "src/a.rs"]);
        let b = overlap_manifest("ses-b", &["src/lib.rs"]);
        let c = overlap_manifest("ses-c", &["src/c.rs"]);
        let views = [
            view(&a, "TASK-1", (at(0), at(100))),
            view(&b, "TASK-2", (at(50), at(150))),
            // Overlaps in time with a, but writes nothing a wrote.
            view(&c, "TASK-3", (at(10), at(20))),
        ];
        let findings = overlapping_writes(&views);
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.path, "src/lib.rs");
        assert_eq!(finding.task_a, "TASK-1");
        assert_eq!(finding.session_a, "ses-a");
        assert_eq!(finding.task_b, "TASK-2");
        assert_eq!(finding.session_b, "ses-b");
        // Every shared path of the pair is reported, in the first manifest's
        // write order.
        let b2 = overlap_manifest("ses-b2", &["src/a.rs", "src/lib.rs"]);
        let views = [
            view(&a, "TASK-1", (at(0), at(100))),
            view(&b2, "TASK-2", (at(50), at(150))),
        ];
        let paths: Vec<String> = overlapping_writes(&views)
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(paths, vec!["src/lib.rs", "src/a.rs"]);
    }

    #[test]
    fn overlapping_writes_ignores_same_task_succession_and_non_writes() {
        let a = overlap_manifest("ses-a", &["src/lib.rs"]);
        let same_task = overlap_manifest("ses-b", &["src/lib.rs"]);
        let later = overlap_manifest("ses-c", &["src/lib.rs"]);
        let views = [
            // Same task re-runs are expected to touch the same files.
            view(&a, "TASK-1", (at(0), at(100))),
            view(&same_task, "TASK-1", (at(50), at(150))),
        ];
        assert!(overlapping_writes(&views).is_empty());

        let views = [
            // Succession, not concurrency: b starts exactly when a ends.
            view(&a, "TASK-1", (at(0), at(100))),
            view(&later, "TASK-2", (at(100), at(200))),
        ];
        assert!(overlapping_writes(&views).is_empty());

        let reader = overlap_manifest("ses-e", &[]);
        let views = [
            // Overlap on reads only — no write, no finding.
            view(&a, "TASK-1", (at(0), at(100))),
            view(&reader, "TASK-2", (at(50), at(150))),
        ];
        assert!(overlapping_writes(&views).is_empty());
    }
}
