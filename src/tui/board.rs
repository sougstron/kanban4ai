use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::app::{App, CardHitbox, Screen};
use super::card::sanitize_terminal_text;
use super::{card, detail, dialogs, search, sessions};

pub fn ui(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if !matches!(app.screen, Screen::Board | Screen::Help) {
        app.card_hitboxes.clear();
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    match app.screen {
        Screen::Board => render_board(frame, app, chunks[0]),
        Screen::Detail => detail::render(frame, app, chunks[0]),
        Screen::Sessions => sessions::render_sessions(frame, app, chunks[0]),
        Screen::Archive => sessions::render_archive(frame, app, chunks[0]),
        Screen::Help => {
            render_board(frame, app, chunks[0]);
            render_help(frame, app, centered_rect(70, 55, area));
        }
    }
    if app.search.active {
        search::render(frame, app, chunks[0]);
    }
    if let Some(modal) = &app.modal {
        dialogs::render(frame, app, modal, centered_rect(70, 70, area));
    }
    render_status(frame, app, chunks[1]);
}

fn render_board(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let mut hitboxes = Vec::new();
    let column_count = app.board.columns.len().max(1) as u32;
    let constraints = (0..column_count)
        .map(|_| Constraint::Ratio(1, column_count))
        .collect::<Vec<_>>();
    let areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);
    for (index, column) in app.board.columns.iter().enumerate() {
        let focused = index == app.focused_column;
        let border_style = if focused {
            Style::default().fg(app.theme.focus)
        } else {
            Style::default().fg(app.theme.border)
        };
        let block = Block::default()
            .title(format!(
                " {} · {} ({}) ",
                sanitize_terminal_text(&column.name),
                sanitize_terminal_text(&column.id),
                column.tasks.len()
            ))
            .borders(Borders::ALL)
            .border_style(border_style)
            .style(Style::default().bg(app.theme.bg).fg(app.theme.fg));
        let inner = block.inner(areas[index]);
        frame.render_widget(block, areas[index]);
        hitboxes.extend(render_cards(frame, app, index, inner));
    }
    app.card_hitboxes = hitboxes;
}

fn render_cards(
    frame: &mut Frame<'_>,
    app: &App,
    column_index: usize,
    area: Rect,
) -> Vec<CardHitbox> {
    let card_height = app.settings.card_height_lines.max(1);
    let visible = (area.height / card_height).max(1) as usize;
    let offset = app.column_offsets.get(column_index).copied().unwrap_or(0);
    let all_tasks = app.visible_tasks_for_column(column_index);
    let tasks = all_tasks
        .into_iter()
        .skip(offset)
        .take(visible)
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        let paragraph = Paragraph::new("No tasks")
            .style(Style::default().fg(app.theme.muted))
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, area);
        return Vec::new();
    }
    let constraints = tasks
        .iter()
        .map(|_| Constraint::Length(card_height))
        .collect::<Vec<_>>();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut hitboxes = Vec::new();
    for (task_index, task) in tasks.into_iter().enumerate() {
        let absolute_index = offset + task_index;
        let focused = column_index == app.focused_column && absolute_index == app.focused_card;
        card::render_card(frame, app, task, rows[task_index], focused);
        hitboxes.push(CardHitbox {
            column: column_index,
            card: absolute_index,
            area: rows[task_index],
        });
    }
    hitboxes
}

fn render_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let status = format!(
        " {} │ Enter detail │ / search │ n/e/m/d/s/w actions │ a archive │ l sessions │ Ctrl+T theme │ ? help │ q quit │ refreshed {:?} ago ",
        app.status,
        app.board.loaded_at.elapsed()
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().bg(app.theme.border).fg(app.theme.fg)),
        area,
    );
}

fn render_help(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(Clear, area);
    let help = vec![
        Line::from("kanban4ai TUI"),
        Line::from(""),
        Line::from("←/→ or Tab/Shift+Tab: switch columns"),
        Line::from("↑/↓ PgUp/PgDn Home/End: navigate cards or scroll detail"),
        Line::from("Enter: detail / attach selected session"),
        Line::from("n/e/m/d/s/w/r: new, edit, move, delete, delegate, answer, recover"),
        Line::from("a/l: archive and sessions"),
        Line::from("/: search"),
        Line::from("Ctrl+T: cycle and persist theme"),
        Line::from("Detail Tab: switch thread/editor · Ctrl+S: save and re-run"),
        Line::from("?: toggle help"),
        Line::from("q/Esc/Ctrl+C: quit"),
    ];
    frame.render_widget(
        Paragraph::new(help)
            .block(
                Block::default()
                    .title(" Help ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.theme.focus)),
            )
            .style(Style::default().bg(app.theme.bg).fg(app.theme.fg))
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}
