use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use unicode_width::UnicodeWidthStr;

use super::app::{App, HitAction, Hitbox, Screen, UiAction};

use super::card::{sanitize_terminal_text, truncate_display};
use super::{card, detail, dialogs, limits, projects, search, sessions};

pub fn ui(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // Reset the full frame so stale symbols from a previous frame (e.g. a
    // closed modal or a shorter card list) cannot bleed through. ratatui's
    // Block only sets style, never clears the cell symbol, so uncovered cells
    // would otherwise retain whatever the previous widget wrote there.
    frame.render_widget(Clear, area);
    // Renderers re-register their regions on every frame.
    app.hitboxes.clear();
    // Content, the optional provider-limits row, then the status bar. The
    // limits row is zero-height (and skipped) until a snapshot exists, so the
    // layout is unchanged on screens and runs that have no limits to show.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(limits::row_height(app)),
            Constraint::Length(1),
        ])
        .split(area);
    match app.screen {
        Screen::Board => render_board(frame, app, chunks[0]),
        Screen::Detail => detail::render(frame, app, chunks[0]),
        Screen::Sessions => sessions::render_sessions(frame, app, chunks[0]),
        Screen::Archive => sessions::render_archive(frame, app, chunks[0]),
        Screen::LogView => sessions::render_log_view(frame, app, chunks[0]),
        Screen::TextView => sessions::render_text_view(frame, app, chunks[0]),
        Screen::Projects => projects::render(frame, app, chunks[0]),
        Screen::Help => {
            if app.help_return == Screen::Projects {
                projects::render(frame, app, chunks[0]);
            } else {
                render_board(frame, app, chunks[0]);
            }
            render_help(frame, app, area);
        }
    }
    if app.search.active {
        search::render(frame, app, chunks[0]);
    }
    if let Some(mut modal) = app.modal.take() {
        let modal_hits = dialogs::render(frame, app, &mut modal, centered_rect(70, 70, area));
        // Modal controls must win hit testing over every underlying board or
        // detail region, preventing click-through and drag initiation.
        app.hitboxes.splice(0..0, modal_hits);
        app.modal = Some(modal);
    }
    limits::render(frame, app, chunks[1]);
    render_status(frame, app, chunks[2]);
    app.capture_and_highlight(frame.buffer_mut());
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
    // The project badge goes on the rightmost column, whose top border row is
    // the only one with a free right corner; it is drawn after the loop so the
    // block cannot paint over it.
    let mut badge_slot = None;
    for index in 0..app.board.columns.len() {
        let column = &app.board.columns[index];
        let focused = index == app.focused_column;
        // A card being dropped here (a column other than its own) gets a
        // distinct bold "drop zone" border so the drag reads differently from
        // ordinary keyboard focus.
        let drop_target = app.drop_target_column() == Some(index);
        let border_style = if drop_target {
            Style::default()
                .fg(app.theme.ok)
                .add_modifier(Modifier::BOLD)
        } else if focused {
            Style::default().fg(app.theme.focus)
        } else {
            Style::default().fg(app.theme.border)
        };
        let matching = app.matching_task_count(index);
        let shown = matching.min(app.settings.max_tasks_per_column);
        let count = if matching > shown {
            format!("({shown} of {matching})")
        } else {
            format!("({matching})")
        };
        let title = column_title(&column.name, &count, areas[index].width.saturating_sub(2));
        if index + 1 == app.board.columns.len() {
            badge_slot = Some((areas[index], UnicodeWidthStr::width(title.as_str()) as u16));
        }
        let block = Block::default()
            .title(title.clone())
            .borders(Borders::ALL)
            .border_style(border_style)
            .style(Style::default().bg(app.theme.bg).fg(app.theme.fg));
        let inner = block.inner(areas[index]);
        let capacity = card_capacity_with_indicators(
            inner,
            app.settings.card_height_lines.max(1),
            app.column_offsets.get(index).copied().unwrap_or(0),
            matching,
        );
        app.set_visible_card_capacity(index, capacity);
        frame.render_widget(block, areas[index]);
        // Cards first: hitboxes are searched front-to-back, and the column
        // area encloses them.
        hitboxes.extend(render_cards(frame, app, index, inner));
        hitboxes.push(Hitbox {
            area: areas[index],
            action: HitAction::ColumnFocus(index),
        });
    }
    if let Some((block, title_width)) = badge_slot
        && let Some(hitbox) = render_project_badge(frame, app, block, title_width)
    {
        // The badge lies on the last column's border, so it has to be searched
        // before that column's own region.
        hitboxes.insert(0, hitbox);
    }
    app.hitboxes = hitboxes;
}

