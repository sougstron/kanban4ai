use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::models::Task;

use super::app::App;

pub fn render_card(frame: &mut Frame<'_>, app: &App, task: &Task, area: Rect, focused: bool) {
    let border = if task.has_questions {
        app.theme.warn
    } else if focused {
        app.theme.focus
    } else {
        app.theme.border
    };
    let title = if focused {
        Span::styled(
            sanitize_terminal_text(&task.id),
            Style::default()
                .fg(app.theme.focus)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            sanitize_terminal_text(&task.id),
            Style::default().fg(app.theme.muted),
        )
    };
    let mut lines = vec![Line::from(vec![
        title,
        Span::raw(" "),
        Span::raw(truncate_display(
            &sanitize_terminal_text(&task.title),
            app.settings.card_line_max_symbols,
        )),
    ])];
    let badges = badges(task).join(" ");
    if !badges.is_empty() {
        lines.push(Line::from(Span::styled(
            badges,
            Style::default().fg(app.theme.ok),
        )));
    }
    if !task.description.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_display(
                &sanitize_terminal_text(&task.description).replace('\n', " "),
                app.settings.card_line_max_symbols,
            ),
            Style::default().fg(app.theme.muted),
        )));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::default().bg(app.theme.bg).fg(app.theme.fg)),
        area,
    );
}

pub fn badges(task: &Task) -> Vec<&'static str> {
    let mut badges = Vec::new();
    if task.session.is_some() {
        badges.push("▶ session");
    }
    if task.interactive {
        badges.push("☑ interactive");
    }
    if task.chained_to.is_some() {
        badges.push("↪ chain");
    }
    if task.has_questions {
        badges.push("? questions");
    }
    badges
}

pub fn truncate_display(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    let mut out = String::new();
    let limit = max_width.saturating_sub(1);
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > limit {
            break;
        }
        width += ch_width;
        out.push(ch);
    }
    out.push('…');
    out
}

pub fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\n' | '\t' => ch,
            '\u{001b}' | '\u{007f}'..='\u{009f}' => '�',
            ch if ch.is_control() => '�',
            ch => ch,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{sanitize_terminal_text, truncate_display};

    #[test]
    fn sanitizes_terminal_control_sequences() {
        let malicious = "title\u{001b}]52;c;secret\u{0007}\u{001b}[2J";
        let sanitized = sanitize_terminal_text(malicious);
        assert!(!sanitized.contains('\u{001b}'));
        assert!(!sanitized.contains('\u{0007}'));
        assert!(sanitized.contains('�'));
    }

    #[test]
    fn preserves_safe_whitespace_and_unicode() {
        assert_eq!(
            sanitize_terminal_text("Привет\n世界\t🙂"),
            "Привет\n世界\t🙂"
        );
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate_display("世界界", 5), "世界…");
        assert_eq!(truncate_display("task", 4), "task");
    }
}
