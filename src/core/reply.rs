//! The agent's whole session answer, harvested from a backend transcript.
//!
//! What the agent said during a run used to land only in
//! `.kanban/logs/<session>.log`, so the task thread showed the audit trail
//! (launch, context notes, exit) but never what the agent actually said.
//! Every backend with a machine transcript already streams that text through
//! stdout, so it is extracted here at exit and posted to the thread as a
//! `context` message.
//!
//! The capture is the run's **entire assistant text**, not just the last
//! message: delegated agents finish with `kanban` tool calls, so their final
//! message is a short wrap-up ("Task done, moved to Review") while the
//! substantive answer is the text printed earlier in the session. Keeping
//! only the last message demonstrably posted just that wrap-up and lost the
//! actual answer, so every backend's gatherings include all assistant text in
//! order, exactly as the session rendered it.
//!
//! Unlike [`crate::core::provenance`] (telemetry, deliberately kept out of the
//! thread) this *is* thread content: it is the agent's own prose, and the next
//! prompt is built from it like any other context entry.
//!
//! Each backend delivers assistant text differently:
//! * claude (`--output-format stream-json`) streams `assistant` events whose
//!   message `id` groups the blocks of one message; a `result` event closes
//!   the run repeating the last message, so it only serves as a fallback when
//!   the run recorded no assistant text at all.
//! * opencode (`run --format json`) emits `text` events tagged with the
//!   `messageID` they belong to.
//! * the pi family (pi/omp, `--mode json`) finalizes each assistant turn in one
//!   `message_end`.

use std::path::Path;

use serde_json::Value;

/// Marker appended when a reply is cut down to the configured budget.
const TRUNCATION_MARKER: &str = "\n... (agent reply truncated)";

/// Every assistant text of a run, in order — the whole answer exactly as the
/// session rendered it — or `None` when the backend has no parseable
/// transcript, the file is unreadable, or the run said nothing.
pub fn session_reply(backend: &str, transcript: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(transcript).ok()?;
    let reply = match backend {
        "claude" => claude_session_reply(&raw),
        "opencode" => opencode_session_reply(&raw),
        "pi" | "omp" => pi_family_session_reply(&raw),
        _ => None,
    }?;
    let reply = reply.trim();
    (!reply.is_empty()).then(|| reply.to_string())
}

/// Last `type: error` event in a transcript, if any. Used on a non-zero exit
/// so the thread can show why the agent died and crash-restart can skip
/// errors the backend marked `isRetryable: false`.
pub fn fatal_error(transcript: &Path) -> Option<crate::core::provenance::StreamError> {
    let raw = std::fs::read_to_string(transcript).ok()?;
    json_lines(&raw)
        .filter_map(|value| crate::core::provenance::stream_error(&value))
        .last()
}

/// Clamp a reply to `max_chars`, flagging the cut. A whole session's worth of
/// agent prose can be arbitrarily long and every thread entry is replayed into
/// the next prompt, so the budget is a board threshold rather than a hardcoded
/// limit.
pub fn truncate_reply(reply: &str, max_chars: usize) -> String {
    if reply.chars().count() <= max_chars {
        return reply.to_string();
    }
    let kept: String = reply.chars().take(max_chars).collect();
    format!("{}{TRUNCATION_MARKER}", kept.trim_end())
}

fn json_lines(raw: &str) -> impl Iterator<Item = Value> + '_ {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
}

/// Text blocks of a content array, in order, empty ones dropped.
fn text_blocks(content: &Value) -> Vec<String> {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// claude: every assistant message that carried text, kept in the order the
/// run printed it. One message's blocks arrive as separate events sharing a
/// message `id`, so they are grouped under that id. The closing `result`
/// event repeats the last message's text, so it only serves as a fallback
/// for runs that recorded no assistant events at all.
fn claude_session_reply(raw: &str) -> Option<String> {
    let mut result: Option<String> = None;
    let mut order: Vec<String> = Vec::new();
    let mut texts_by_id: Vec<Vec<String>> = Vec::new();
    for (index, value) in json_lines(raw).enumerate() {
        match value.get("type").and_then(Value::as_str) {
            Some("result") => {
                if let Some(text) = value
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    result = Some(text.to_string());
                }
            }
            Some("assistant") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                let texts = message.get("content").map(text_blocks).unwrap_or_default();
                if texts.is_empty() {
                    continue;
                }
                let id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("#{index}"));
                match order.iter().position(|known| *known == id) {
                    Some(position) => texts_by_id[position].extend(texts),
                    None => {
                        order.push(id);
                        texts_by_id.push(texts);
                    }
                }
            }
            _ => {}
        }
    }
    let messages: Vec<String> = texts_by_id
        .into_iter()
        .map(|texts| texts.join("\n\n"))
        .collect();
    (!messages.is_empty())
        .then(|| messages.join("\n\n"))
        .or(result)
}

/// opencode: `text` events carry `part.messageID`; every message's text is
/// kept, grouped by that id and in the order the run printed it.
fn opencode_session_reply(raw: &str) -> Option<String> {
    let mut order: Vec<String> = Vec::new();
    let mut texts_by_id: Vec<Vec<String>> = Vec::new();
    for (index, value) in json_lines(raw).enumerate() {
        if value.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(part) = value.get("part") else {
            continue;
        };
        let Some(text) = part
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let id = part
            .get("messageID")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("#{index}"));
        match order.iter().position(|known| *known == id) {
            Some(position) => texts_by_id[position].push(text.to_string()),
            None => {
                order.push(id);
                texts_by_id.push(vec![text.to_string()]);
            }
        }
    }
    let messages: Vec<String> = texts_by_id
        .into_iter()
        .map(|texts| texts.join("\n\n"))
        .collect();
    (!messages.is_empty()).then(|| messages.join("\n\n"))
}

