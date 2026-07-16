use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::core::models::{Message, MessageKind, MessageStatus, Task, TaskStatus};
use crate::core::session::SessionState;
use crate::core::timefmt;

use super::app::{App, DetailFocus, HitAction, Hitbox, UiAction};
use super::card::{sanitize_terminal_text, truncate_display};
use super::theme::Theme;

pub fn render(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(detail) = app.detail.as_ref() else {
        frame.render_widget(Paragraph::new("No task selected"), area);
        return;
    };
    let Some(task) = detail.task.as_ref() else {
        frame.render_widget(Paragraph::new("Task not found"), area);
        return;
    };

    let theme = app.theme;
    let waiting = app
        .board
        .extras
        .get(&task.id)
        .is_some_and(|extra| extra.waiting);
    let session_state = app.board.session_states.get(&task.id).copied();
    let wait_deadline = app.board.session_wait_deadlines.get(&task.id).copied();
    let wait_note = app.board.session_wait_notes.get(&task.id).cloned();
    let open_questions = detail
        .open_questions()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let show_answer = !open_questions.is_empty();
    let show_edits = detail.show_edits_panel();
    let edits_editable = detail.edits_editable();
    let focus = detail.focus;
    let task = task.clone();

    let description_lines = task_description_lines(&task);
    let meta_extra_lines = u16::from(waiting)
        + u16::from(wait_deadline.is_some())
        + u16::from(session_state == Some(SessionState::Crashed))
        + description_lines;
    let meta_height = 6 + meta_extra_lines;
    let answer_height = if show_answer {
        let question = &open_questions[detail.question_index.min(open_questions.len() - 1)];
        let desired = 4 + question.variants.len() as u16 + 1;
        let reserved = meta_height
            .saturating_add(if show_edits { 6 } else { 0 })
            .saturating_add(1)
            .saturating_add(3);
        desired.min(area.height.saturating_sub(reserved).max(5))
    } else {
        0
    };
    let mut constraints = vec![
        Constraint::Length(meta_height),
        Constraint::Min(5),
        Constraint::Length(answer_height),
        Constraint::Length(if show_edits { 6 } else { 0 }),
        Constraint::Length(1),
    ];
    if !show_answer {
        constraints[2] = Constraint::Length(0);
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let meta_state = MetaState {
        waiting,
        wait_deadline,
        wait_note: wait_note.as_deref(),
        session_state,
    };
    render_meta(frame, &theme, &task, &meta_state, chunks[0]);

    // Thread panel with a clamped scroll and a scrollbar.
    let thread_lines = thread_lines(&app.detail.as_ref().unwrap().messages, &theme);
    // Approximation: logical lines; wrapped lines may add a little slack.
    let content_height = thread_lines.len() as u16;
    let visible_height = chunks[1].height.saturating_sub(2);
    let max_scroll = content_height.saturating_sub(visible_height);
    let scroll = {
        let detail = app.detail.as_mut().unwrap();
        detail.max_scroll = max_scroll;
        detail.scroll = detail.scroll.min(max_scroll);
        detail.scroll
    };
    app.hitboxes.push(Hitbox {
        area: chunks[1],
        action: HitAction::DetailThread,
    });
    frame.render_widget(
        Paragraph::new(thread_lines)
            .block(
                Block::default()
                    .title(" Thread ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if focus == DetailFocus::Thread {
                        theme.focus
                    } else {
                        theme.border
                    })),
            )
            .style(Style::default().bg(theme.bg).fg(theme.fg))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        chunks[1],
    );
    if max_scroll > 0 {
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll as usize).position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            chunks[1],
            &mut scrollbar_state,
        );
    }

    if show_answer {
        render_answer_panel(frame, app, &theme, &open_questions, chunks[2]);
    }

    if show_edits {
        render_edits_panel(frame, app, &theme, edits_editable, chunks[3]);
    }

    render_action_bar(frame, app, &theme, &task, show_answer, chunks[4]);
}

fn task_description_lines(task: &Task) -> u16 {
    if task.description.trim().is_empty() {
        0
    } else {
        1 + task.description.lines().take(3).count() as u16
    }
}

struct MetaState<'a> {
    waiting: bool,
    wait_deadline: Option<chrono::NaiveDateTime>,
    wait_note: Option<&'a str>,
    session_state: Option<SessionState>,
}

