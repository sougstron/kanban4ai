//! Trash screen: projects removed with their board data, newest first, each
//! with the time left before it is deleted for good. Opened with `t` from the
//! projects list; rows mirror the projects table (name line, folder line,
//! blank separator).

use chrono::NaiveDateTime;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::core::timefmt;

use super::app::{App, HitAction, Hitbox};
use super::card::{sanitize_terminal_text, truncate_display};
use super::projects::shorten_path;

const ROW_CONTENT_HEIGHT: u16 = 2;
const ROW_HEIGHT: u16 = ROW_CONTENT_HEIGHT + 1;

pub fn render(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let days = app.trash_retention_days();
    let block = Block::default()
        .title(" Trash · Enter restore · d delete for good · D empty · q back ")
        .title_bottom(format!(" removed projects are kept {days} days "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.focus));
    if app.trash_items.is_empty() {
        let message = format!(
            "Trash is empty. Projects removed with their board data stay here for {days} days."
        );
        frame.render_widget(Paragraph::new(message).block(block), area);
        return;
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let total = app.trash_items.len();
    let per_page = ((inner.height + 1) / ROW_HEIGHT).max(1) as usize;
    let selected = app.trash_selected.min(total - 1);
    app.trash_scroll = scroll_offset(app.trash_scroll, selected, per_page, total);
    let now = timefmt::now();

    for (slot, index) in (app.trash_scroll..total).take(per_page).enumerate() {
        let top = inner.y + slot as u16 * ROW_HEIGHT;
        let height = ROW_CONTENT_HEIGHT.min(inner.bottom().saturating_sub(top));
        if height == 0 {
            break;
        }
        let row = Rect {
            x: inner.x,
            y: top,
            width: inner.width,
            height,
        };
        let action = HitAction::FocusTrash { index };
        let background = if index == selected {
            Some(app.theme.border)
        } else if app.is_hovered(action) {
            Some(app.theme.hover)
        } else {
            None
        };
        if let Some(background) = background {
            frame.render_widget(Block::default().style(Style::default().bg(background)), row);
        }
        app.hitboxes.push(Hitbox { area: row, action });

        let item = &app.trash_items[index];
        let when = format!(
            "deleted {} · {}",
            item.deleted_at.format("%Y-%m-%d %H:%M"),
            time_left(now, item.expires_at)
        );
        let width = row.width as usize;
        let when_width = UnicodeWidthStr::width(when.as_str());
        let name_width = width.saturating_sub(when_width + 2).max(1);
        let name = truncate_display(&sanitize_terminal_text(&item.name), name_width);
        let gap = width.saturating_sub(UnicodeWidthStr::width(name.as_str()) + when_width);
        let folder = format!(
            "{} · id {}",
            shorten_path(&item.work_path),
            sanitize_terminal_text(&item.id)
        );
        let lines = vec![
            Line::from(vec![
                Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" ".repeat(gap)),
                Span::styled(when, Style::default().fg(app.theme.warn)),
            ]),
            Line::from(Span::styled(
                truncate_display(&folder, width),
                Style::default().fg(app.theme.muted),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), row);
    }
}

/// `expires in 6d 23h`, `expires in 40m`, or `expires now`.
fn time_left(now: NaiveDateTime, expires_at: NaiveDateTime) -> String {
    let left = expires_at - now;
    let minutes = left.num_minutes();
    if minutes <= 0 {
        return "expires now".to_string();
    }
    let (days, hours) = (left.num_days(), left.num_hours() % 24);
    if days > 0 {
        format!("expires in {days}d {hours}h")
    } else if hours > 0 {
        format!("expires in {hours}h {}m", minutes % 60)
    } else {
        format!("expires in {minutes}m")
    }
}

fn scroll_offset(current: usize, selected: usize, per_page: usize, total: usize) -> usize {
    let mut offset = current.min(total.saturating_sub(per_page));
    if selected < offset {
        offset = selected;
    } else if selected >= offset + per_page {
        offset = selected + 1 - per_page;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_left_rounds_down_to_the_largest_units() {
        let now = timefmt::parse("2026-10-24T12:00:00").unwrap();
        let at = |s: &str| timefmt::parse(s).unwrap();
        assert_eq!(
            time_left(now, at("2026-10-31T11:00:00")),
            "expires in 6d 23h"
        );
        assert_eq!(
            time_left(now, at("2026-10-24T14:30:00")),
            "expires in 2h 30m"
        );
        assert_eq!(time_left(now, at("2026-10-24T12:40:00")), "expires in 40m");
        assert_eq!(time_left(now, at("2026-10-24T11:00:00")), "expires now");
    }
}
