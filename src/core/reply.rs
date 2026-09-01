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
//! Messages are therefore kept as a **list**, not one blob: the thread budget
//! is spent from the tail ([`compose_reply`]), so the run's final message
//! survives whole and only the early chatter is dropped. Spending it from the
//! head instead used to keep the opening planning talk and cut the answer off
//! mid-sentence.
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

/// Blank line between two messages of the same run, as the session rendered it.
const SEPARATOR: &str = "\n\n";

/// Every assistant message of a run, in order — the whole answer exactly as
/// the session rendered it — or `None` when the backend has no parseable
/// transcript, the file is unreadable, or the run said nothing.
pub fn session_messages(backend: &str, transcript: &Path) -> Option<Vec<String>> {
    let raw = std::fs::read_to_string(transcript).ok()?;
    let messages = match backend {
        "claude" => claude_session_messages(&raw),
        "opencode" => opencode_session_messages(&raw),
        "pi" | "omp" => pi_family_session_messages(&raw),
        _ => None,
    }?;
    let messages: Vec<String> = messages
        .into_iter()
        .map(|message| message.trim().to_string())
        .filter(|message| !message.is_empty())
        .collect();
    (!messages.is_empty()).then_some(messages)
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

/// Join a run's messages into one thread entry within `max_chars`.
///
/// A whole session's worth of agent prose can be arbitrarily long and every
/// thread entry is replayed into the next prompt, so the budget is a board
/// threshold rather than a hardcoded limit. It is spent **from the tail**: the
/// run's last message — the answer the agent finished on — is laid down first
/// and earlier messages are prepended while they still fit, so a long run
/// loses its opening chatter instead of its conclusion. Each earlier message
/// is additionally clamped to `message_max_chars` (`0` disables that) so one
/// mid-run wall of text cannot eat the whole budget.
///
/// Cuts land on a line boundary where there is one, since agent answers are
/// markdown and slicing a table row mid-cell reads as corruption. `log_path`
/// (the run's `.kanban/logs/<session>.log`) is named in every marker so the
/// dropped text can still be read in full; pass `""` to omit it.
pub fn compose_reply(
    messages: &[String],
    max_chars: usize,
    message_max_chars: usize,
    log_path: &str,
) -> String {
    let Some((last, earlier)) = messages.split_last() else {
        return String::new();
    };
    let mut kept = vec![clamp(last, max_chars, log_path)];
    let mut used = char_count(&kept[0]);
    let mut omitted = 0usize;
    for message in earlier.iter().rev() {
        let message = match message_max_chars {
            0 => message.clone(),
            limit => clamp(message, limit, log_path),
        };
        let cost = char_count(&message) + char_count(SEPARATOR);
        // Once one message no longer fits, everything before it is dropped
        // too: the kept messages stay a contiguous run of the session's tail.
        if omitted > 0 || used + cost > max_chars {
            omitted += 1;
            continue;
        }
        used += cost;
        kept.push(message);
    }
    kept.reverse();
    let body = kept.join(SEPARATOR);
    match omitted {
        0 => body,
        count => format!("{}{SEPARATOR}{body}", omission_marker(count, log_path)),
    }
}

/// Clamp one message to `max_chars`, flagging the cut. The head is kept: a
/// message that overflows the whole budget on its own is the final answer, and
/// its opening states what it did.
fn clamp(message: &str, max_chars: usize, log_path: &str) -> String {
    if char_count(message) <= max_chars {
        return message.to_string();
    }
    let kept: String = message.chars().take(max_chars).collect();
    // Prefer the last line break, but only when it does not throw away most of
    // the allowance — a message with no early newline is kept as-is.
    let cut = match kept.rfind('\n') {
        Some(at) if at * 2 >= kept.len() => &kept[..at],
        _ => kept.as_str(),
    };
    format!(
        "{}\n... (agent reply truncated{})",
        cut.trim_end(),
        full_text_hint(log_path)
    )
}

fn omission_marker(count: usize, log_path: &str) -> String {
    let plural = if count == 1 { "message" } else { "messages" };
    format!(
        "... ({count} earlier agent {plural} omitted{})",
        full_text_hint(log_path)
    )
}

fn full_text_hint(log_path: &str) -> String {
    if log_path.is_empty() {
        return String::new();
    }
    format!(", full text in {log_path}")
}

fn char_count(text: &str) -> usize {
    text.chars().count()
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
fn claude_session_messages(raw: &str) -> Option<Vec<String>> {
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
    if messages.is_empty() {
        return result.map(|text| vec![text]);
    }
    Some(messages)
}

/// opencode: `text` events carry `part.messageID`; every message's text is
/// kept, grouped by that id and in the order the run printed it.
fn opencode_session_messages(raw: &str) -> Option<Vec<String>> {
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
    (!messages.is_empty()).then_some(messages)
}

/// pi family (pi/omp): each assistant turn is finalized in one `message_end`,
/// so every `message_end` carrying text is kept in order. `turn_end`
/// duplicates that message and is ignored, exactly as in the log renderer.
fn pi_family_session_messages(raw: &str) -> Option<Vec<String>> {
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
    (!messages.is_empty()).then_some(messages)
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

    /// Messages as the thread would render them, for the gathering tests.
    fn joined(backend: &str, path: &std::path::Path) -> Option<String> {
        session_messages(backend, path).map(|messages| messages.join(SEPARATOR))
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
            joined("claude", &path).as_deref(),
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
            joined("claude", &path).as_deref(),
            Some("Early note.\n\nDone.\n\nChecks pass.")
        );
        let (_dir, path) = transcript(concat!(
            r#"{"type":"result","subtype":"success","result":"Only the result text."}"#,
            "\n",
        ));
        assert_eq!(
            joined("claude", &path).as_deref(),
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
            joined("opencode", &path).as_deref(),
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
            joined("omp", &path).as_deref(),
            Some("Working.\n\nAll checks green.")
        );
        assert_eq!(
            joined("pi", &path).as_deref(),
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
        assert_eq!(session_messages("claude", &path), None);
        assert_eq!(session_messages("aider", &path), None);
        assert_eq!(
            session_messages("claude", Path::new("/nope/missing.jsonl")),
            None
        );
    }

    fn messages(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| text.to_string()).collect()
    }

    #[test]
    fn short_runs_are_joined_untouched() {
        let composed = compose_reply(&messages(&["Planning.", "Done."]), 100, 50, "/logs/ses.log");
        assert_eq!(composed, "Planning.\n\nDone.");
        assert_eq!(compose_reply(&[], 100, 50, ""), "");
    }

    /// The budget is spent from the tail: the final answer survives whole and
    /// the early chatter is what goes, with the log named for the rest.
    #[test]
    fn budget_is_spent_from_the_last_message_backwards() {
        let composed = compose_reply(
            &messages(&["Opening plan.", "Middle note.", "The final answer."]),
            35,
            50,
            "/logs/ses.log",
        );
        assert!(composed.ends_with("Middle note.\n\nThe final answer."));
        assert!(!composed.contains("Opening plan."));
        assert!(
            composed
                .starts_with("... (1 earlier agent message omitted, full text in /logs/ses.log)")
        );
        // Several dropped messages read as a plural count.
        let composed = compose_reply(
            &messages(&["a".repeat(50).as_str(), "b".repeat(50).as_str(), "Answer."]),
            30,
            50,
            "",
        );
        assert_eq!(
            composed,
            "... (2 earlier agent messages omitted)\n\nAnswer."
        );
    }

    /// A single message longer than the whole budget keeps its head and is
    /// flagged; multi-byte characters are counted, not bytes.
    #[test]
    fn an_overlong_final_message_is_clamped_and_flagged() {
        let long = "я".repeat(50);
        let composed = compose_reply(&messages(&[&long]), 10, 0, "/logs/ses.log");
        assert!(composed.starts_with(&"я".repeat(10)));
        assert!(composed.ends_with("... (agent reply truncated, full text in /logs/ses.log)"));
        assert!(
            compose_reply(&messages(&[&long]), 10, 0, "").ends_with("... (agent reply truncated)")
        );
    }

    /// Cuts land on a line boundary so a markdown table is never sliced
    /// mid-row — unless the message has no early newline to fall back on.
    #[test]
    fn cuts_prefer_a_line_boundary() {
        let table = "| a | b |\n| 1 | 2 |\n| 3 | 4 |";
        let composed = compose_reply(&messages(&[table]), 22, 0, "");
        assert!(composed.starts_with("| a | b |\n| 1 | 2 |\n..."));
        let unbroken = "x".repeat(40);
        let composed = compose_reply(&messages(&[&unbroken]), 20, 0, "");
        assert!(composed.starts_with(&"x".repeat(20)));
    }

    /// One long mid-run message is clamped on its own so it cannot eat the
    /// budget the earlier messages would otherwise share.
    #[test]
    fn earlier_messages_get_their_own_cap() {
        let composed = compose_reply(
            &messages(&["Start.", &"m".repeat(500), "Answer."]),
            200,
            20,
            "",
        );
        assert!(composed.starts_with("Start."));
        assert!(composed.ends_with("Answer."));
        assert!(composed.contains(&format!("{}\n... (agent reply truncated)", "m".repeat(20))));
        assert!(!composed.contains(&"m".repeat(21)));
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