fn render_meta(
    frame: &mut Frame<'_>,
    theme: &Theme,
    task: &Task,
    meta_state: &MetaState<'_>,
    area: Rect,
) {
    let mut meta = vec![
        Line::from(vec![
            Span::styled(
                task.id.clone(),
                Style::default()
                    .fg(theme.focus)
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
            "Backend: {} │ Model: {} │ Effort: {} │ Agent: {} │ Chain: {}",
            sanitize_terminal_text(task.agent_backend.as_deref().unwrap_or("-")),
            sanitize_terminal_text(task.ai_model.as_deref().unwrap_or("-")),
            sanitize_terminal_text(task.ai_effort.as_deref().unwrap_or("-")),
            sanitize_terminal_text(task.agent_name.as_deref().unwrap_or("-")),
            sanitize_terminal_text(task.chained_to.as_deref().unwrap_or("-"))
        )),
        Line::from(format!(
            "Created: {} │ Updated: {}",
            timefmt::format(&task.created_at),
            timefmt::format(&task.updated_at)
        )),
    ];
    if !task.description.trim().is_empty() {
        meta.push(Line::from(Span::styled(
            "Description:",
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        )));
        let width = area.width.saturating_sub(4).max(1) as usize;
        for line in task.description.lines().take(3) {
            meta.push(Line::from(truncate_display(
                &sanitize_terminal_text(line),
                width,
            )));
        }
    }
    if meta_state.waiting {
        meta.push(Line::from(Span::styled(
            "⏳ Agent is blocked on a question and waits for your answer",
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        )));
    }
    if let Some(deadline) = meta_state.wait_deadline {
        let note = meta_state
            .wait_note
            .map(sanitize_terminal_text)
            .filter(|note| !note.is_empty())
            .unwrap_or_else(|| "declared wait".to_string());
        meta.push(Line::from(Span::styled(
            format!("⏳ Waiting until {} — {note}", deadline.format("%H:%M")),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        )));
    }
    if meta_state.session_state == Some(SessionState::Crashed)
        && task.status == TaskStatus::InProgress
    {
        meta.push(Line::from(Span::styled(
            "✖ Session is stuck or crashed — press u / Recover to return it to To Do",
            Style::default().fg(theme.err).add_modifier(Modifier::BOLD),
        )));
    }
    frame.render_widget(
        Paragraph::new(meta)
            .block(
                Block::default()
                    .title(" Task ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.focus)),
            )
            .style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );
}

fn render_answer_panel(
    frame: &mut Frame<'_>,
    app: &mut App,
    theme: &Theme,
    open_questions: &[Message],
    area: Rect,
) {
    let (focused, index, input_text, selected, question_body, variants) = {
        let detail = app.detail.as_ref().unwrap();
        let index = detail.question_index.min(open_questions.len() - 1);
        let question = &open_questions[index];
        (
            detail.focus == DetailFocus::Answer,
            index,
            sanitize_terminal_text(&detail.answer_input.lines().join(" ")),
            detail.variant_selected,
            sanitize_terminal_text(&question.body),
            question.variants.clone(),
        )
    };

    let title = format!(
        " Answer question {}/{} · ←→ question · ↑↓ variant · Enter send ",
        index + 1,
        open_questions.len()
    );
    let custom_label = if input_text.is_empty() {
        "Custom answer: (type to fill)".to_string()
    } else {
        format!("Custom answer: {input_text}")
    };
    let mut options = vec![option_line(
        &custom_label,
        selected == 0,
        focused,
        app.is_hovered(HitAction::DetailAnswerOption { index: 0 }),
        theme,
    )];
    for (variant_index, variant) in variants.iter().enumerate() {
        let option_index = variant_index + 1;
        options.push(option_line(
            &format!("{}. {}", variant_index + 1, sanitize_terminal_text(variant)),
            selected == option_index,
            focused,
            app.is_hovered(HitAction::DetailAnswerOption {
                index: option_index,
            }),
            theme,
        ));
    }
    let mut lines = vec![Line::from(Span::styled(
        question_body,
        Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
    ))];
    let visible_options = area.height.saturating_sub(3).max(1) as usize;
    let max_start = options.len().saturating_sub(visible_options);
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible_options)
        .min(max_start);
    if start > 0 {
        lines.push(Line::from(Span::styled(
            "↑ more variants",
            Style::default().fg(theme.muted),
        )));
    }
    let already_used = lines.len().saturating_sub(1);
    let take = visible_options.saturating_sub(already_used);
    let first_option_row = lines.len();
    for (visible_offset, (option_index, line)) in options
        .iter()
        .enumerate()
        .skip(start)
        .take(take)
        .enumerate()
    {
        app.hitboxes.push(Hitbox {
            area: Rect {
                x: area.x.saturating_add(1),
                y: area
                    .y
                    .saturating_add(1 + (first_option_row + visible_offset) as u16),
                width: area.width.saturating_sub(2),
                height: 1,
            },
            action: HitAction::DetailAnswerOption {
                index: option_index,
            },
        });
        lines.push(line.clone());
    }
    if start + take < options.len() {
        lines.push(Line::from(Span::styled(
            "↓ more variants",
            Style::default().fg(theme.muted),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if focused {
                        theme.focus
                    } else {
                        theme.warn
                    })),
            )
            .style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );
    if focused && selected == 0 && start == 0 {
        let cursor_x = area.x.saturating_add(3).saturating_add(
            UnicodeWidthStr::width("Custom answer: ") as u16
                + UnicodeWidthStr::width(input_text.as_str()) as u16,
        );
        frame.set_cursor_position((
            cursor_x.min(area.x.saturating_add(area.width.saturating_sub(2))),
            area.y.saturating_add(2),
        ));
    }
}

fn option_line(
    label: &str,
    selected: bool,
    panel_focused: bool,
    hovered: bool,
    theme: &Theme,
) -> Line<'static> {
    let marker = if selected || hovered { "› " } else { "  " };
    let style = if (selected && panel_focused) || hovered {
        Style::default()
            .fg(theme.focus)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(Span::styled(format!("{marker}{label}"), style))
}

fn render_edits_panel(
    frame: &mut Frame<'_>,
    app: &mut App,
    theme: &Theme,
    editable: bool,
    area: Rect,
) {
    let detail = app.detail.as_ref().unwrap();
    let focused = detail.focus == DetailFocus::Edits;
    let title = if !editable {
        " Review edits (read-only outside Review) "
    } else if focused {
        " Review edits [focused] · Ctrl+S save · Ctrl+R re-run · Esc thread "
    } else {
        " Review edits · Tab to edit · Ctrl+S save · Ctrl+R re-run "
    };
    let mut review_edits = detail.review_edits.clone();
    review_edits.set_block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if focused {
                theme.focus
            } else if editable {
                theme.review
            } else {
                theme.muted
            })),
    );
    frame.render_widget(&review_edits, area);
}

