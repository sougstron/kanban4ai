//! Thread panel helpers: the kanban-author display filter and the
//! open-to-last-message scroll.

use crate::core::models::Message;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Format only pipe tables; all other Markdown remains verbatim. The caller
/// sanitizes terminal controls before this parser sees the message.
pub(super) fn message_body_lines(body: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    let source: Vec<_> = body.lines().collect();
    let mut result = Vec::new();
    let mut index = 0;
    let mut fence: Option<(char, usize)> = None;
    while index < source.len() {
        let line = source[index];
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let run = trimmed.chars().take_while(|&ch| ch == marker).count();
        if matches!(marker, '`' | '~') && run >= 3 {
            match fence {
                None => fence = Some((marker, run)),
                Some((open, count))
                    if marker == open && run >= count && trimmed[run..].trim().is_empty() =>
                {
                    fence = None;
                }
                _ => {}
            }
            result.push(Line::from(Span::styled(line.to_owned(), style)));
            index += 1;
            continue;
        }
        if fence.is_none()
            && index + 1 < source.len()
            && let Some(header) = table_cells(line)
            && let Some(separator) = table_cells(source[index + 1])
            && header.len() == separator.len()
            && separator.iter().all(|cell| {
                let dashes = cell.trim_matches(':');
                dashes.len() >= 3 && dashes.bytes().all(|ch| ch == b'-')
            })
        {
            let mut rows = vec![header];
            index += 2;
            while index < source.len() {
                let Some(cells) = table_cells(source[index]) else {
                    break;
                };
                // Do not silently discard malformed rows or extra cells.
                if cells.len() != rows[0].len() {
                    break;
                }
                rows.push(cells);
                index += 1;
            }
            render_table(&rows, &separator, usize::from(width), style, &mut result);
        } else {
            result.push(Line::from(Span::styled(line.to_owned(), style)));
            index += 1;
        }
    }
    if result.is_empty() {
        result.push(Line::from(""));
    }
    result
}

fn table_cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.chars().peekable();
    let mut has_pipe = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push(chars.next().expect("peeked pipe"));
            }
            '|' => {
                cells.push(cell.trim().to_owned());
                cell.clear();
                has_pipe = true;
            }
            _ => cell.push(ch),
        }
    }
    if !has_pipe {
        return None;
    }
    cells.push(cell.trim().to_owned());
    if line.starts_with('|') {
        cells.remove(0);
    }
    if line.ends_with('|') && cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    (!cells.is_empty()).then_some(cells)
}