/// Marker in front of the project name, so the badge reads as a label rather
/// than as one more column title on the same row.
const BADGE_MARKER: &str = " ▸ ";
/// Shorter than this a truncated name stops being an orientation cue, so the
/// badge is dropped and the frame is left clean instead.
const BADGE_MIN_NAME: usize = 4;

fn badge_chrome() -> usize {
    // The marker, plus the trailing space that keeps the name off the corner.
    UnicodeWidthStr::width(BADGE_MARKER) + 1
}

/// The project name fitted to the room left on a block's top border row, or
/// `None` when there is not enough of it for a readable label.
fn badge_name(name: &str, available: u16) -> Option<String> {
    let name = sanitize_terminal_text(name);
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let room = (available as usize).checked_sub(badge_chrome())?;
    if room < UnicodeWidthStr::width(name).min(BADGE_MIN_NAME) {
        return None;
    }
    Some(truncate_display(name, room))
}

/// Draw the open project's name at the right end of `block`'s top border row —
/// the row that already carries the block's own title, so a permanent "which
/// board am I looking at" anchor costs no card space. `title_width` is what
/// that left-aligned title took; one more cell is kept clear so the two never
/// run together.
///
/// Returns the badge's hitbox. It sits inside the block, so callers must
/// register it ahead of whatever region encloses it.
pub(super) fn render_project_badge(
    frame: &mut Frame<'_>,
    app: &App,
    block: Rect,
    title_width: u16,
) -> Option<Hitbox> {
    if !app.has_board() || block.width == 0 || block.height == 0 {
        return None;
    }
    // Both corners, the existing title, and the gap after it are spoken for.
    let available = block
        .width
        .saturating_sub(2)
        .saturating_sub(title_width)
        .saturating_sub(1);
    let name = badge_name(&app.settings.project_name, available)?;
    let width = (UnicodeWidthStr::width(name.as_str()) + badge_chrome()) as u16;
    let area = Rect {
        x: block.x + block.width.saturating_sub(width.saturating_add(1)),
        y: block.y,
        width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(BADGE_MARKER, Style::default().fg(app.theme.muted)),
            Span::styled(
                name,
                Style::default()
                    .fg(app.theme.fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]))
        .style(Style::default().bg(app.theme.bg)),
        area,
    );
    Some(Hitbox {
        area,
        action: HitAction::Action(UiAction::OpenProjects),
    })
}

fn column_title(name: &str, count: &str, available: u16) -> String {
    let name = sanitize_terminal_text(name);
    let full = format!(" {name} {count} ");
    let available = available as usize;
    if UnicodeWidthStr::width(full.as_str()) <= available {
        return full;
    }
    let count_width = UnicodeWidthStr::width(count);
    let reserved = count_width.saturating_add(3);
    if available > reserved + 1 {
        let name_width = available - reserved;
        format!(" {} {count} ", truncate_display(&name, name_width))
    } else if available > count_width + 2 {
        format!(" … {count} ")
    } else {
        format!(" {count} ")
    }
}

