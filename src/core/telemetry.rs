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

use std::collections::HashSet;
use std::path::Path;

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
    if let Ok(raw) = std::fs::read_to_string(&transcript) {
        match backend {
            "codex" => parse_codex(&raw, &mut progress),
            "opencode" => parse_opencode(&raw, &mut progress),
            "pi" | "omp" => parse_pi_family(&raw, &mut progress),
            _ => parse_claude(&raw, &mut progress),
        }
    }
    if progress.tokens.is_none() {
        progress.tokens = estimate_session_tokens(project_path, session_id);
    }
    progress
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
///
/// Mid-run there is no cumulative total, so tokens are approximated as
/// `last_input + Σ output`: output tokens are per-turn and never overlap, while
/// the input count is the (growing) context of the latest turn. Once the final
/// `result` event arrives its cumulative `usage` supersedes the estimate.
fn parse_claude(raw: &str, progress: &mut SessionProgress) {
    let mut sum_output: i64 = 0;
    let mut last_input: i64 = 0;
    let mut saw_assistant_usage = false;
    let mut result_tokens: Option<i64> = None;

    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let message = value.get("message");
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    last_input = usage
                        .get("input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(last_input);
                    sum_output += usage
                        .get("output_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    saw_assistant_usage = true;
                }
                if let Some(content) = message
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                            continue;
                        }
                        if block.get("name").and_then(Value::as_str) == Some("TodoWrite")
                            && let Some(todos) = block
                                .get("input")
                                .and_then(|input| input.get("todos"))
                                .and_then(Value::as_array)
                        {
                            apply_todos(todos, progress);
                        }
                        progress.last_activity = Some(claude_tool_summary(block));
                    }
                }
            }
            Some("result") => {
                if let Some(tokens) = value.get("usage").and_then(usage_input_output) {
                    result_tokens = Some(tokens);
                }
                if let Some(cost) = value.get("total_cost_usd").and_then(Value::as_f64) {
                    progress.cost_usd = Some(cost);
                }
            }
            _ => {}
        }
    }

    progress.tokens =
        result_tokens.or_else(|| saw_assistant_usage.then_some(last_input + sum_output));
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

/// Parse opencode's `run --format json` transcript. Token usage placement is not
/// stable across versions, so it is read best-effort from a `tokens` object on
/// each event's `part` (last seen wins); when absent the caller falls back to
/// the log scraper.
fn parse_opencode(raw: &str, progress: &mut SessionProgress) {
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(part) = value.get("part") else {
            continue;
        };
        if let Some(tokens) = part.get("tokens").and_then(opencode_tokens) {
            progress.tokens = Some(tokens);
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
                apply_todos(todos, progress);
            }
            progress.last_activity = Some(opencode_tool_summary(part));
        }
    }
}

/// Parse Codex's `exec --json` transcript. Codex reports cumulative usage on
/// `turn.completed` and finalized commands/file changes as `item.completed`.
/// The stream has no native todo tool, so only tokens, cost, and last activity
/// are populated here.
fn parse_codex(raw: &str, progress: &mut SessionProgress) {
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("turn.completed") => {
                if let Some(usage) = value.get("usage") {
                    let input = usage.get("input_tokens").and_then(Value::as_i64);
                    let output = usage.get("output_tokens").and_then(Value::as_i64);
                    let total =
                        usage
                            .get("total_tokens")
                            .and_then(Value::as_i64)
                            .or_else(|| match (input, output) {
                                (None, None) => None,
                                _ => Some(input.unwrap_or(0) + output.unwrap_or(0)),
                            });
                    if total.is_some() {
                        progress.tokens = total;
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
                    continue;
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

/// Parse the pi family (pi/omp) `--mode json` NDJSON stream. Each assistant turn
/// is finalized in one `message_end` carrying cumulative-per-message `usage`
/// (`input`/`output`, and `cost.total`) and that turn's tool calls;
/// `message_start` is a zeroed placeholder and `turn_end` duplicates the last
/// message, so both are skipped to avoid double counting. Tokens follow the same
/// live accounting as claude (`last_input + Σ output`); cost sums each turn's
/// `cost.total`. omp's `todo` tool is replayed into the progress counts, while
/// pi (no todo tool) simply reports none.
fn parse_pi_family(raw: &str, progress: &mut SessionProgress) {
    let mut sum_output: i64 = 0;
    let mut last_input: i64 = 0;
    let mut saw_usage = false;
    let mut total_cost = 0.0_f64;
    let mut saw_cost = false;
    let mut todo_items: Vec<String> = Vec::new();
    let mut todo_done: HashSet<String> = HashSet::new();

    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("message_end") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(usage) = message.get("usage") {
            last_input = usage
                .get("input")
                .and_then(Value::as_i64)
                .unwrap_or(last_input);
            sum_output += usage.get("output").and_then(Value::as_i64).unwrap_or(0);
            saw_usage = true;
            if let Some(cost) = usage
                .get("cost")
                .and_then(|cost| cost.get("total"))
                .and_then(Value::as_f64)
            {
                total_cost += cost;
                saw_cost = true;
            }
        }
        if let Some(content) = message.get("content").and_then(Value::as_array) {
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                    continue;
                }
                if block.get("name").and_then(Value::as_str) == Some("todo") {
                    let args = block.get("arguments").cloned().unwrap_or(Value::Null);
                    apply_pi_todo(&args, &mut todo_items, &mut todo_done);
                }
                progress.last_activity = Some(pi_tool_summary(block));
            }
        }
    }

    if saw_usage {
        progress.tokens = Some(last_input + sum_output);
    }
    if saw_cost {
        progress.cost_usd = Some(total_cost);
    }
    if !todo_items.is_empty() {
        progress.todos_total = todo_items.len();
        progress.todos_done = todo_items
            .iter()
            .filter(|item| todo_done.contains(*item))
            .count();
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
}
