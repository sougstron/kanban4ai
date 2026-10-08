//! Live agent telemetry derived from the backend's machine transcript
//! (`.kanban/logs/<session>.transcript.jsonl`).
//!
//! Where [`crate::core::provenance`] harvests *what a run consumed* once at
//! exit, this module answers *how a run is going right now* — todo progress,
//! tokens spent, cost, and the last tool it invoked — cheaply enough to read on
//! every TUI tick. Nothing here is persisted: the transcript is the single
//! source of truth, so a re-read always reflects the latest state and no new
//! on-disk record (or fixture surface) is introduced. The parsing mirrors the
//! provenance harvesters and reuses their tool-summary helpers so the two stay
//! in lock-step on backend event shapes.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::provenance::{
    claude_tool_summary, codex_tool_summary, opencode_tool_summary, pi_tool_summary,
};
use crate::core::session::{SessionManager, estimate_session_tokens};

/// A snapshot of an agent run's progress, all fields best-effort and independent
/// (a backend may report some but not others). Empty (`has_data() == false`) when
/// no transcript exists or nothing parseable was found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionProgress {
    /// Approximate tokens spent so far (see [`parse_claude`] for the
    /// live-vs-final accounting). `None` when neither the transcript nor the
    /// log yielded a number.
    pub tokens: Option<i64>,
    /// Total cost in USD, reported by claude's final `result` event and summed
    /// per-turn for the pi family (pi/omp). `None` for backends that omit it.
    pub cost_usd: Option<f64>,
    /// Completed items in the agent's todo list (claude/opencode `TodoWrite`, or
    /// omp's replayed `todo` tool).
    pub todos_done: usize,
    /// Total items in that list; `0` means the agent has posted no todos (always
    /// the case for pi, which has no todo tool).
    pub todos_total: usize,
    /// Human-readable summary of the last tool call (`Edit src/x.rs`, …).
    pub last_activity: Option<String>,
    /// Cumulative input/output/cache split for the per-task analytics panel.
    /// Unlike [`Self::tokens`] it counts cached prompt tokens too, so the cache
    /// hit rate can be derived. `None` when the transcript carried no usage.
    pub breakdown: Option<TokenBreakdown>,
}

/// Cumulative token usage of one or more runs, normalized across backends.
/// `input` is the *whole* prompt side — uncached, cache reads and cache writes
/// alike — so `cache_read / input` is the cache hit rate whichever convention
/// the backend reports in (claude/pi split the cached part out of `input`,
/// codex folds it in).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenBreakdown {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl TokenBreakdown {
    /// Build from a split where `uncached` excludes both cache fields.
    fn from_split(uncached: i64, output: i64, cache_read: i64, cache_write: i64) -> Self {
        TokenBreakdown {
            input: uncached + cache_read + cache_write,
            output,
            cache_read,
            cache_write,
        }
    }

    pub fn add(&mut self, other: &TokenBreakdown) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0
    }

    /// Share of prompt tokens served from cache, in percent; `None` with no
    /// input to divide by.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        (self.input > 0).then(|| self.cache_read as f64 * 100.0 / self.input as f64)
    }
}

