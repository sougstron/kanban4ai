//! The agent's own closing answer, harvested from a backend transcript.
//!
//! An agent's last words — the summary it prints when it finishes — used to
//! land only in `.kanban/logs/<session>.log`, so the task thread showed the
//! audit trail (launch, context notes, exit) but never what the agent actually
//! said. Every backend with a machine transcript already streams that text
//! through stdout, so it is extracted here at exit and posted to the thread as
//! a `context` message.
//!
//! Unlike [`crate::core::provenance`] (telemetry, deliberately kept out of the
//! thread) this *is* thread content: it is the agent's own prose, and the next
//! prompt is built from it like any other context entry.
//!
//! Each backend marks the final assistant message differently:
//! * claude (`--output-format stream-json`) closes with a `result` event whose
//!   `result` is the final text; the last `assistant` message's `text` blocks
//!   are the fallback when the run ends without one.
//! * opencode (`run --format json`) emits `text` events tagged with the
//!   `messageID` they belong to, so the final message is the last group.
//! * the pi family (pi/omp, `--mode json`) finalizes each assistant turn in one
//!   `message_end`, so the last one carrying text wins.

use std::path::Path;

use serde_json::Value;

/// Marker appended when a reply is cut down to the configured budget.
const TRUNCATION_MARKER: &str = "\n... (agent reply truncated)";

/// The final assistant text of a run, or `None` when the backend has no
/// parseable transcript, the file is unreadable, or the run said nothing.
pub fn final_reply(backend: &str, transcript: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(transcript).ok()?;
    let reply = match backend {
        "claude" => claude_final_reply(&raw),
        "opencode" => opencode_final_reply(&raw),
        "pi" | "omp" => pi_family_final_reply(&raw),
        _ => None,
    }?;
    let reply = reply.trim();
    (!reply.is_empty()).then(|| reply.to_string())
}

/// Clamp a reply to `max_chars`, flagging the cut. A whole agent monologue can
/// be arbitrarily long and every thread entry is replayed into the next
/// prompt, so the budget is a board threshold rather than a hardcoded limit.
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

/// claude: the `result` event's `result` is the finished answer. Without one
/// (interrupted run, older CLI) fall back to the last assistant message that
/// carried text — its blocks arrive as separate events sharing one `message.id`,
/// so they are grouped by that id and the previous turn is dropped.
fn claude_final_reply(raw: &str) -> Option<String> {
    let mut result: Option<String> = None;
    let mut current_id = String::new();
    let mut parts: Vec<String> = Vec::new();
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
                if id != current_id {
                    parts.clear();
                    current_id = id;
                }
                parts.extend(texts);
            }
            _ => {}
        }
    }
    result.or_else(|| (!parts.is_empty()).then(|| parts.join("\n\n")))
}

/// opencode: `text` events carry `part.messageID`, so the final message is the
/// last group of text parts sharing one id.
fn opencode_final_reply(raw: &str) -> Option<String> {
    let mut current_id = String::new();
    let mut parts: Vec<String> = Vec::new();
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
        if id != current_id {
            parts.clear();
            current_id = id;
        }
        parts.push(text.to_string());
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// pi family (pi/omp): each assistant turn is finalized in one `message_end`,
/// so the last one carrying text is the closing answer. `turn_end` duplicates
/// that message and is ignored, exactly as in the log renderer.
fn pi_family_final_reply(raw: &str) -> Option<String> {
    let mut last: Option<String> = None;
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
            last = Some(texts.join("\n\n"));
        }
    }
    last
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
    fn claude_prefers_the_result_event() {
        let (_dir, path) = transcript(concat!(
            r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"Planning."}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"text","text":"Summary line."}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","result":"Summary line.\n\n- one\n- two"}"#,
            "\n",
        ));
        assert_eq!(
            final_reply("claude", &path).as_deref(),
            Some("Summary line.\n\n- one\n- two")
        );
    }

    /// Without a `result` event only the last assistant message counts, and its
    /// separately streamed text blocks (same `message.id`) are joined.
    #[test]
    fn claude_falls_back_to_the_last_assistant_message() {
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
            final_reply("claude", &path).as_deref(),
            Some("Done.\n\nChecks pass.")
        );
    }

    #[test]
    fn opencode_takes_the_last_message_group() {
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
            final_reply("opencode", &path).as_deref(),
            Some("Основные разделы:\n\n- src/ — код")
        );
    }

    #[test]
    fn pi_family_takes_the_last_assistant_message_end() {
        let (_dir, path) = transcript(concat!(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Working."}]}}"#,
            "\n",
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"All checks green."}]}}"#,
            "\n",
            r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"echo"}]}}"#,
            "\n",
        ));
        assert_eq!(
            final_reply("omp", &path).as_deref(),
            Some("All checks green.")
        );
        assert_eq!(
            final_reply("pi", &path).as_deref(),
            Some("All checks green.")
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
        assert_eq!(final_reply("claude", &path), None);
        assert_eq!(final_reply("aider", &path), None);
        assert_eq!(
            final_reply("claude", Path::new("/nope/missing.jsonl")),
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
}