fn render_table(
    rows: &[Vec<String>],
    separator: &[String],
    width: usize,
    style: Style,
    result: &mut Vec<Line<'static>>,
) {
    let columns = rows[0].len();
    let overhead = columns * 3 + 1;
    // Two display columns per cell are needed for wide Unicode characters.
    if width < overhead + columns * 2 {
        for row in &rows[1..] {
            for (header, cell) in rows[0].iter().zip(row) {
                result.push(Line::from(Span::styled(format!("{header}: {cell}"), style)));
            }
            result.push(Line::from(""));
        }
        if rows.len() == 1 {
            result.push(Line::from(Span::styled(rows[0].join(" · "), style)));
        }
        return;
    }
    let mut widths = vec![2; columns];
    for row in rows {
        for (target, cell) in widths.iter_mut().zip(row) {
            *target = (*target).max(cell.width());
        }
    }
    let available = width - overhead;
    // Water-fill the available width instead of shrinking once per byte of
    // a potentially huge agent response.
    let mut allocated = vec![2; columns];
    let mut remaining = available - columns * 2;
    while remaining > 0 {
        let mut changed = false;
        for (size, desired) in allocated.iter_mut().zip(&widths) {
            if *size < *desired && remaining > 0 {
                *size += 1;
                remaining -= 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    widths = allocated;
    let border = |left: char, middle: char, right: char| {
        let mut text = String::new();
        text.push(left);
        for (column, size) in widths.iter().enumerate() {
            text.push_str(&"─".repeat(size + 2));
            text.push(if column + 1 == columns { right } else { middle });
        }
        Line::from(Span::styled(text, style))
    };
    result.push(border('┌', '┬', '┐'));
    for (row_index, row) in rows.iter().enumerate() {
        let wrapped: Vec<_> = row
            .iter()
            .zip(&widths)
            .map(|(cell, size)| wrap_cell(cell, *size))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line_index in 0..height {
            let mut text = String::from("│");
            for (column, cell) in wrapped.iter().enumerate() {
                let value = cell.get(line_index).map(String::as_str).unwrap_or("");
                let padding = widths[column] - value.width();
                let alignment = &separator[column];
                let left = if alignment.ends_with(':') {
                    if alignment.starts_with(':') {
                        padding / 2
                    } else {
                        padding
                    }
                } else {
                    0
                };
                text.push(' ');
                text.push_str(&" ".repeat(left));
                text.push_str(value);
                text.push_str(&" ".repeat(padding - left));
                text.push_str(" │");
            }
            let row_style = if row_index == 0 {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            };
            result.push(Line::from(Span::styled(text, row_style)));
        }
        if row_index == 0 {
            result.push(border('├', '┼', '┤'));
        }
    }
    result.push(border('└', '┴', '┘'));
}

fn wrap_cell(cell: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in cell.split_inclusive(char::is_whitespace) {
        let word_width = word.width();
        if used > 0 && word_width <= width && used + word_width > width {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        // Unbroken identifiers and URLs still need a hard wrap.
        for ch in word.chars() {
            let size = ch.width().unwrap_or(0);
            if used + size > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(ch);
            used += size;
        }
    }
    lines.push(line);
    lines
}

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
    use super::message_body_lines;
    use super::{is_kanban_authored, pin_last_message_scroll, visible_thread_messages};
    use crate::core::models::{Message, MessageKind, MessageRole};
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::Style,
        widgets::{Paragraph, Widget, Wrap},
    };
    use unicode_width::UnicodeWidthStr;

    fn rendered(body: &str, width: u16) -> Vec<String> {
        message_body_lines(body, width, Style::default())
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn tables_align_cells_and_keep_surrounding_text() {
        let lines = rendered(
            "Before\n| Check | Result |\n| :--- | ---: |\n| Engine | OK |\nAfter",
            40,
        );
        assert_eq!(
            lines,
            [
                "Before",
                "┌────────┬────────┐",
                "│ Check  │ Result │",
                "├────────┼────────┤",
                "│ Engine │     OK │",
                "└────────┴────────┘",
                "After"
            ]
        );
    }

    #[test]
    fn table_wraps_wide_unicode_without_breaking_grid() {
        let body = "| Name | Value |\n| --- | --- |\n| двигатель | 界界界界 |\n| empty | |";
        let lines = message_body_lines(body, 19, Style::default());
        assert!(lines.iter().all(|line| line.to_string().width() == 19));
        let cells: Vec<_> = lines.iter().map(ToString::to_string).collect();
        for (column, expected) in [(1, "Nameдвигательempty"), (2, "Value界界界界")] {
            let values: String = cells
                .iter()
                .filter(|line| line.starts_with('│'))
                .map(|line| line.split('│').nth(column).unwrap().trim())
                .collect();
            assert_eq!(values, expected);
        }
        let mut buffer = Buffer::empty(Rect::new(0, 0, 19, lines.len() as u16));
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(buffer.area, &mut buffer);
        for y in 0..buffer.area.height {
            assert!(matches!(buffer[(18, y)].symbol(), "┐" | "┤" | "│" | "┘"));
        }
    }

    #[test]
    fn tiny_table_view_preserves_header_value_relationship() {
        assert_eq!(
            rendered("A | B\n--- | ---\nhello | world", 8),
            ["A: hello", "B: world", ""]
        );
    }

    #[test]
    fn escaped_pipes_and_missing_outer_pipes_are_supported() {
        let lines = rendered("A | B\n--- | ---\nx\\|y | z", 40);
        assert!(lines.contains(&"│ x|y │ z  │".to_owned()));
    }

    #[test]
    fn fenced_tables_and_non_table_pipes_stay_literal() {
        for body in [
            "```md\n| A | B |\n| --- | --- |\n| a | b |\n```",
            "~~~\nA | B\n--- | ---\na | b\n~~~",
            "A | B\n-- | ---\na | b",
        ] {
            assert_eq!(rendered(body, 40), body.lines().collect::<Vec<_>>());
        }
        let lines = rendered("| A | B |\n| --- | --- |\n| x | y | extra |", 40);
        assert_eq!(lines.last().unwrap(), "| x | y | extra |");
    }

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