fn int_field(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// claude/grok `usage` object (Messages API: `input_tokens` excludes the
/// `cache_*_input_tokens` fields).
fn claude_breakdown(usage: &Value) -> TokenBreakdown {
    TokenBreakdown::from_split(
        int_field(usage, "input_tokens"),
        int_field(usage, "output_tokens"),
        int_field(usage, "cache_read_input_tokens"),
        int_field(usage, "cache_creation_input_tokens"),
    )
}

impl SessionProgress {
    /// Whether anything worth displaying was found.
    pub fn has_data(&self) -> bool {
        self.tokens.is_some()
            || self.cost_usd.is_some()
            || self.todos_total > 0
            || self.last_activity.is_some()
    }

    /// `Some((done, total))` when the agent has posted a todo list, else `None`.
    pub fn todos(&self) -> Option<(usize, usize)> {
        (self.todos_total > 0).then_some((self.todos_done, self.todos_total))
    }
}

/// Read progress for one session. `backend` selects the transcript dialect:
/// `codex`, `opencode`, the pi family (`pi`/`omp`), or claude (the default for
/// anything else). Falls back to the log-scraping [`estimate_session_tokens`] for the
/// token count when the transcript is absent or reported no usage.
///
/// The TUI calls this on every tick for each live agent, and pi-family
/// transcripts grow to tens of megabytes of streaming deltas, so the parse is
/// incremental: see [`TranscriptCache`].
pub fn read_session_progress(
    project_path: &Path,
    session_id: &str,
    backend: &str,
) -> SessionProgress {
    let mut progress = SessionProgress::default();
    if SessionManager::validate_session_id(session_id).is_err() {
        return progress;
    }
    let transcript = project_path
        .join(".kanban")
        .join("logs")
        .join(format!("{session_id}.transcript.jsonl"));
    if let Some(found) = TranscriptCache::read(&transcript, Dialect::of(backend)) {
        progress = found;
    }
    if progress.tokens.is_none() {
        progress.tokens = estimate_session_tokens(project_path, session_id);
    }
    progress
}

/// Process-wide memo of each transcript's parser state and how far into the
/// file it has read. Transcripts are append-only, so a re-read only parses the
/// bytes written since the last one. A file that shrank or whose already-read
/// tail no longer matches was rewritten, and is parsed again from the start.
struct TranscriptCache;

/// Bytes before the read offset compared on every re-read to detect a rewrite.
const TAIL_PROBE: usize = 64;

struct CachedTranscript {
    dialect: Dialect,
    /// Offset just past the last complete line fed to `parser`.
    offset: u64,
    /// The file's bytes right before `offset` (up to [`TAIL_PROBE`]).
    tail: Vec<u8>,
    parser: Parser,
}

impl TranscriptCache {
    fn read(path: &Path, dialect: Dialect) -> Option<SessionProgress> {
        use std::sync::{Mutex, OnceLock};
        static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedTranscript>>> = OnceLock::new();
        let mut file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let reusable = cache
            .get(path)
            .is_some_and(|entry| entry.dialect == dialect && entry.still_prefix_of(&mut file, len));
        if !reusable {
            cache.insert(
                path.to_path_buf(),
                CachedTranscript {
                    dialect,
                    offset: 0,
                    tail: Vec::new(),
                    parser: Parser::new(dialect),
                },
            );
        }
        let entry = cache.get_mut(path)?;
        let mut fresh = Vec::new();
        file.seek(SeekFrom::Start(entry.offset)).ok()?;
        file.read_to_end(&mut fresh).ok()?;
        let complete = fresh
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        for line in fresh[..complete].split(|byte| *byte == b'\n') {
            entry.parser.feed_line(line);
        }
        if complete > 0 {
            entry.offset += complete as u64;
            let mut tail = std::mem::take(&mut entry.tail);
            tail.extend_from_slice(&fresh[..complete]);
            let keep = tail.len().saturating_sub(TAIL_PROBE);
            entry.tail = tail.split_off(keep);
        }
        // A trailing line without its newline yet is either still being
        // written (and fails to parse) or the file's unterminated last line;
        // fold it into this answer only, so it is re-read once complete.
        let partial = &fresh[complete..];
        if partial.iter().all(u8::is_ascii_whitespace) {
            Some(entry.parser.snapshot())
        } else {
            let mut parser = entry.parser.clone();
            parser.feed_line(partial);
            Some(parser.snapshot())
        }
    }
}

impl CachedTranscript {
    /// Whether the file still begins with the bytes this entry already parsed.
    fn still_prefix_of(&self, file: &mut File, len: u64) -> bool {
        if len < self.offset {
            return false;
        }
        let start = self.offset - self.tail.len() as u64;
        let mut probe = vec![0; self.tail.len()];
        file.seek(SeekFrom::Start(start)).is_ok()
            && file.read_exact(&mut probe).is_ok()
            && probe == self.tail
    }
}

/// Transcript format, chosen by backend name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Claude,
    Codex,
    Opencode,
    PiFamily,
}

impl Dialect {
    fn of(backend: &str) -> Self {
        match backend {
            "codex" => Dialect::Codex,
            "opencode" => Dialect::Opencode,
            "pi" | "omp" => Dialect::PiFamily,
            // Grok Build's streaming-messages-json is the claude stream shape.
            _ => Dialect::Claude,
        }
    }
}

/// Running parse of one transcript, fed a line at a time.
#[derive(Debug, Clone)]
enum Parser {
    Claude(ClaudeParser),
    Codex(SessionProgress),
    Opencode(OpencodeParser),
    PiFamily(PiParser),
}

impl Parser {
    fn new(dialect: Dialect) -> Self {
        match dialect {
            Dialect::Claude => Parser::Claude(ClaudeParser::default()),
            Dialect::Codex => Parser::Codex(SessionProgress::default()),
            Dialect::Opencode => Parser::Opencode(OpencodeParser::default()),
            Dialect::PiFamily => Parser::PiFamily(PiParser::default()),
        }
    }