fn render_cards(frame: &mut Frame<'_>, app: &App, column_index: usize, area: Rect) -> Vec<Hitbox> {
    let offset = app.column_offsets.get(column_index).copied().unwrap_or(0);
    let all_tasks = app.visible_tasks_for_column(column_index);
    let total = app.matching_task_count(column_index);
    // Cards are uniform within a column but the column grows to its tallest
    // card, so a running card's telemetry (or a questioned card's badges) is
    // never clipped, while columns of plain cards stay at the configured
    // minimum. `+2` accounts for the card border; the content count comes from
    // `card::card_line_count`.
    let base_height = app.settings.card_height_lines.max(1);
    let card_height = all_tasks
        .iter()
        .map(|task| card::card_line_count(app, task).saturating_add(2))
        .max()
        .unwrap_or(0)
        .max(base_height);
    let visible = card_capacity_with_indicators(area, card_height, offset, total);
    let tasks = all_tasks
        .into_iter()
        .skip(offset)
        .take(visible)
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        let message = if app.board.columns[column_index].id == "todo" {
            "press n to create task"
        } else {
            "No tasks"
        };
        let paragraph = Paragraph::new(message)
            .style(Style::default().fg(app.theme.muted))
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, area);
        return Vec::new();
    }
    let above = offset;
    let below = total.saturating_sub(offset.saturating_add(tasks.len()));
    let mut constraints = Vec::new();
    if above > 0 {
        constraints.push(Constraint::Length(1));
    }
    constraints.extend(tasks.iter().map(|_| Constraint::Length(card_height)));
    if below > 0 {
        constraints.push(Constraint::Length(1));
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut hitboxes = Vec::new();
    let mut row_index = 0;
    if above > 0 {
        frame.render_widget(
            Paragraph::new(format!("↑ {above} above")).style(Style::default().fg(app.theme.muted)),
            rows[row_index],
        );
        row_index += 1;
    }
    for (task_index, task) in tasks.into_iter().enumerate() {
        let absolute_index = offset + task_index;
        let focused = column_index == app.focused_column && absolute_index == app.focused_card;
        // One selection at a time: the pointer paints a card as hovered only
        // while that card already is the selection. After keyboard navigation
        // moves the selection away, the card under a stationary pointer stops
        // reading as selected until the pointer moves onto a card again.
        let hovered = focused
            && (app.is_hovered(HitAction::FocusCard {
                column: column_index,
                card: absolute_index,
            }) || app.is_hovered(HitAction::OpenAnswer {
                column: column_index,
                card: absolute_index,
            }));
        let row = rows[row_index];
        row_index += 1;
        let dragging = app.is_dragging_card(column_index, absolute_index);
        card::render_card(frame, app, task, row, focused, hovered, dragging);
        // The question-preview line (second content line of the card) jumps
        // straight to the answer panel; register it before the card region.
        let has_preview = app
            .board
            .extras
            .get(&task.id)
            .is_some_and(|extra| extra.question_preview.is_some());
        if has_preview && row.height >= 4 {
            hitboxes.push(Hitbox {
                area: Rect {
                    x: row.x + 1,
                    y: row.y + 2,
                    width: row.width.saturating_sub(2),
                    height: 1,
                },
                action: HitAction::OpenAnswer {
                    column: column_index,
                    card: absolute_index,
                },
            });
        }
        hitboxes.push(Hitbox {
            area: row,
            action: HitAction::FocusCard {
                column: column_index,
                card: absolute_index,
            },
        });
    }
    if below > 0 {
        frame.render_widget(
            Paragraph::new(format!("↓ {below} below")).style(Style::default().fg(app.theme.muted)),
            rows[row_index],
        );
    }
    hitboxes
}

fn card_capacity_with_indicators(
    area: Rect,
    card_height: u16,
    offset: usize,
    total: usize,
) -> usize {
    let above = usize::from(offset > 0);
    let provisional = (area.height.saturating_sub(above as u16) / card_height).max(1) as usize;
    let below = usize::from(total > offset.saturating_add(provisional));
    (area.height.saturating_sub((above + below) as u16) / card_height).max(1) as usize
}

/// A status-bar hint: a hotkey label, an optional click action, and a drop
/// priority — higher numbers disappear first when the bar overflows.
struct StatusSegment {
    label: &'static str,
    action: Option<UiAction>,
    priority: u8,
}

fn seg(label: &'static str, action: Option<UiAction>, priority: u8) -> StatusSegment {
    StatusSegment {
        label,
        action,
        priority,
    }
}

/// The `Q` hint for the current task, when it applies: a queued card offers
/// unqueueing; To Do and idle In Progress cards offer explicit enqueueing.
fn queue_segment(app: &App) -> Option<StatusSegment> {
    let task = app.current_task()?;
    match app.queue_action_for(&task)? {
        UiAction::Dequeue => Some(seg("Q unqueue", Some(UiAction::Dequeue), 3)),
        UiAction::Enqueue => Some(seg("Q queue", Some(UiAction::Enqueue), 3)),
        _ => None,
    }
}

