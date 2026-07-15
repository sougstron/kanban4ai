use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::models::Task;
use crate::core::session::SessionState;

use super::app::App;

pub fn render_card(frame: &mut Frame<'_>, app: &App, task: &Task, area: Rect, focused: bool) {
    let session_state = app.board.session_states.get(&task.id).copied();
    let border = if session_state == Some(SessionState::Crashed) {
        app.theme.err
    } else if task.has_questions {
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
    let extra = app.board.extras.get(&task.id);
    let visible_title = truncate_display(
        &sanitize_terminal_text(&task.title),
        app.settings.card_line_max_symbols,
    );
    let mut title_spans = vec![title, Span::raw(" ")];
    title_spans.extend(highlight_title_matches(
        &visible_title,
        &app.search.text(),
        app.theme.warn,
    ));
    let mut lines = vec![Line::from(title_spans)];
    // The open question goes right under the title: it is the thing the
    // human must act on, and short cards truncate from the bottom.
    if let Some(preview) = extra.and_then(|extra| extra.question_preview.as_deref()) {
        lines.push(Line::from(Span::styled(
            truncate_display(
                &format!("? {}", sanitize_terminal_text(preview)),
                app.settings.card_line_max_symbols,
            ),
            Style::default()
                .fg(app.theme.warn)
                .add_modifier(Modifier::BOLD),
        )));
    }
    let mut badges = badges(task, session_state, app);
    if extra.is_some_and(|extra| extra.waiting) {
        badges.push(("⏳ waiting".to_string(), app.theme.ok));
    }
    if !badges.is_empty() {
        let badge_spans = badges
            .drain(..)
            .enumerate()
            .flat_map(|(index, (badge, color))| {
                let separator = (index > 0).then(|| Span::raw(" "));
                separator.into_iter().chain(std::iter::once(Span::styled(
                    badge,
                    Style::default().fg(color),
                )))
            })
            .collect::<Vec<_>>();
        lines.push(Line::from(badge_spans));
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

pub fn highlight_title_matches(
    title: &str,
    query: &str,
    warn: ratatui::style::Color,
) -> Vec<Span<'static>> {
    if query.is_empty() {
        return vec![Span::raw(title.to_string())];
    }
    let chars = title.chars().collect::<Vec<_>>();
    let ranges = case_insensitive_match_ranges(&chars, query);
    let mut spans = Vec::new();
    let mut plain_start = 0;
    for (start, end) in ranges {
        if plain_start < start {
            spans.push(Span::raw(
                chars[plain_start..start].iter().collect::<String>(),
            ));
        }
        spans.push(Span::styled(
            chars[start..end].iter().collect::<String>(),
            Style::default().fg(warn),
        ));
        plain_start = end;
    }
    if plain_start < chars.len() {
        spans.push(Span::raw(chars[plain_start..].iter().collect::<String>()));
    }
    spans
}

pub fn case_insensitive_match(text: &str, query: &str) -> bool {
    !case_insensitive_match_ranges(&text.chars().collect::<Vec<_>>(), query).is_empty()
}

fn case_insensitive_match_ranges(chars: &[char], query: &str) -> Vec<(usize, usize)> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut folded = String::new();
        let mut end = start;
        let mut matched = None;
        while end < chars.len() {
            folded.extend(chars[end].to_lowercase());
            if folded == query || folded.starts_with(&query) {
                matched = Some(end + 1);
                break;
            }
            if !query.starts_with(&folded) {
                break;
            }
            end += 1;
        }
        if let Some(end) = matched {
            ranges.push((start, end));
            start = end;
        } else {
            start += 1;
        }
    }
    ranges
}

pub fn badges(
    task: &Task,
    session_state: Option<SessionState>,
    app: &App,
) -> Vec<(String, ratatui::style::Color)> {
    let mut badges = Vec::new();
    match session_state {
        Some(SessionState::Live) => badges.push(("▶ running".to_string(), app.theme.ok)),
        Some(SessionState::Waiting) => {
            let label = app
                .board
                .session_wait_deadlines
                .get(&task.id)
                .map(|deadline| format!("⏳ until {}", deadline.format("%H:%M")))
                .unwrap_or_else(|| "⏳ wait mode".to_string());
            badges.push((label, app.theme.warn));
        }
        Some(SessionState::Crashed) => {
            badges.push(("✖ crashed · u recover".to_string(), app.theme.err))
        }
        None => {}
    }
    if task.interactive {
        badges.push(("☑ interactive".to_string(), app.theme.ok));
    }
    if task.chained_to.is_some() {
        badges.push(("↪ chain".to_string(), app.theme.ok));
    }
    if task.has_questions {
        badges.push(("? questions".to_string(), app.theme.warn));
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
    use ratatui::style::Color;

    use super::{
        case_insensitive_match, highlight_title_matches, sanitize_terminal_text, truncate_display,
    };

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

    #[test]
    fn highlights_case_insensitive_unicode_matches_without_byte_indices() {
        let spans = highlight_title_matches("İSTANBUL İSTANBUL", "i̇s", Color::Yellow);
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "İSTANBUL İSTANBUL"
        );
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.style.fg == Some(Color::Yellow))
                .count(),
            2
        );
    }

    #[test]
    fn filtering_and_highlighting_agree_for_expanded_unicode_lowercase() {
        let spans = highlight_title_matches("İstanbul", "i", Color::Yellow);
        assert!(case_insensitive_match("İstanbul", "i"));
        assert_eq!(spans[0].content.as_ref(), "İ");
        assert_eq!(spans[0].style.fg, Some(Color::Yellow));
    }
}