/// Bottom action bar: contextual buttons that dispatch the same `UiAction`s
/// as the hotkeys. Registers a hitbox per button.
fn render_action_bar(
    frame: &mut Frame<'_>,
    app: &mut App,
    theme: &Theme,
    task: &Task,
    has_questions: bool,
    area: Rect,
) {
    // An archived task offers only what applies to it: restore or delete.
    let buttons: Vec<(String, UiAction)> = if task.status == TaskStatus::Archive {
        vec![
            ("Restore u".to_string(), UiAction::Restore),
            ("Del d".to_string(), UiAction::DeleteTask),
        ]
    } else {
        let mut buttons: Vec<(String, UiAction)> = vec![("▶ Run r".to_string(), UiAction::Run)];
        if has_questions {
            buttons.push(("Answer w".to_string(), UiAction::AnswerQuestion));
        }
        if task.status == TaskStatus::Review {
            buttons.push(("Approve y".to_string(), UiAction::Approve));
            buttons.push(("Re-run ^R".to_string(), UiAction::Rerun));
        }
        if task.session.is_some() {
            buttons.push(("Attach t".to_string(), UiAction::Attach));
        }
        if app.board.session_states.get(&task.id) == Some(&SessionState::Crashed) {
            buttons.push(("Recover u".to_string(), UiAction::Recover));
        }
        buttons.push(("Edit e".to_string(), UiAction::EditTask));
        buttons.push(("Move m".to_string(), UiAction::MoveTask));
        buttons.push(("+Ctx c".to_string(), UiAction::AddContext));
        if app.ops.task_has_backups(&task.id) {
            buttons.push(("Revert".to_string(), UiAction::Revert));
        }
        buttons.push(("Del d".to_string(), UiAction::DeleteTask));
        buttons
    };

    let mut spans = Vec::new();
    let mut x = area.x;
    for (label, action) in buttons {
        let text = format!("[ {label} ]");
        let width = text.chars().count() as u16;
        if x + width > area.x + area.width {
            break;
        }
        app.hitboxes.push(Hitbox {
            area: Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            action: HitAction::Action(action),
        });
        let style = if app.is_hovered(HitAction::Action(action)) {
            Style::default()
                .fg(theme.focus)
                .bg(theme.border)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg).bg(theme.border)
        };
        spans.push(Span::styled(text, style));
        spans.push(Span::raw(" "));
        x += width + 1;
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn thread_lines(messages: &[Message], theme: &Theme) -> Vec<Line<'static>> {
    if messages.is_empty() {
        return vec![Line::from("No thread messages")];
    }
    let mut lines = Vec::new();
    for message in messages {
        let style = match message.kind {
            MessageKind::Question => Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            MessageKind::Suggestion => Style::default().fg(theme.focus),
            MessageKind::Context => Style::default().fg(theme.muted),
            MessageKind::ReviewEdit => Style::default().fg(theme.review),
            MessageKind::Task => Style::default().fg(theme.ok),
            MessageKind::System => Style::default().fg(theme.muted),
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
        let sanitized_body = sanitize_terminal_text(&message.body);
        if sanitized_body.is_empty() {
            lines.push(Line::from(""));
        } else {
            for body_line in sanitized_body.lines() {
                lines.push(Line::from(body_line.to_string()));
            }
        }
        if !message.variants.is_empty() {
            lines.push(Line::from(format!(
                "Variants: {}",
                truncate_display(&sanitize_terminal_text(&message.variants.join(" | ")), 120)
            )));
        }
        if let Some(answer) = &message.answer {
            lines.push(Line::from(Span::styled(
                format!("Answer: {}", sanitize_terminal_text(answer)),
                Style::default().fg(theme.ok),
            )));
        }
        lines.push(Line::from(""));
    }
    lines
}