    #[cfg(test)]
    fn feed_str(&mut self, raw: &str) {
        for line in raw.lines() {
            self.feed_line(line.as_bytes());
        }
    }

    /// Parse one line. A line that cannot carry an event this dialect reads
    /// is skipped before JSON decoding: streaming deltas (pi's
    /// `message_update`) are the bulk of a transcript and never count.
    fn feed_line(&mut self, line: &[u8]) {
        let Ok(line) = std::str::from_utf8(line) else {
            return;
        };
        let relevant = match self {
            Parser::Claude(_) => line.contains("\"assistant\"") || line.contains("\"result\""),
            Parser::Codex(_) => line.contains("turn.completed") || line.contains("item.completed"),
            Parser::Opencode(_) => line.contains("\"part\""),
            Parser::PiFamily(_) => line.contains("\"message_end\""),
        };
        if !relevant {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        match self {
            Parser::Claude(parser) => parser.feed(&value),
            Parser::Codex(progress) => feed_codex(&value, progress),
            Parser::Opencode(parser) => parser.feed(&value),
            Parser::PiFamily(parser) => parser.feed(&value),
        }
    }

    fn snapshot(&self) -> SessionProgress {
        match self {
            Parser::Claude(parser) => parser.snapshot(),
            Parser::Codex(progress) => progress.clone(),
            Parser::Opencode(parser) => parser.snapshot(),
            Parser::PiFamily(parser) => parser.snapshot(),
        }
    }
}

/// Apply a `TodoWrite`/`todowrite` `todos` array (last write wins).
fn apply_todos(todos: &[Value], progress: &mut SessionProgress) {
    progress.todos_total = todos.len();
    progress.todos_done = todos
        .iter()
        .filter(|todo| todo.get("status").and_then(Value::as_str) == Some("completed"))
        .count();
}

/// Sum `input_tokens + output_tokens` from a `usage` object, ignoring the
/// (potentially huge) cache fields so the number tracks real work. `None` when
/// neither field is present.
fn usage_input_output(usage: &Value) -> Option<i64> {
    let input = usage.get("input_tokens").and_then(Value::as_i64);
    let output = usage.get("output_tokens").and_then(Value::as_i64);
    match (input, output) {
        (None, None) => None,
        _ => Some(input.unwrap_or(0) + output.unwrap_or(0)),
    }
}

/// Parse claude's `--output-format stream-json` transcript.
#[cfg(test)]
fn parse_claude(raw: &str, progress: &mut SessionProgress) {
    let mut parser = Parser::new(Dialect::Claude);
    parser.feed_str(raw);
    *progress = parser.snapshot();
}

/// Running state of a claude stream-json parse.
///
/// Mid-run there is no cumulative total, so tokens are approximated as
/// `last_input + Σ output`: output tokens are per-turn and never overlap, while
/// the input count is the (growing) context of the latest turn. Once the final
/// `result` event arrives its cumulative `usage` supersedes the estimate.
#[derive(Debug, Clone, Default)]
struct ClaudeParser {
    progress: SessionProgress,
    sum_output: i64,
    last_input: i64,
    saw_assistant_usage: bool,
    result_tokens: Option<i64>,
    // One API call streams as several `assistant` events (one per content
    // block) repeating the same message id and usage, so the breakdown keeps
    // the last usage per id instead of summing every event.
    message_usage: HashMap<String, TokenBreakdown>,
    anonymous_usage: TokenBreakdown,
    result_breakdown: Option<TokenBreakdown>,
}

impl ClaudeParser {
    fn feed(&mut self, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let message = value.get("message");
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    self.last_input = usage
                        .get("input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(self.last_input);
                    self.sum_output += usage
                        .get("output_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    self.saw_assistant_usage = true;
                    let breakdown = claude_breakdown(usage);
                    match message.and_then(|m| m.get("id")).and_then(Value::as_str) {
                        Some(id) => {
                            self.message_usage.insert(id.to_string(), breakdown);
                        }
                        None => self.anonymous_usage.add(&breakdown),
                    }
                }
                if let Some(content) = message
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                            continue;
                        }
                        if matches!(
                            block.get("name").and_then(Value::as_str),
                            Some("TodoWrite" | "todo_write")
                        ) && let Some(todos) = block
                            .get("input")
                            .and_then(|input| input.get("todos"))
                            .and_then(Value::as_array)
                        {
                            apply_todos(todos, &mut self.progress);
                        }
                        self.progress.last_activity = Some(claude_tool_summary(block));
                    }
                }
            }
            Some("result") => {
                if let Some(usage) = value.get("usage") {
                    if let Some(tokens) = usage_input_output(usage) {
                        self.result_tokens = Some(tokens);
                    }
                    // The closing event's usage is cumulative for the run.
                    self.result_breakdown = Some(claude_breakdown(usage));
                }
                if let Some(cost) = value.get("total_cost_usd").and_then(Value::as_f64) {
                    self.progress.cost_usd = Some(cost);
                }
            }
            _ => {}
        }
    }

    fn snapshot(&self) -> SessionProgress {
        let mut progress = self.progress.clone();
        progress.tokens = self.result_tokens.or_else(|| {
            self.saw_assistant_usage
                .then_some(self.last_input + self.sum_output)
        });
        progress.breakdown = self.result_breakdown.or_else(|| {
            self.saw_assistant_usage.then(|| {
                let mut total = self.anonymous_usage;
                for breakdown in self.message_usage.values() {
                    total.add(breakdown);
                }
                total
            })
        });
        progress
    }
}