/// pi family (pi/omp): each assistant turn is finalized in one `message_end`,
/// so every `message_end` carrying text is kept in order. `turn_end`
/// duplicates that message and is ignored, exactly as in the log renderer.
fn pi_family_session_reply(raw: &str) -> Option<String> {
    let mut messages: Vec<String> = Vec::new();
    for value in json_lines(raw) {
        if value.get("type").and_then(Value::as_str) != Some("message_end") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let texts = message.get("content").map(text_blocks).unwrap_or_default();
        if !texts.is_empty() {
            messages.push(texts.join("\n\n"));
        }
    }
    (!messages.is_empty()).then(|| messages.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ses.transcript.jsonl");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn claude_gathers_every_assistant_message_and_ignores_result() {
        let (_dir, path) = transcript(concat!(
            r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"Planning."}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"text","text":"Summary line."}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","result":"Summary line.\n\n- one\n- two"}"#,
            "\n",
        ));
        assert_eq!(
            session_reply("claude", &path).as_deref(),
            Some("Planning.\n\nSummary line.")
        );
    }

    /// Without any assistant text the closing `result` event is the fallback,
    /// and the separately streamed blocks of one message (same `message.id`)
    /// are joined.
    #[test]
    fn claude_groups_message_blocks_and_falls_back_to_result() {
        let (_dir, path) = transcript(concat!(
            r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"Early note."}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"text","text":"Done."}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"text","text":"Checks pass."}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
        ));
        assert_eq!(
            session_reply("claude", &path).as_deref(),
            Some("Early note.\n\nDone.\n\nChecks pass.")
        );
        let (_dir, path) = transcript(concat!(
            r#"{"type":"result","subtype":"success","result":"Only the result text."}"#,
            "\n",
        ));
        assert_eq!(
            session_reply("claude", &path).as_deref(),
            Some("Only the result text.")
        );
    }

    #[test]
    fn opencode_gathers_every_message_in_order() {
        let (_dir, path) = transcript(concat!(
            r#"{"type":"text","part":{"type":"text","messageID":"msg_a","text":"Reading files."}}"#,
            "\n",
            r#"{"type":"tool_use","part":{"type":"tool","tool":"read","state":{"input":{"filePath":"a.rs"}}}}"#,
            "\n",
            r#"{"type":"text","part":{"type":"text","messageID":"msg_b","text":"Основные разделы:"}}"#,
            "\n",
            r#"{"type":"text","part":{"type":"text","messageID":"msg_b","text":"- src/ — код"}}"#,
            "\n",
            r#"{"type":"step_finish","part":{"type":"step-finish"}}"#,
            "\n",
        ));
        assert_eq!(
            session_reply("opencode", &path).as_deref(),
            Some("Reading files.\n\nОсновные разделы:\n\n- src/ — код")
        );
    }

    #[test]
    fn pi_family_gathers_every_assistant_message_end() {
        let (_dir, path) = transcript(concat!(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Working."}]}}"#,
            "\n",
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"All checks green."}]}}"#,
            "\n",
            r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"echo"}]}}"#,
            "\n",
        ));
        assert_eq!(
            session_reply("omp", &path).as_deref(),
            Some("Working.\n\nAll checks green.")
        );
        assert_eq!(
            session_reply("pi", &path).as_deref(),
            Some("Working.\n\nAll checks green.")
        );
    }

    /// Tool-only runs, unparseable noise, unknown backends, and missing files
    /// all yield nothing rather than a bogus reply.
    #[test]
    fn quiet_runs_and_unknown_backends_yield_nothing() {
        let (_dir, path) = transcript(concat!(
            "not json at all\n",
            r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"tool_use","name":"Read","input":{"file_path":"a.rs"}}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","result":"   "}"#,
            "\n",
        ));
        assert_eq!(session_reply("claude", &path), None);
        assert_eq!(session_reply("aider", &path), None);
        assert_eq!(
            session_reply("claude", Path::new("/nope/missing.jsonl")),
            None
        );
    }

    #[test]
    fn truncation_flags_the_cut_and_counts_characters() {
        assert_eq!(truncate_reply("короткий", 20), "короткий");
        // Multi-byte characters are counted, not bytes, so the cut lands on a
        // character boundary.
        let long = "я".repeat(50);
        let cut = truncate_reply(&long, 10);
        assert!(cut.starts_with(&"я".repeat(10)));
        assert!(cut.ends_with("(agent reply truncated)"));
    }

    #[test]
    fn fatal_error_takes_the_last_error_event() {
        let (_dir, path) = transcript(concat!(
            r#"{"type":"text","part":{"text":"hi"}}"#,
            "\n",
            r#"{"type":"error","error":{"data":{"message":"first","isRetryable":true}}}"#,
            "\n",
            r#"{"type":"error","error":{"data":{"message":"Insufficient balance.","isRetryable":false}}}"#,
            "\n",
        ));
        let err = fatal_error(&path).expect("error event");
        assert_eq!(err.message, "Insufficient balance.");
        assert!(!err.retryable);
        assert_eq!(fatal_error(Path::new("/nope/missing.jsonl")), None);
    }
}
