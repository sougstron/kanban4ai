use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

use crate::core::session::SessionState;

use super::app::App;
use super::card::{sanitize_terminal_text, truncate_display};

const SESSIONS_TITLE: &str = " Sessions · Enter attach · v log · x kill · o task · q back ";
const ARCHIVE_TITLE: &str = " Archive · Enter detail · u restore · / filter · q back ";

pub fn render_sessions(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let active_sessions = app.filtered_active_sessions();
    if active_sessions.is_empty() {
        let message = if app.search.text().is_empty() {
            "No sessions"
        } else {
            "No sessions match filter"
        };
        frame.render_widget(
            Paragraph::new(message)
                .block(Block::default().title(SESSIONS_TITLE).borders(Borders::ALL)),
            area,
        );
        return;
    }
    let items = active_sessions
        .iter()
        .map(|active_session| {
            let session = &active_session.session;
            let (icon, color) = match active_session.state {
                SessionState::Live => ("▶ ", app.theme.ok),
                SessionState::Waiting => ("⏳ ", app.theme.warn),
                SessionState::Crashed => ("✖ ", app.theme.err),
            };
            let wait = if active_session.state == SessionState::Waiting {
                session
                    .wait_until
                    .map(|deadline| format!("  until {}", deadline.format("%H:%M")))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            ListItem::new(Line::from(vec![
                Span::styled(icon, Style::default().fg(color)),
                Span::raw(truncate_display(&sanitize_terminal_text(&session.id), 26)),
                Span::raw("  "),
                Span::raw(truncate_display(
                    &sanitize_terminal_text(&active_session.task_label),
                    38,
                )),
                Span::raw("  tokens: "),
                Span::raw(&active_session.token_display),
                Span::styled(wait, Style::default().fg(color)),
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
                .title(SESSIONS_TITLE)
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
    let tasks = app.filtered_archived_tasks();
    if tasks.is_empty() {
        let message = if app.search.text().is_empty() {
            "Archive is empty"
        } else {
            "No archived tasks match filter"
        };
        frame.render_widget(
            Paragraph::new(message)
                .block(Block::default().title(ARCHIVE_TITLE).borders(Borders::ALL)),
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
                .title(ARCHIVE_TITLE)
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

/// Pager over the cached tail of the selected session's log. The scroll is
/// clamped against the real viewport here; follow mode pins it to the bottom
/// so the tick-driven reload behaves like `tail -f`.
pub fn render_log_view(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let theme = app.theme;
    let Some(log) = app.log_view.as_mut() else {
        frame.render_widget(
            Paragraph::new("No session log selected")
                .block(Block::default().title(" Log ").borders(Borders::ALL)),
            area,
        );
        return;
    };
    let visible_height = area.height.saturating_sub(2);
    log.max_scroll = (log.lines.len() as u16).saturating_sub(visible_height);
    log.scroll = if log.follow {
        log.max_scroll
    } else {
        log.scroll.min(log.max_scroll)
    };
    let title = format!(
        " Log · {} · ↑/↓ PgUp/PgDn scroll · End follow · q back ",
        truncate_display(&sanitize_terminal_text(&log.session_id), 40)
    );
    let lines = log
        .lines
        .iter()
        .map(|line| Line::from(line.clone()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.focus)),
            )
            .style(Style::default().bg(theme.bg).fg(theme.fg))
            .scroll((log.scroll, 0)),
        area,
    );
    if log.max_scroll > 0 {
        let mut scrollbar_state =
            ScrollbarState::new(log.max_scroll as usize).position(log.scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
}