/// Sum opencode's `tokens` object (`{input, output, ...}` — its keys drop the
/// `_tokens` suffix claude uses). `None` when neither field is present.
fn opencode_tokens(tokens: &Value) -> Option<i64> {
    let input = tokens.get("input").and_then(Value::as_i64);
    let output = tokens.get("output").and_then(Value::as_i64);
    match (input, output) {
        (None, None) => None,
        _ => Some(input.unwrap_or(0) + output.unwrap_or(0)),
    }
}

/// Parse opencode's `run --format json` transcript.
#[cfg(test)]
fn parse_opencode(raw: &str, progress: &mut SessionProgress) {
    let mut parser = Parser::new(Dialect::Opencode);
    parser.feed_str(raw);
    *progress = parser.snapshot();
}

/// Running state of an opencode parse. Token usage placement is not stable
/// across versions, so it is read best-effort from a `tokens` object on each
/// event's `part` (last seen wins); when absent the caller falls back to the
/// log scraper.
#[derive(Debug, Clone, Default)]
struct OpencodeParser {
    progress: SessionProgress,
    // `tokens` on a `step-finish` part is that one step's usage; the breakdown
    // sums steps, keyed by part id so a re-emitted part is not counted twice.
    step_usage: HashMap<String, TokenBreakdown>,
}

impl OpencodeParser {
    fn feed(&mut self, value: &Value) {
        let Some(part) = value.get("part") else {
            return;
        };
        if let Some(tokens) = part.get("tokens").and_then(opencode_tokens) {
            self.progress.tokens = Some(tokens);
        }
        if let Some(tokens) = part.get("tokens") {
            let cache = tokens.get("cache").unwrap_or(&Value::Null);
            let breakdown = TokenBreakdown::from_split(
                int_field(tokens, "input"),
                int_field(tokens, "output") + int_field(tokens, "reasoning"),
                int_field(cache, "read"),
                int_field(cache, "write"),
            );
            let key = part
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("#{}", self.step_usage.len()));
            self.step_usage.insert(key, breakdown);
        }
        if value.get("type").and_then(Value::as_str) == Some("tool_use") {
            let tool = part.get("tool").and_then(Value::as_str).unwrap_or("");
            if tool == "todowrite"
                && let Some(todos) = part
                    .get("state")
                    .and_then(|state| state.get("input"))
                    .and_then(|input| input.get("todos"))
                    .and_then(Value::as_array)
            {
                apply_todos(todos, &mut self.progress);
            }
            self.progress.last_activity = Some(opencode_tool_summary(part));
        }
    }

    fn snapshot(&self) -> SessionProgress {
        let mut progress = self.progress.clone();
        if !self.step_usage.is_empty() {
            let mut total = TokenBreakdown::default();
            for breakdown in self.step_usage.values() {
                total.add(breakdown);
            }
            progress.breakdown = Some(total);
        }
        progress
    }
}

/// Parse Codex's `exec --json` transcript.
#[cfg(test)]
fn parse_codex(raw: &str, progress: &mut SessionProgress) {
    let mut parser = Parser::new(Dialect::Codex);
    parser.feed_str(raw);
    *progress = parser.snapshot();
}

