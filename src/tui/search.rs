use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear};
use tui_textarea::TextArea;

use super::app::App;
use super::board::centered_rect;
use super::card::sanitize_terminal_text;

#[derive(Clone)]
pub struct SearchState {
    pub active: bool,
    pub query: TextArea<'static>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            active: false,
            query: TextArea::new(vec![String::new()]),
        }
    }
}

impl SearchState {
    pub fn text(&self) -> String {
        sanitize_terminal_text(&self.query.lines().join(" "))
            .trim()
            .to_string()
    }
}

pub fn render(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rect = centered_rect(55, 12, area);
    frame.render_widget(Clear, rect);
    let mut query = app.search.query.clone();
    query.set_block(
        Block::default()
            .title(" Search · type to filter · Esc close ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.focus)),
    );
    query.set_style(
        Style::default()
            .bg(app.theme.bg)
            .fg(app.theme.fg)
            .add_modifier(Modifier::BOLD),
    );
    query.set_cursor_line_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(&query, rect);
}
