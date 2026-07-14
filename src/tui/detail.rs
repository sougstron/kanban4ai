use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::core::models::{Message, MessageKind, MessageStatus};
use crate::core::timefmt;

use super::app::App;
use super::card::sanitize_terminal_text;

pub fn render(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Min(5),
            Constraint::Length(6),
        ])
        .split(area);
    let Some(detail) = app.detail.as_ref() else {
        frame.render_widget(Paragraph::new("No task selected"), area);
        return;
    };
    let Some(task) = detail.task.as_ref() else {
        frame.render_widget(Paragraph::new("Task not found"), area);
        return;
    };
    let meta = vec![
        Line::from(vec![
            Span::styled(
                &task.id,
                Style::default()
                    .fg(app.theme.focus)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::raw(sanitize_terminal_text(&task.title)),
        ]),
        Line::from(format!(
            "Status: {} │ Session: {} │ Interactive: {}",
            task.status,
            sanitize_terminal_text(task.session.as_deref().unwrap_or("-")),
            task.interactive
        )),
        Line::from(format!(
            "Backend: {} │ Model: {} │ Agent: {} │ Chain: {}",
            sanitize_terminal_text(task.agent_backend.as_deref().unwrap_or("-")),
            sanitize_terminal_text(task.ai_model.as_deref().unwrap_or("-")),
            sanitize_terminal_text(task.agent_name.as_deref().unwrap_or("-")),
            sanitize_terminal_text(task.chained_to.as_deref().unwrap_or("-"))
        )),
        Line::from(format!(
            "Created: {} │ Updated: {}",
            timefmt::format(&task.created_at),
            timefmt::format(&task.updated_at)
        )),
    ];
    frame.render_widget(
        Paragraph::new(meta)
            .block(
                Block::default()
                    .title(" Task ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.theme.focus)),
            )
            .style(Style::default().bg(app.theme.bg).fg(app.theme.fg)),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(thread_lines(&detail.messages, app))
            .block(
                Block::default()
                    .title(" Thread ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.theme.border)),
            )
            .style(Style::default().bg(app.theme.bg).fg(app.theme.fg))
            .wrap(Wrap { trim: false })
            .scroll((detail.scroll, 0)),
        chunks[1],
    );

    let mut review_edits = detail.review_edits.clone();
    review_edits.set_block(
        Block::default()
            .title(if detail.review_editing {
                " Review edits [focused] · Tab thread · Ctrl+S Save & Re-run "
            } else {
                " Review edits · Tab edit · Ctrl+S Save & Re-run "
            })
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.review)),
    );
    frame.render_widget(&review_edits, chunks[2]);
}

fn thread_lines(messages: &[Message], app: &App) -> Vec<Line<'static>> {
    if messages.is_empty() {
        return vec![Line::from("No thread messages")];
    }
    let mut lines = Vec::new();
    for message in messages {
        let style = match message.kind {
            MessageKind::Question => Style::default()
                .fg(app.theme.warn)
                .add_modifier(Modifier::BOLD),
            MessageKind::Suggestion => Style::default().fg(app.theme.focus),
            MessageKind::Context => Style::default().fg(app.theme.muted),
            MessageKind::ReviewEdit => Style::default().fg(app.theme.review),
            MessageKind::Task => Style::default().fg(app.theme.ok),
            MessageKind::System => Style::default().fg(app.theme.muted),
        };
        let status = match message.status {
            MessageStatus::Open => "open",
            MessageStatus::Answered => "answered",
            MessageStatus::Resolved => "resolved",
        };
        lines.push(Line::from(Span::styled(
            format!("{} · {} · {}", message.id, message.kind, status),
            style,
        )));
        lines.push(Line::from(sanitize_terminal_text(&message.body)));
        if !message.variants.is_empty() {
            lines.push(Line::from(format!(
                "Variants: {}",
                sanitize_terminal_text(&message.variants.join(" | "))
            )));
        }
        if let Some(answer) = &message.answer {
            lines.push(Line::from(Span::styled(
                format!("Answer: {}", sanitize_terminal_text(answer)),
                Style::default().fg(app.theme.ok),
            )));
        }
        lines.push(Line::from(""));
    }
    lines
}