/// Feed one Codex event. Codex reports cumulative usage on `turn.completed`
/// and finalized commands/file changes as `item.completed`. The stream has no
/// native todo tool, so only tokens, cost, and last activity are populated.
fn feed_codex(value: &Value, progress: &mut SessionProgress) {
    match value.get("type").and_then(Value::as_str) {
        Some("turn.completed") => {
            if let Some(usage) = value.get("usage") {
                let input = usage.get("input_tokens").and_then(Value::as_i64);
                let output = usage.get("output_tokens").and_then(Value::as_i64);
                let total = usage
                    .get("total_tokens")
                    .and_then(Value::as_i64)
                    .or_else(|| match (input, output) {
                        (None, None) => None,
                        _ => Some(input.unwrap_or(0) + output.unwrap_or(0)),
                    });
                if total.is_some() {
                    progress.tokens = total;
                }
                // OpenAI convention: `input_tokens` already includes the
                // cached part.
                if input.is_some() || output.is_some() {
                    let cached = int_field(usage, "cached_input_tokens");
                    progress.breakdown = Some(TokenBreakdown {
                        input: input.unwrap_or(0),
                        output: output.unwrap_or(0),
                        cache_read: cached,
                        cache_write: 0,
                    });
                }
            }
            if let Some(cost) = value
                .get("usage")
                .and_then(|usage| usage.get("cost_usd"))
                .and_then(Value::as_f64)
                .or_else(|| value.get("cost_usd").and_then(Value::as_f64))
            {
                progress.cost_usd = Some(cost);
            }
        }
        Some("item.completed") => {
            let Some(item) = value.get("item") else {
                return;
            };
            if matches!(
                item.get("type").and_then(Value::as_str),
                Some("command_execution")
                    | Some("file_change")
                    | Some("mcp_tool_call")
                    | Some("web_search")
                    | Some("web_search_call")
            ) {
                progress.last_activity = Some(codex_tool_summary(item));
            }
        }
        _ => {}
    }
}

/// Replay one omp `todo` tool call into the running item/done sets. `init`
/// establishes the full phase→items tree (resetting prior state); `append` adds
/// items; `done` marks one item complete by its text; `view` is a no-op. pi has
/// no todo tool, so this is only ever driven by omp transcripts.
fn apply_pi_todo(args: &Value, items: &mut Vec<String>, done: &mut HashSet<String>) {
    match args.get("op").and_then(Value::as_str) {
        Some("init") => {
            items.clear();
            done.clear();
            if let Some(list) = args.get("list").and_then(Value::as_array) {
                for phase in list {
                    if let Some(phase_items) = phase.get("items").and_then(Value::as_array) {
                        items.extend(
                            phase_items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string),
                        );
                    }
                }
            }
        }
        Some("append") => {
            if let Some(phase_items) = args.get("items").and_then(Value::as_array) {
                items.extend(
                    phase_items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string),
                );
            }
        }
        Some("done") => {
            if let Some(task) = args.get("task").and_then(Value::as_str) {
                done.insert(task.to_string());
            }
        }
        _ => {}
    }
}

/// Parse the pi family (pi/omp) `--mode json` NDJSON stream.
#[cfg(test)]
fn parse_pi_family(raw: &str, progress: &mut SessionProgress) {
    let mut parser = Parser::new(Dialect::PiFamily);
    parser.feed_str(raw);
    *progress = parser.snapshot();
}

/// Running state of a pi-family parse. Each assistant turn is finalized in one
/// `message_end` carrying cumulative-per-message `usage` (`input`/`output`,
/// and `cost.total`) and that turn's tool calls; `message_start` is a zeroed
/// placeholder and `turn_end` duplicates the last message, so both are skipped
/// to avoid double counting. Tokens follow the same live accounting as claude
/// (`last_input + Σ output`); cost sums each turn's `cost.total`. omp's `todo`
/// tool is replayed into the progress counts, while pi (no todo tool) simply
/// reports none.
#[derive(Debug, Clone, Default)]
struct PiParser {
    progress: SessionProgress,
    sum_output: i64,
    last_input: i64,
    saw_usage: bool,
    total_cost: f64,
    saw_cost: bool,
    breakdown: TokenBreakdown,
    todo_items: Vec<String>,
    todo_done: HashSet<String>,
}

