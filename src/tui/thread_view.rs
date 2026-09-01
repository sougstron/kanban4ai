//! Thread panel helpers: the kanban-author display filter and the
//! open-to-last-message scroll.

use crate::core::models::Message;

/// Board-generated audit/system lines are authored `"kanban"`.
pub(super) fn is_kanban_authored(message: &Message) -> bool {
    message
        .author
        .as_deref()
        .is_some_and(|author| author.trim().eq_ignore_ascii_case("kanban"))
}

/// Messages the thread panel should paint. `hide_kanban` is display-only —
/// the sidecar still holds every message.
pub(super) fn visible_thread_messages(messages: &[Message], hide_kanban: bool) -> Vec<&Message> {
    if hide_kanban {
        messages
            .iter()
            .filter(|message| !is_kanban_authored(message))
            .collect()
    } else {
        messages.iter().collect()
    }
}

/// Scroll so `last_start` (first wrapped row of the last visible message) sits
/// as high as possible without leaving empty rows under the thread.
pub(super) fn pin_last_message_scroll(
    last_start: u16,
    content_height: u16,
    visible_height: u16,
) -> u16 {
    last_start.min(content_height.saturating_sub(visible_height))
}

#[cfg(test)]
mod tests {
    use super::{is_kanban_authored, pin_last_message_scroll, visible_thread_messages};
    use crate::core::models::{Message, MessageKind, MessageRole};

    fn message(id: &str, author: Option<&str>) -> Message {
        let mut message = Message::new(id, MessageRole::Agent, MessageKind::Context, id);
        message.author = author.map(str::to_string);
        message
    }

    #[test]
    fn is_kanban_authored_when_author_is_kanban() {
        assert!(is_kanban_authored(&message("MSG-001", Some("kanban"))));
        assert!(is_kanban_authored(&message("MSG-002", Some(" Kanban "))));
        assert!(!is_kanban_authored(&message(
            "MSG-003",
            Some("agent-reply")
        )));
        assert!(!is_kanban_authored(&message("MSG-004", Some("user"))));
        assert!(!is_kanban_authored(&message("MSG-005", None)));
    }

    #[test]
    fn visible_thread_messages_keeps_kanban_when_filter_off() {
        let messages = [
            message("MSG-001", Some("kanban")),
            message("MSG-002", Some("user")),
            message("MSG-003", Some("kanban")),
        ];
        let visible = visible_thread_messages(&messages, false);
        assert_eq!(visible.len(), 3);
    }

    #[test]
    fn visible_thread_messages_drops_kanban_when_filter_on() {
        let messages = [
            message("MSG-001", Some("kanban")),
            message("MSG-002", Some("user")),
            message("MSG-003", Some("kanban")),
            message("MSG-004", Some("agent-reply")),
        ];
        let visible = visible_thread_messages(&messages, true);
        assert_eq!(
            visible
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["MSG-002", "MSG-004"]
        );
    }

    #[test]
    fn visible_thread_messages_is_empty_when_filter_hides_everything() {
        let messages = [message("MSG-001", Some("kanban"))];
        assert!(visible_thread_messages(&messages, true).is_empty());
    }

    #[test]
    fn pin_last_message_scroll_stays_zero_when_thread_fits() {
        assert_eq!(pin_last_message_scroll(4, 8, 10), 0);
        assert_eq!(pin_last_message_scroll(0, 10, 10), 0);
    }

    #[test]
    fn pin_last_message_scroll_raises_last_header_without_blank_below() {
        // Last message starts at 12; viewport 10 of 20 rows → max scroll 10,
        // so the header can sit at the top.
        assert_eq!(pin_last_message_scroll(12, 20, 10), 10);
        // Short last message: putting its header at the top would leave a
        // blank tail, so clamp to max_scroll (content - visible).
        assert_eq!(pin_last_message_scroll(18, 20, 10), 10);
        // Tall last message: header at the top, rest below the fold.
        assert_eq!(pin_last_message_scroll(5, 20, 10), 5);
    }
}