fn status_segments(app: &App) -> Vec<StatusSegment> {
    let primary_action = match app.current_task() {
        Some(task) if app.primary_run_action_for(&task) == UiAction::Revoke => {
            ("r revoke", UiAction::Revoke)
        }
        _ => ("r run", UiAction::Run),
    };
    match app.screen {
        Screen::Board => {
            let mut segments = vec![
                seg("n new", Some(UiAction::NewTask), 2),
                seg("e edit", Some(UiAction::EditTask), 4),
                seg(primary_action.0, Some(primary_action.1), 1),
                seg("m move", Some(UiAction::MoveTask), 4),
                seg("y approve", Some(UiAction::Approve), 5),
                seg("A archive done", Some(UiAction::ArchiveAllDone), 6),
                seg("b review done", Some(UiAction::MarkReviewDone), 6),
                seg("/ filter", Some(UiAction::Search), 3),
                seg("s settings", Some(UiAction::OpenSettings), 7),
                seg("? help", Some(UiAction::Help), 1),
            ];
            if app
                .current_task()
                .is_some_and(|task| app.task_can_stop(&task))
            {
                segments.insert(3, seg("k stop", Some(UiAction::Stop), 2));
            }
            if let Some(queue) = queue_segment(app) {
                segments.insert(3, queue);
            }
            segments
        }
        Screen::Detail => {
            let show_tab = app.detail.as_ref().is_some_and(|detail| {
                !detail.open_questions().is_empty() || detail.show_edits_panel()
            });
            let mut segments = vec![
                seg(primary_action.0, Some(primary_action.1), 1),
                seg("w answer", Some(UiAction::AnswerQuestion), 3),
                seg("y approve", Some(UiAction::Approve), 3),
                seg("x reject", Some(UiAction::ToggleReject), 5),
            ];
            if app
                .current_task()
                .is_some_and(|task| app.task_can_stop(&task))
            {
                segments.insert(1, seg("k stop", Some(UiAction::Stop), 2));
            }
            if let Some(queue) = queue_segment(app) {
                segments.insert(1, queue);
            }
            if show_tab {
                segments.push(seg("Tab editor", None, 4));
                segments.push(seg("Ctrl+S save", Some(UiAction::SaveReviewEdits), 4));
            }
            segments.push(seg("q/Esc back", None, 2));
            segments
        }
        Screen::Sessions => vec![
            seg("Enter attach", Some(UiAction::OpenDetail), 1),
            seg("v log", Some(UiAction::ViewLog), 2),
            seg("x kill", Some(UiAction::KillSession), 2),
            seg("o task", Some(UiAction::OpenSessionTask), 3),
            seg("/ filter", Some(UiAction::Search), 4),
            seg("q back", None, 3),
        ],
        Screen::Archive => vec![
            seg("Enter detail", Some(UiAction::OpenDetail), 1),
            seg("u restore", Some(UiAction::Restore), 1),
            seg("/ filter", Some(UiAction::Search), 3),
            seg("q back", None, 2),
        ],
        Screen::LogView => vec![seg("↑/↓ scroll", None, 2), seg("q back", None, 1)],
        Screen::TextView => vec![seg("↑/↓ PgUp/PgDn scroll", None, 2), seg("q back", None, 1)],
        Screen::Help => vec![
            seg("↑/↓ scroll", None, 2),
            seg("? close", Some(UiAction::Help), 1),
        ],
        Screen::Projects => vec![
            seg("Enter open", Some(UiAction::OpenProject), 1),
            seg("n new", Some(UiAction::NewProject), 2),
            seg("r rename", Some(UiAction::RenameProject), 3),
            seg("p path", Some(UiAction::SetProjectPath), 4),
            // Ties with settings, which is listed after it and so drops
            // first: the title bar still advertises `s`, but the only hint
            // for the folder button is here.
            seg("o folder", Some(UiAction::OpenProjectFolder), 5),
            seg("d delete", Some(UiAction::DeleteProject), 3),
            seg("s settings", Some(UiAction::OpenGlobalSettings), 5),
            seg("/ filter", Some(UiAction::Search), 4),
            seg("? help", Some(UiAction::Help), 1),
        ],
    }
}

