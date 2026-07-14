use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::core::session::{SessionManager, estimate_session_tokens};

use super::app::App;
use super::card::{sanitize_terminal_text, truncate_display};

pub fn render_sessions(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let sessions = SessionManager::new(&app.project_path).list_active_sessions();
    if sessions.is_empty() {
        frame.render_widget(
            Paragraph::new("No active sessions")
                .block(Block::default().title(" Sessions ").borders(Borders::ALL)),
            area,
        );
        return;
    }
    let items = sessions
        .iter()
        .map(|session| {
            let task_label = app
                .ops
                .get_task(&session.task_id)
                .ok()
                .flatten()
                .map(|task| sanitize_terminal_text(&format!("{} {}", task.id, task.title)))
                .unwrap_or_else(|| session.task_id.clone());
            let tokens = estimate_session_tokens(&app.project_path, &session.id)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            ListItem::new(Line::from(vec![
                Span::styled("▶ ", Style::default().fg(app.theme.ok)),
                Span::raw(truncate_display(&sanitize_terminal_text(&session.id), 26)),
                Span::raw("  "),
                Span::raw(truncate_display(&task_label, 38)),
                Span::raw("  tokens: "),
                Span::raw(tokens),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(Some(
        app.session_selected.min(items.len().saturating_sub(1)),
    ));
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Sessions ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.focus)),
        )
        .highlight_style(
            Style::default()
                .fg(app.theme.focus)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

pub fn render_archive(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let tasks = app.ops.list_archived_tasks(None).unwrap_or_default();
    if tasks.is_empty() {
        frame.render_widget(
            Paragraph::new("Archive is empty")
                .block(Block::default().title(" Archive ").borders(Borders::ALL)),
            area,
        );
        return;
    }
    let items = tasks
        .iter()
        .map(|task| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    sanitize_terminal_text(&task.id),
                    Style::default().fg(app.theme.muted),
                ),
                Span::raw("  "),
                Span::raw(truncate_display(&sanitize_terminal_text(&task.title), 70)),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(Some(
        app.archive_selected.min(items.len().saturating_sub(1)),
    ));
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Archive ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.focus)),
        )
        .highlight_style(
            Style::default()
                .fg(app.theme.focus)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}