impl PiParser {
    fn feed(&mut self, value: &Value) {
        if value.get("type").and_then(Value::as_str) != Some("message_end") {
            return;
        }
        let Some(message) = value.get("message") else {
            return;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        if let Some(usage) = message.get("usage") {
            self.last_input = usage
                .get("input")
                .and_then(Value::as_i64)
                .unwrap_or(self.last_input);
            self.sum_output += usage.get("output").and_then(Value::as_i64).unwrap_or(0);
            self.saw_usage = true;
            // Each `message_end` is one turn's own usage; `input` excludes
            // the cache fields.
            self.breakdown.add(&TokenBreakdown::from_split(
                int_field(usage, "input"),
                int_field(usage, "output"),
                int_field(usage, "cacheRead"),
                int_field(usage, "cacheWrite"),
            ));
            if let Some(cost) = usage
                .get("cost")
                .and_then(|cost| cost.get("total"))
                .and_then(Value::as_f64)
            {
                self.total_cost += cost;
                self.saw_cost = true;
            }
        }
        if let Some(content) = message.get("content").and_then(Value::as_array) {
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                    continue;
                }
                if block.get("name").and_then(Value::as_str) == Some("todo") {
                    let args = block.get("arguments").cloned().unwrap_or(Value::Null);
                    apply_pi_todo(&args, &mut self.todo_items, &mut self.todo_done);
                }
                self.progress.last_activity = Some(pi_tool_summary(block));
            }
        }
    }

    fn snapshot(&self) -> SessionProgress {
        let mut progress = self.progress.clone();
        if self.saw_usage {
            progress.tokens = Some(self.last_input + self.sum_output);
            progress.breakdown = Some(self.breakdown);
        }
        if self.saw_cost {
            progress.cost_usd = Some(self.total_cost);
        }
        if !self.todo_items.is_empty() {
            progress.todos_total = self.todo_items.len();
            progress.todos_done = self
                .todo_items
                .iter()
                .filter(|item| self.todo_done.contains(*item))
                .count();
        }
        progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_TRANSCRIPT: &str = r#"
{"type":"system","subtype":"init","session_id":"claude-abc"}
{"type":"assistant","message":{"usage":{"input_tokens":1000,"output_tokens":50,"cache_read_input_tokens":9000},"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"},{"content":"c","status":"pending"}]}}]}}
{"type":"assistant","message":{"usage":{"input_tokens":1200,"output_tokens":80},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/auth/mod.rs"}}]}}
{"type":"assistant","message":{"usage":{"input_tokens":1300,"output_tokens":40},"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"completed"},{"content":"c","status":"pending"}]}}]}}
"#;

    #[test]
    fn claude_progress_todos_activity_and_live_tokens() {
        let mut progress = SessionProgress::default();
        parse_claude(CLAUDE_TRANSCRIPT, &mut progress);

        // Last TodoWrite wins: 2 of 3 completed.
        assert_eq!(progress.todos(), Some((2, 3)));
        // Last tool_use overall is the second TodoWrite (activity is any tool).
        assert!(progress.last_activity.is_some());
        // No result event yet: live estimate = last_input(1300) + Σoutput(50+80+40).
        assert_eq!(progress.tokens, Some(1300 + 170));
        assert_eq!(progress.cost_usd, None);
    }

    #[test]
    fn claude_result_event_supersedes_with_cost() {
        let transcript = format!(
            "{CLAUDE_TRANSCRIPT}{}\n",
            r#"{"type":"result","subtype":"success","total_cost_usd":0.4231,"usage":{"input_tokens":5000,"output_tokens":600},"result":"done"}"#
        );
        let mut progress = SessionProgress::default();
        parse_claude(&transcript, &mut progress);

        assert_eq!(progress.tokens, Some(5600));
        assert_eq!(progress.cost_usd, Some(0.4231));
        assert_eq!(progress.todos(), Some((2, 3)));
    }

    #[test]
    fn codex_progress_reads_completed_turn_usage_and_activity() {
        let transcript = r#"
{"type":"item.completed","item":{"type":"command_execution","command":"cargo test"}}
{"type":"turn.completed","usage":{"input_tokens":1200,"output_tokens":80,"total_tokens":1280}}
"#;
        let mut progress = SessionProgress::default();
        parse_codex(transcript, &mut progress);

        assert_eq!(progress.tokens, Some(1280));
        assert_eq!(progress.cost_usd, None);
        assert_eq!(
            progress.last_activity.as_deref(),
            Some("command cargo test")
        );
    }

    #[test]
    fn opencode_progress_from_parts() {
        let transcript = r#"
{"type":"text","part":{"type":"text","text":"working"}}
{"type":"tool_use","part":{"type":"tool","tool":"read","state":{"input":{"filePath":"src/main.rs"}}}}
{"type":"tool_use","part":{"type":"tool","tool":"todowrite","state":{"input":{"todos":[{"content":"x","status":"completed"},{"content":"y","status":"pending"}]}}}}
{"type":"step_finish","part":{"type":"step-finish","tokens":{"input":2000,"output":150}}}
"#;
        let mut progress = SessionProgress::default();
        parse_opencode(transcript, &mut progress);

        assert_eq!(progress.todos(), Some((1, 2)));
        assert_eq!(progress.tokens, Some(2150));
        assert_eq!(progress.last_activity.as_deref(), Some("todowrite"));
    }

    // omp `--mode json` stream: assistant turns finalize in `message_end` with
    // `usage.cost.total` and `todo`/tool calls; `message_start`/`turn_end` are
    // decoys that must not be double counted.
    const OMP_TRANSCRIPT: &str = r#"
{"type":"session","id":"019-omp","cwd":"/repo"}
{"type":"message_start","message":{"role":"assistant","content":[],"usage":{"input":0,"output":0,"cost":{"total":0}}}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","name":"todo","arguments":{"op":"init","list":[{"phase":"P","items":["a","b","c"]}]}}],"usage":{"input":1000,"output":50,"cost":{"total":0.01}}}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","name":"todo","arguments":{"op":"done","task":"a"}},{"type":"toolCall","name":"todo","arguments":{"op":"done","task":"b"}}],"usage":{"input":1200,"output":80,"cost":{"total":0.02}}}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","name":"edit","arguments":{"path":"src/auth/mod.rs","i":"Fix auth"}}],"usage":{"input":1300,"output":40,"cost":{"total":0.005}}}}
{"type":"turn_end","message":{"role":"assistant","content":[],"usage":{"input":1300,"output":40,"cost":{"total":0.005}}}}
"#;

    #[test]
    fn omp_progress_todos_tokens_cost_and_activity() {
        let mut progress = SessionProgress::default();
        parse_pi_family(OMP_TRANSCRIPT, &mut progress);

        // init a,b,c (total 3); done a,b → 2/3.
        assert_eq!(progress.todos(), Some((2, 3)));
        // last_input(1300) + Σoutput(50+80+40); turn_end/message_start ignored.
        assert_eq!(progress.tokens, Some(1300 + 170));
        // Σ cost.total across the three message_end turns.
        assert_eq!(progress.cost_usd, Some(0.01 + 0.02 + 0.005));
        assert_eq!(
            progress.last_activity.as_deref(),
            Some("edit src/auth/mod.rs")
        );
    }

    #[test]
    fn pi_reports_tokens_cost_activity_but_no_todos() {
        // pi shares omp's stream shape but has no todo tool.
        let transcript = r#"
{"type":"session","id":"pi-1"}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","name":"read","arguments":{"path":"Cargo.toml"}}],"usage":{"input":2000,"output":30,"cost":{"total":0.05}}}}
"#;
        let mut progress = SessionProgress::default();
        parse_pi_family(transcript, &mut progress);

        assert_eq!(progress.tokens, Some(2030));
        assert_eq!(progress.cost_usd, Some(0.05));
        assert_eq!(progress.todos(), None);
        assert_eq!(progress.last_activity.as_deref(), Some("read Cargo.toml"));
    }

    #[test]
    fn claude_breakdown_dedupes_streamed_blocks_and_prefers_result() {
        // Two content blocks of one API call repeat its id and usage.
        let transcript = r#"
{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":900,"cache_creation_input_tokens":90},"content":[]}}
{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":900,"cache_creation_input_tokens":90},"content":[]}}
{"type":"assistant","message":{"id":"m2","usage":{"input_tokens":20,"output_tokens":7,"cache_read_input_tokens":980},"content":[]}}
"#;
        let mut progress = SessionProgress::default();
        parse_claude(transcript, &mut progress);
        let live = progress.breakdown.expect("live breakdown");
        assert_eq!(
            live,
            TokenBreakdown {
                input: 10 + 900 + 90 + 20 + 980,
                output: 12,
                cache_read: 1880,
                cache_write: 90,
            }
        );
        assert_eq!(live.cache_hit_rate(), Some(1880.0 * 100.0 / 2000.0));

        let finished = format!(
            "{transcript}{}\n",
            r#"{"type":"result","usage":{"input_tokens":34,"cache_creation_input_tokens":66,"cache_read_input_tokens":900,"output_tokens":50}}"#
        );
        let mut progress = SessionProgress::default();
        parse_claude(&finished, &mut progress);
        let total = progress.breakdown.expect("result breakdown");
        assert_eq!(
            (total.input, total.output, total.cache_read),
            (1000, 50, 900)
        );
    }

    #[test]
    fn breakdown_for_codex_opencode_and_pi() {
        let mut progress = SessionProgress::default();
        parse_codex(
            r#"{"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":40}}"#,
            &mut progress,
        );
        let codex = progress.breakdown.expect("codex");
        assert_eq!(
            (codex.input, codex.output, codex.cache_read),
            (1000, 40, 800)
        );

        let mut progress = SessionProgress::default();
        parse_opencode(
            r#"
{"type":"step_finish","part":{"id":"p1","type":"step-finish","tokens":{"input":100,"output":10,"reasoning":5,"cache":{"read":300,"write":0}}}}
{"type":"step_finish","part":{"id":"p2","type":"step-finish","tokens":{"input":50,"output":20,"reasoning":0,"cache":{"read":350,"write":10}}}}
"#,
            &mut progress,
        );
        let opencode = progress.breakdown.expect("opencode");
        assert_eq!(
            (opencode.input, opencode.output, opencode.cache_read),
            (810, 35, 650)
        );

        let mut progress = SessionProgress::default();
        parse_pi_family(
            r#"
{"type":"message_end","message":{"role":"assistant","usage":{"input":600,"output":70,"cacheRead":16000,"cacheWrite":0}}}
{"type":"message_end","message":{"role":"assistant","usage":{"input":200,"output":30,"cacheRead":17000,"cacheWrite":200}}}
"#,
            &mut progress,
        );
        let pi = progress.breakdown.expect("pi");
        assert_eq!((pi.input, pi.output, pi.cache_read), (34_000, 100, 33_000));
    }

    #[test]
    fn empty_when_nothing_parseable() {
        let mut progress = SessionProgress::default();
        parse_claude("not json\n{\"type\":\"system\"}\n", &mut progress);
        assert!(!progress.has_data());
        assert_eq!(progress.tokens, None);
    }

    #[test]
    fn missing_transcript_falls_back_to_none_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let progress = read_session_progress(dir.path(), "ses-none", "claude");
        assert!(!progress.has_data());
    }

    #[test]
    fn invalid_session_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let progress = read_session_progress(dir.path(), "../etc/passwd", "claude");
        assert_eq!(progress, SessionProgress::default());
    }

    fn omp_turn(input: i64, output: i64, tool: &str) -> String {
        format!(
            r#"{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"toolCall","name":"{tool}","arguments":{{"path":"x"}}}}],"usage":{{"input":{input},"output":{output}}}}}}}"#
        )
    }

    #[test]
    fn incremental_reads_parse_only_appended_lines() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join(".kanban").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let path = logs.join("ses-omp-1.transcript.jsonl");
        let read = || read_session_progress(dir.path(), "ses-omp-1", "omp");

        std::fs::write(&path, format!("{}\n", omp_turn(100, 10, "read"))).unwrap();
        assert_eq!(read().tokens, Some(110));

        // A delta line and a turn still being written: the partial line is
        // counted once it parses, and not double-counted after its newline.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let turn = omp_turn(200, 20, "edit");
        let (head, rest) = turn.split_at(turn.len() / 2);
        write!(file, "{{\"type\":\"message_update\"}}\n{head}").unwrap();
        assert_eq!(read().tokens, Some(110));
        write!(file, "{rest}").unwrap();
        assert_eq!(read().tokens, Some(230));
        writeln!(file).unwrap();
        let progress = read();
        assert_eq!(progress.tokens, Some(230));
        assert_eq!(progress.last_activity.as_deref(), Some("edit x"));

        // A rewrite (not an append) is parsed from scratch.
        std::fs::write(&path, format!("{}\n", omp_turn(5, 1, "read"))).unwrap();
        assert_eq!(read().tokens, Some(6));
        let same_len = format!("{}\n", omp_turn(7, 1, "read"));
        std::fs::write(&path, same_len).unwrap();
        assert_eq!(read().tokens, Some(8));
    }

    #[test]
    fn prefilter_keeps_events_and_skips_streaming_deltas() {
        let delta = r#"{"type":"message_update","message":{"role":"assistant","usage":{"input":9,"output":9}}}"#;
        let mut progress = SessionProgress::default();
        parse_pi_family(
            &format!("{delta}\n{}\n", omp_turn(1, 2, "read")),
            &mut progress,
        );
        assert_eq!(progress.tokens, Some(3));
    }
}