/// Keep segments in order but drop the least important ones (highest
/// priority number, later ones first on ties) until the row fits.
fn fit_segments(segments: Vec<StatusSegment>, available: u16) -> Vec<StatusSegment> {
    let mut kept = segments;
    loop {
        let width = kept
            .iter()
            .enumerate()
            .map(|(index, segment)| {
                UnicodeWidthStr::width(segment.label) as u16 + if index == 0 { 2 } else { 3 }
            })
            .sum::<u16>();
        if width <= available || kept.is_empty() {
            return kept;
        }
        let drop = kept
            .iter()
            .enumerate()
            .max_by_key(|(index, segment)| (segment.priority, *index))
            .map(|(index, _)| index)
            .expect("kept is non-empty");
        kept.remove(drop);
    }
}

/// Screen-specific status bar: warning chips, the last status message, then
/// clickable hotkey hints for the current screen.
fn render_status(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let question_count = app
        .board
        .columns
        .iter()
        .enumerate()
        .flat_map(|(column, _)| app.visible_tasks_for_column(column))
        .filter(|task| task.has_questions)
        .count();
    let mut spans = Vec::new();
    let mut x = area.x;
    let end = area.x.saturating_add(area.width);
    if question_count > 0 {
        let chip = format!(" ? {question_count} questions ");
        let width = UnicodeWidthStr::width(chip.as_str()) as u16;
        app.hitboxes.push(Hitbox {
            area: Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            action: HitAction::Action(UiAction::FocusQuestions),
        });
        spans.push(Span::styled(chip, Style::default().fg(app.theme.warn)));
        x = x.saturating_add(width);
    }
    let filter = truncate_display(&sanitize_terminal_text(&app.search.text()), 32);
    if !filter.is_empty() && !app.search.active {
        let chip = format!(" Filter: \"{filter}\" · Esc clear ");
        let width = UnicodeWidthStr::width(chip.as_str()) as u16;
        app.hitboxes.push(Hitbox {
            area: Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            action: HitAction::Action(UiAction::ClearSearch),
        });
        spans.push(Span::styled(chip, Style::default().fg(app.theme.warn)));
        x = x.saturating_add(width);
    }
    let remaining = end.saturating_sub(x);
    let max_message_width = remaining.saturating_sub(20).max(remaining / 3) as usize;
    // A live drag takes over the message slot so the user can see what is
    // being moved and where; the normal status returns once the drag ends.
    let message_text = app.drag_hint().unwrap_or_else(|| app.status.clone());
    let raw_message = format!(" {} │", sanitize_terminal_text(&message_text));
    let message = truncate_display(&raw_message, max_message_width.max(1));
    x = x.saturating_add(UnicodeWidthStr::width(message.as_str()) as u16);
    spans.push(Span::raw(message));
    let available = end.saturating_sub(x);
    for (index, segment) in fit_segments(status_segments(app), available)
        .into_iter()
        .enumerate()
    {
        let text = if index == 0 {
            format!(" {} ", segment.label)
        } else {
            format!("· {} ", segment.label)
        };
        let width = UnicodeWidthStr::width(text.as_str()) as u16;
        if let Some(action) = segment.action {
            app.hitboxes.push(Hitbox {
                area: Rect {
                    x,
                    y: area.y,
                    width,
                    height: 1,
                },
                action: HitAction::Action(action),
            });
        }
        spans.push(Span::raw(text));
        x = x.saturating_add(width);
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().bg(app.theme.border).fg(app.theme.fg)),
        area,
    );
}

/// Help overlay sized to its content (capped at 90% of the frame) and
/// scrollable with ↑/↓ when the terminal is shorter than the text.
fn render_help(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let help = help_lines();
    let width = (help.iter().map(Line::width).max().unwrap_or(0) as u16 + 4)
        .min(area.width.saturating_mul(9) / 10)
        .min(area.width);
    let height = (help.len() as u16 + 2)
        .min(area.height.saturating_mul(9) / 10)
        .min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let visible_height = rect.height.saturating_sub(2);
    app.help_max_scroll = (help.len() as u16).saturating_sub(visible_height);
    app.help_scroll = app.help_scroll.min(app.help_max_scroll);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(help)
            .block(
                Block::default()
                    .title(" Help · ↑/↓ scroll · ?/q close ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.theme.focus)),
            )
            .style(Style::default().bg(app.theme.bg).fg(app.theme.fg))
            .scroll((app.help_scroll, 0)),
        rect,
    );
    if app.help_max_scroll > 0 {
        let mut scrollbar_state =
            ScrollbarState::new(app.help_max_scroll as usize).position(app.help_scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            rect,
            &mut scrollbar_state,
        );
    }
}

fn help_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(concat!("kanban4ai TUI v", env!("CARGO_PKG_VERSION"))),
        Line::from(""),
        Line::from("Board"),
        Line::from("  ←/→ or Tab/Shift+Tab: switch columns"),
        Line::from("  ↑/↓ PgUp/PgDn Home/End: navigate cards"),
        Line::from("  Enter: open task detail"),
        Line::from("  r: run a task; revoke only while a session is live/crashed"),
        Line::from("  k: stop a live or waiting session (task stays In Progress)"),
        Line::from("  Q: queue for the dispatcher / take a queued task back out"),
        Line::from("  n: new task in focused column · e/m/d: edit, move, delete permanently"),
        Line::from("  w: answer question · y: approve Review → Done"),
        Line::from("  t: attach to the task's agent · c: add context/suggestion"),
        Line::from("  u: recover a crashed task · Ctrl+R: fold edits and re-run"),
        Line::from("  A: archive all Done · b: mark all Review tasks Done (R also works)"),
        Line::from("  a/l: archive and sessions views"),
        Line::from("  /: search · Esc clears an active filter"),
        Line::from("  s: project settings · Ctrl+T: cycle and persist theme"),
        Line::from("  P: projects list (also З on a Russian layout)"),
        Line::from("  Esc: projects list when enabled in global settings"),
        Line::from(""),
        Line::from("Detail"),
        Line::from("  Tab: cycle thread/answer/editor panels when present"),
        Line::from("  Enter: run To Do tasks only · r/buttons: run or revoke while live"),
        Line::from("  k: stop a live or waiting session without starting a new one"),
        Line::from("  Ctrl+S: save review edits (no re-run) · Ctrl+R: re-run"),
        Line::from("  s: project settings · Ctrl+T: cycle and persist theme"),
        Line::from("  Home/End: start/end of thread · q/Esc: back · Esc leaves text panels first"),
        Line::from("  [/]: select a thread message · x: toggle reject (quarantine) on it"),
        Line::from("  p: view assembled prompt · v: view inputs/provenance (when present)"),
        Line::from(""),
        Line::from("Sessions"),
        Line::from("  ▶ live · ⏳ declared wait · ✖ crashed heartbeats"),
        Line::from("  Enter: attach · v: view log · x: kill session · o: open task"),
        Line::from(""),
        Line::from("Archive"),
        Line::from("  Enter: detail · u: restore the task to To Do"),
        Line::from(""),
        Line::from("Projects"),
        Line::from("  Enter: open · n: new project · r: rename · p: change path · d: remove"),
        Line::from("  o: open the project folder in the desktop file manager"),
        Line::from("  s: global settings, incl. Esc-from-board and project sorting"),
        Line::from("  pinned + row creates a project for the current folder with no dialog"),
        Line::from("  delete unregisters by default; toggle also deletes board data"),
        Line::from(
            "  q: quit the TUI · Esc: back to the board, or quit when this is the entry screen",
        ),
        Line::from(""),
        Line::from("Log view"),
        Line::from("  ↑/↓ PgUp/PgDn Home/End: scroll · End re-enables follow · q: back"),
        Line::from(""),
        Line::from("Mouse"),
        Line::from("  hover a card: select it — Enter and hotkeys act on it; arrows"),
        Line::from("    move the selection and drop the pointer's card again"),
        Line::from("  click: open a card, press a button, or pick a dialog field"),
        Line::from("  wheel: scrolls the column under the cursor"),
        Line::from("  drag across text: copy it · hold Shift to select interactive text"),
        Line::from("  drag a card onto another column: move it (target column"),
        Line::from("    highlights green; status bar shows what moves where)"),
        Line::from("  status-bar hints are clickable; column headers show name and count"),
        Line::from(""),
        Line::from("Provider limits (row above the status bar, Board and Projects)"),
        Line::from("  ✳ claude · ✺ codex · ✕ grok · ◆ zai · ✦ synthetic · ◉ yolo"),
        Line::from(
            "  click a provider to refresh now · % left and ↻ reset · codex shows last-session age",
        ),
        Line::from("  hide the row with tui.show_limits: false · kanban limits prints it"),
        Line::from(""),
        Line::from("?: toggle help · q/Esc: back · Ctrl+C twice: quit"),
    ]
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
