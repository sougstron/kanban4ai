use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::limit_share::LimitShare;
use crate::core::limits::format_span;
use crate::core::models::{
    IntegrationState, Message, MessageKind, MessageStatus, RunPhase, Task, TaskStatus,
};
use crate::core::session::SessionState;
use crate::core::stats::TaskAnalytics;
use crate::core::task_diff::TaskDiff;
use crate::core::telemetry::SessionProgress;
use crate::core::timefmt;
use crate::core::token_costs::TokenCosts;

use super::app::{App, DetailFocus, DiffView, HitAction, Hitbox, TextRegion, UiAction};
use super::board;
use super::card;
use super::card::{format_tokens, sanitize_terminal_text, truncate_display};
use super::dialogs::set_cursor_visible;
use super::projects::shorten_path;
use super::theme::Theme;
use super::thread_view::{pin_last_message_scroll, visible_thread_messages};

/// The meta block's own title in both header states; the arrow and `z`
/// advertise the toggle. Both are the same width, which the project badge
/// has to clear on the same border row.
const META_TITLE_EXPANDED: &str = " Task ▾ z ";
const META_TITLE_COLLAPSED: &str = " Task ▸ z ";
const META_TITLE_WIDTH: u16 = 10;
/// Below this many rows the header stays collapsed even when the user
/// expanded it: the thread would otherwise shrink to its five-row floor.
const EXPANDED_HEADER_MIN_HEIGHT: u16 = 24;
/// Prefix of the custom-answer option row; the answer text starts right
/// after the `› ` marker and this label, and `render_answer_panel` plus the
/// mouse caret mapping both measure it.
pub(super) const ANSWER_LABEL: &str = "Custom answer: ";
/// The Analytics panel is one bordered row, plus a second for the task's
/// limit shares once it has any.
fn analytics_height(analytics: &TaskAnalytics) -> u16 {
    if analytics.limit_shares.is_empty() {
        3
    } else {
        4
    }
}

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
    let remote_label = app.board.remote_runs.get(&task.id).map(|runs| {
        runs.iter()
            .map(|run| format!("{} · {}", run.label(), run.phase))
            .collect::<Vec<_>>()
            .join(", ")
    });
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
    let header_expanded =
        app.settings.detail_header_expanded && area.height >= EXPANDED_HEADER_MIN_HEIGHT;
    let analytics_height = if header_expanded {
        analytics_height(&detail.analytics)
    } else {
        0
    };
    let question_index = detail.question_index;
    let count = app
        .sync_board
        .tasks
        .iter()
        .filter(|id| *id == &task.id)
        .count();
    let conflict_height = if count == 0 {
        0
    } else {
        4.min(area.height.saturating_sub(12))
    };
    let conflict_area = Rect::new(area.x, area.y, area.width, conflict_height);
    super::sync_board::render_conflicts(frame, app, &task.id, conflict_area);
    let area = Rect::new(
        area.x,
        area.y.saturating_add(conflict_height),
        area.width,
        area.height.saturating_sub(conflict_height),
    );

    let description_lines = task_description_lines(&task);
    let isolated = task.worktree.is_some();
    let conflicted = task.integration == IntegrationState::Conflict;
    let meta_extra_lines = u16::from(task.permission().is_some_and(|p| p != "full"))
        + u16::from(waiting)
        + u16::from(wait_deadline.is_some())
        + u16::from(task.restart_at.is_some() || session_state == Some(SessionState::Crashed))
        + u16::from(task.launch_at.is_some())
        + u16::from(isolated)
        + u16::from(task.result_path.is_some())
        + u16::from(conflicted)
        + description_lines;
    let alarm_lines = alarm_lines(
        &task,
        waiting,
        wait_deadline.is_some(),
        session_state == Some(SessionState::Crashed),
    );
    let meta_height = if header_expanded {
        6 + meta_extra_lines
    } else {
        4 + alarm_lines
    };
    let answer_height = if show_answer {
        let question = &open_questions[question_index.min(open_questions.len() - 1)];
        // Two more rows when several questions are open: the prev/next
        // question button row pinned under the variants.
        let switch_rows = if open_questions.len() > 1 { 2 } else { 0 };
        let desired = 4 + question.variants.len() as u16 + 1 + switch_rows;
        let reserved = meta_height
            .saturating_add(analytics_height)
            .saturating_add(if show_edits { 6 } else { 0 })
            .saturating_add(1)
            .saturating_add(3);
        desired.min(area.height.saturating_sub(reserved).max(5))
    } else {
        0
    };
    let mut constraints = vec![
        Constraint::Length(meta_height),
        Constraint::Length(analytics_height),
        Constraint::Min(5),
        Constraint::Length(answer_height),
        Constraint::Length(if show_edits { 6 } else { 0 }),
        Constraint::Length(1),
    ];
    if !show_answer {
        constraints[3] = Constraint::Length(0);
    }
    let mut chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec();
    // The parent's subagents sit between the analytics and the thread; the
    // thread keeps at least five rows.
    let subagents = app
        .board
        .subagents
        .get(&task.id)
        .cloned()
        .unwrap_or_default();
    let sub_height = if subagents.is_empty() {
        0
    } else {
        (subagents.len() as u16 + 2)
            .min(8)
            .min(chunks[2].height.saturating_sub(5))
    };
    let sub_area = Rect::new(chunks[2].x, chunks[2].y, chunks[2].width, sub_height);
    chunks[2] = Rect::new(
        chunks[2].x,
        chunks[2].y.saturating_add(sub_height),
        chunks[2].width,
        chunks[2].height.saturating_sub(sub_height),
    );

    let meta_state = MetaState {
        waiting,
        wait_deadline,
        wait_note: wait_note.as_deref(),
        remote_label: remote_label.as_deref(),
        session_state,
        worktree_path: task
            .worktree
            .as_deref()
            .map(|rel| shorten_path(&app.ops.storage.worktrees_dir.join(rel))),
        result_path: app.ops.result_path(&task).map(|path| shorten_path(&path)),
    };
    let live_progress = app.session_progress.get(&task.id);
    if header_expanded {
        render_meta(frame, &theme, &task, &meta_state, chunks[0]);
    } else {
        let detail = app.detail.as_ref().unwrap();
        let totals = AnalyticsTotals::new(
            &task,
            &detail.analytics,
            live_progress,
            session_state,
            &app.token_costs,
        );
        render_meta_compact(
            frame,
            &theme,
            &task,
            &meta_state,
            &totals,
            detail.diff.diff.as_ref(),
            chunks[0],
        );
    }
    if let Some(hitbox) = board::render_project_badge(frame, app, chunks[0], META_TITLE_WIDTH) {
        app.hitboxes.push(hitbox);
    }
    // The badge registered first, so a click on it still opens projects;
    // anywhere else on the header toggles it.
    app.hitboxes.push(Hitbox {
        area: chunks[0],
        action: HitAction::Action(UiAction::ToggleDetailHeader),
    });
    if let Some(detail) = app.detail.as_mut() {
        detail.subagent_count = subagents.len();
        if detail.subagent_count == 0 && detail.focus == DetailFocus::Subagents {
            detail.focus = DetailFocus::Thread;
        }
    }
    if sub_height > 0 {
        let detail = app.detail.as_ref().unwrap();
        render_subagents(
            frame,
            &theme,
            &subagents,
            &detail.analytics,
            detail.subagent_selected,
            focus == DetailFocus::Subagents,
            sub_area,
        );
    }
    if header_expanded {
        render_analytics(
            frame,
            &theme,
            &task,
            &app.detail.as_ref().unwrap().analytics,
            live_progress,
            session_state,
            &app.token_costs,
            chunks[1],
        );
    }

    let tab_title = panel_tab_title(&theme, &app.detail.as_ref().unwrap().diff);
    let diff_open = app.detail.as_ref().unwrap().diff.open;
    push_tab_hitboxes(app, &tab_title, chunks[2]);
    if diff_open {
        render_diff_panel(frame, app, &theme, tab_title.line, focus, chunks[2]);
    } else {
        render_thread_panel(frame, app, &theme, tab_title.line, focus, chunks[2]);
    }

    if show_answer {
        render_answer_panel(frame, app, &theme, &open_questions, chunks[3]);
    }

    if show_edits {
        render_edits_panel(frame, app, &theme, edits_editable, chunks[4]);
    }

    render_action_bar(frame, app, &theme, &task, show_answer, chunks[5]);
}

fn render_thread_panel(
    frame: &mut Frame<'_>,
    app: &mut App,
    theme: &Theme,
    title: Line<'static>,
    focus: DetailFocus,
    area: Rect,
) {
    let theme = *theme;

    // Thread panel with a clamped scroll and a scrollbar. Input-provenance
    // (what the agent actually consumed) is telemetry, not conversation, so it
    // is kept out of the thread entirely and shown only in the `v` popup.
    let inner_width = area.width.saturating_sub(2);
    let hide_kanban = app.settings.hide_kanban_messages;
    let mut links = Vec::new();
    let mut thread_region = TextRegion::default();
    let (panel_lines, last_start) = {
        let detail_ref = app.detail.as_ref().unwrap();
        let visible = visible_thread_messages(&detail_ref.messages, hide_kanban);
        let selected_id = detail_ref
            .thread_selected
            .and_then(|index| detail_ref.messages.get(index))
            .map(|message| message.id.clone());
        let panel_lines = thread_lines(
            &visible,
            selected_id.as_deref(),
            &theme,
            inner_width,
            &mut links,
            &mut thread_region,
        );
        let last_start = if visible.len() <= 1 {
            0
        } else {
            wrapped_row_count(
                &thread_lines(
                    &visible[..visible.len() - 1],
                    None,
                    &theme,
                    inner_width,
                    &mut Vec::new(),
                    &mut TextRegion::default(),
                ),
                inner_width,
            )
        };
        (panel_lines, last_start)
    };
    let thread = Paragraph::new(panel_lines)
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .wrap(Wrap { trim: false });
    // Measure the rows the wrap actually produces at this width: a logical line
    // spans several rows in a narrow terminal, and counting logical lines would
    // stop the scroll short of the thread's last row. The block is attached
    // after measuring so `line_count` sees the inner width only.
    let content_height = u16::try_from(thread.line_count(inner_width)).unwrap_or(u16::MAX);
    let visible_height = area.height.saturating_sub(2);
    let max_scroll = content_height.saturating_sub(visible_height);
    let scroll = {
        let detail = app.detail.as_mut().unwrap();
        detail.max_scroll = max_scroll;
        if detail.pin_last_message {
            detail.scroll = pin_last_message_scroll(last_start, content_height, visible_height);
            detail.pin_last_message = false;
        } else {
            detail.scroll = detail.scroll.min(max_scroll);
        }
        detail.scroll
    };
    app.hitboxes.push(Hitbox {
        area,
        action: HitAction::DetailThread,
    });
    thread_region.area = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        inner_width,
        visible_height,
    );
    thread_region.rows = thread_region
        .rows
        .into_iter()
        .skip(usize::from(scroll))
        .take(usize::from(visible_height))
        .collect();
    app.text_regions.push(thread_region);
    frame.render_widget(
        thread
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if focus == DetailFocus::Thread {
                        theme.focus
                    } else {
                        theme.border
                    })),
            )
            .scroll((scroll, 0)),
        area,
    );
    for link in links {
        if link.row < usize::from(scroll)
            || link.row >= usize::from(scroll) + usize::from(visible_height)
            || link.column >= usize::from(inner_width)
        {
            continue;
        }
        let area = Rect::new(
            area.x + 1 + link.column as u16,
            area.y + 1 + (link.row - usize::from(scroll)) as u16,
            link.width.min(usize::from(inner_width) - link.column) as u16,
            1,
        );
        // Hover is evaluated after all fragments have registered.
        app.thread_links.push((area, link.url));
    }
    if app.modal.is_none()
        && let Some((x, y)) = app.mouse_position
        && let Some((_, target)) = app
            .thread_links
            .iter()
            .find(|(area, _)| area.contains((x, y).into()))
    {
        for (area, url) in &app.thread_links {
            if url == target {
                for x in area.x..area.right() {
                    frame.buffer_mut()[(x, area.y)]
                        .modifier
                        .insert(Modifier::UNDERLINED);
                }
            }
        }
    }
    if max_scroll > 0 {
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll as usize).position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
}

/// Rows of the warnings the collapsed header never hides: blocked on a
/// question, a declared wait, a crash (or its retry) and an integration
/// conflict. Mirrors the conditions in [`alarm_spans`].
fn alarm_lines(task: &Task, waiting: bool, wait_deadline: bool, crashed: bool) -> u16 {
    u16::from(waiting)
        + u16::from(wait_deadline)
        + u16::from(task.status == TaskStatus::InProgress && (task.restart_at.is_some() || crashed))
        + u16::from(task.integration == IntegrationState::Conflict)
}

/// The alarm rows themselves, shared by both header states.
fn alarm_spans(theme: &Theme, task: &Task, meta_state: &MetaState<'_>) -> Vec<Line<'static>> {
    let warn = Style::default().fg(theme.warn).add_modifier(Modifier::BOLD);
    let err = Style::default().fg(theme.err).add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    if meta_state.waiting {
        lines.push(Line::from(Span::styled(
            "⏳ Agent is blocked on a question and waits for your answer",
            warn,
        )));
    }
    if let Some(deadline) = meta_state.wait_deadline {
        let note = meta_state
            .wait_note
            .map(sanitize_terminal_text)
            .filter(|note| !note.is_empty())
            .unwrap_or_else(|| "declared wait".to_string());
        lines.push(Line::from(Span::styled(
            format!("⏳ Waiting until {} — {note}", deadline.format("%H:%M")),
            warn,
        )));
    }
    if task.status == TaskStatus::InProgress {
        if let Some(restart_at) = task.restart_at {
            lines.push(Line::from(Span::styled(
                format!(
                    "↻ Crash retry at {} — waiting for the backoff before the queue picks it up",
                    restart_at.format("%H:%M")
                ),
                warn,
            )));
        } else if meta_state.session_state == Some(SessionState::Crashed) {
            lines.push(Line::from(Span::styled(
                "✖ Session is stuck or crashed — press u / Recover to return it to To Do",
                err,
            )));
        }
    }
    if task.integration == IntegrationState::Conflict {
        lines.push(Line::from(Span::styled(
            "⚠ Integration conflict — resolve in the worktree, then Re-run (Ctrl+R)",
            err,
        )));
    }
    lines
}

/// Run time and token totals of every agent run, the live session included:
/// what both the Analytics panel and the collapsed header show.
struct AnalyticsTotals {
    running_seconds: i64,
    runs: usize,
    tokens: crate::core::telemetry::TokenBreakdown,
    /// Tokens weighted by the task's backend/model prices (plus unsplit ones).
    equivalent: f64,
    empty: bool,
}

impl AnalyticsTotals {
    fn new(
        task: &Task,
        analytics: &TaskAnalytics,
        live: Option<&SessionProgress>,
        session_state: Option<SessionState>,
        costs: &TokenCosts,
    ) -> Self {
        let mut running_seconds = analytics.running_seconds;
        if session_state == Some(SessionState::Live)
            && let Some(since) = analytics.running_since
        {
            running_seconds += (timefmt::now() - since).num_seconds().max(0);
        }
        let tokens = analytics.tokens_with_live(
            task.session.as_deref(),
            live.and_then(|live| live.breakdown.as_ref()),
        );
        // The task's current backend/model stands in for every run. A task
        // that switched models mid-way is priced as its latest one.
        let weights = costs.weights_for(task.agent_backend.as_deref(), task.ai_model.as_deref());
        let mut equivalent = weights.equivalent(&tokens);
        if analytics.unsplit_tokens > 0 {
            equivalent += analytics.unsplit_tokens as f64;
        }
        let empty = analytics.runs == 0 && tokens.is_empty() && analytics.unsplit_tokens == 0;
        Self {
            running_seconds,
            runs: analytics.runs,
            tokens,
            equivalent,
            empty,
        }
    }
}

/// The collapsed Task header: two rows plus any alarm. Row one names the
/// task and its state, row two folds the Analytics panel and the diff size
/// into one line; `z` or a click shows everything.
#[allow(clippy::too_many_arguments)]
fn render_meta_compact(
    frame: &mut Frame<'_>,
    theme: &Theme,
    task: &Task,
    meta_state: &MetaState<'_>,
    totals: &AnalyticsTotals,
    diff: Option<&TaskDiff>,
    area: Rect,
) {
    let muted = Style::default().fg(theme.muted);
    let value = Style::default().add_modifier(Modifier::BOLD);
    let sep = || Span::styled(" │ ", Style::default().fg(theme.border));
    let mut first = vec![
        Span::styled(
            task.id.clone(),
            Style::default()
                .fg(theme.focus)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::raw(sanitize_terminal_text(&task.title)),
        sep(),
        Span::styled(
            format!(
                "{}{}",
                task.status,
                task.run_phase
                    .map(|phase| format!(" · {phase}"))
                    .unwrap_or_default()
            ),
            value,
        ),
    ];
    let mut badges = Vec::new();
    if let Some(permission) = task.permission().filter(|p| *p != "full") {
        badges.push(format!("🔒 {}", sanitize_terminal_text(permission)));
    }
    if task.silence {
        badges.push("🔇".to_string());
    }
    if let Some(launch_at) = task.launch_at {
        badges.push(format!("🕐 {}", launch_at.format("%H:%M")));
    }
    if task.integration != IntegrationState::None {
        badges.push(format!("⇄ {}", task.integration));
    }
    if !badges.is_empty() {
        first.push(sep());
        first.push(Span::styled(badges.join(" "), muted));
    }
    if meta_state.result_path.is_some() {
        first.push(sep());
        first.push(Span::styled(
            "Result · o",
            Style::default()
                .fg(theme.review)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let mut second = Vec::new();
    if totals.empty {
        second.push(Span::styled("No agent runs yet", muted));
    } else {
        second.push(Span::styled("▶ ", muted));
        second.push(Span::styled(format_span(totals.running_seconds), value));
        second.push(Span::styled(
            format!(
                " · {} run{}",
                totals.runs,
                if totals.runs == 1 { "" } else { "s" }
            ),
            muted,
        ));
        second.push(sep());
        second.push(Span::styled(
            format_tokens(totals.tokens.prompt_total() + totals.tokens.output),
            value,
        ));
        second.push(Span::styled(" tok", muted));
        if let Some(rate) = totals.tokens.cache_hit_rate() {
            second.push(Span::styled(format!(" · {rate:.0}% cache"), muted));
        }
        second.push(Span::styled(
            format!(" · ≈{} eq", format_tokens(totals.equivalent.round() as i64)),
            muted,
        ));
    }
    if let Some(diff) = diff.filter(|diff| !diff.files.is_empty()) {
        second.push(sep());
        second.push(Span::styled("Δ ", muted));
        second.push(Span::styled(
            format!("+{}", diff.added()),
            Style::default().fg(theme.ok),
        ));
        second.push(Span::styled(
            format!(" −{}", diff.removed()),
            Style::default().fg(theme.err),
        ));
        let count = diff.files.len();
        second.push(Span::styled(
            format!(" · {count} file{} · g", if count == 1 { "" } else { "s" }),
            muted,
        ));
    }
    second.push(sep());
    second.push(Span::styled(
        sanitize_terminal_text(&match (&task.agent_backend, &task.ai_model) {
            (Some(backend), Some(model)) => format!("{backend}/{model}"),
            (Some(backend), None) => backend.clone(),
            (None, Some(model)) => model.clone(),
            (None, None) => "default".to_string(),
        }),
        muted,
    ));
    if let Some(label) = meta_state.remote_label {
        second.push(sep());
        second.push(Span::styled(sanitize_terminal_text(label), muted));
    }

    let mut lines = vec![Line::from(first), Line::from(second)];
    lines.extend(alarm_spans(theme, task, meta_state));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(META_TITLE_COLLAPSED)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.focus)),
            )
            .style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );
}

/// The lower panel's tab strip, drawn as its border title:
/// ` Thread │ Diff +12 −3 (2) · g `, the active tab bold. `diff_start` and
/// `diff_end` are the Diff tab's columns within the title, for its hitbox.
struct TabTitle {
    line: Line<'static>,
    thread_width: u16,
    diff_width: u16,
}

fn panel_tab_title(theme: &Theme, view: &DiffView) -> TabTitle {
    let active = Style::default()
        .fg(theme.focus)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let idle = Style::default().fg(theme.muted);
    let (thread_style, diff_style) = if view.open {
        (idle, active)
    } else {
        (active, idle)
    };
    let thread = " Thread ";
    let mut spans = vec![
        Span::styled(thread, thread_style),
        Span::styled("│", Style::default().fg(theme.border)),
        Span::styled(" Diff", diff_style),
    ];
    let mut diff_text = String::from(" Diff");
    if let Some(diff) = view.diff.as_ref().filter(|diff| !diff.files.is_empty()) {
        let plus = format!(" +{}", diff.added());
        let minus = format!(" −{}", diff.removed());
        let count = format!(" ({})", diff.files.len());
        spans.push(Span::styled(plus.clone(), Style::default().fg(theme.ok)));
        spans.push(Span::styled(minus.clone(), Style::default().fg(theme.err)));
        spans.push(Span::styled(count.clone(), idle));
        diff_text.push_str(&plus);
        diff_text.push_str(&minus);
        diff_text.push_str(&count);
    }
    let hint = if view.open {
        " · ↑↓ Enter · g "
    } else {
        " · g "
    };
    spans.push(Span::styled(hint, idle));
    diff_text.push_str(hint);
    TabTitle {
        line: Line::from(spans),
        thread_width: thread.width() as u16,
        diff_width: diff_text.width() as u16,
    }
}

/// Clicks on the inactive tab label switch tabs. Registered before the
/// panel's own hitbox so they win.
fn push_tab_hitboxes(app: &mut App, title: &TabTitle, area: Rect) {
    if area.height == 0 || area.width < 3 {
        return;
    }
    let right = area.right().saturating_sub(1);
    let thread = Rect::new(
        area.x + 1,
        area.y,
        title.thread_width.min(right.saturating_sub(area.x + 1)),
        1,
    );
    let diff_x = (thread.right() + 1).min(right);
    let diff = Rect::new(
        diff_x,
        area.y,
        title.diff_width.min(right.saturating_sub(diff_x)),
        1,
    );
    for rect in [thread, diff] {
        app.hitboxes.push(Hitbox {
            area: rect,
            action: HitAction::Action(UiAction::ToggleDiffTab),
        });
    }
}

/// The Diff tab: one row per changed file, hunks of the expanded ones
/// inline. Rows never wrap, so a row is a line and the selected file's row
/// is known for scrolling and clicks.
fn render_diff_panel(
    frame: &mut Frame<'_>,
    app: &mut App,
    theme: &Theme,
    title: Line<'static>,
    focus: DetailFocus,
    area: Rect,
) {
    let inner_width = usize::from(area.width.saturating_sub(2));
    let visible_height = area.height.saturating_sub(2);
    let muted = Style::default().fg(theme.muted);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut file_rows: Vec<(usize, usize)> = Vec::new();
    let view = &app.detail.as_ref().unwrap().diff;
    match (&view.diff, view.loaded) {
        (_, false) => lines.push(Line::from(Span::styled("Computing the diff…", muted))),
        (None, true) => lines.push(Line::from(Span::styled(
            "No diff: the task has no worktree, branch, landed commit or backups yet",
            muted,
        ))),
        (Some(diff), true) => {
            let mut header = format!("{} · {}", diff.source.label(), diff.summary());
            if diff.truncated {
                header.push_str(" · long patch truncated");
            }
            lines.push(Line::from(Span::styled(
                truncate_display(&header, inner_width),
                muted,
            )));
            if diff.files.is_empty() {
                lines.push(Line::from(Span::styled("No changes", muted)));
            }
            let selected = view.selected.min(diff.files.len().saturating_sub(1));
            for (index, file) in diff.files.iter().enumerate() {
                let expanded = view.expanded.contains(&file.path);
                file_rows.push((index, lines.len()));
                let counts = if file.binary {
                    " binary".to_string()
                } else {
                    format!(" +{} −{}", file.added, file.removed)
                };
                let marker = if expanded { "▾ " } else { "▸ " };
                let path_width = inner_width.saturating_sub(marker.width() + counts.width());
                let path = truncate_display(&sanitize_terminal_text(&file.path), path_width);
                let pad = " ".repeat(path_width.saturating_sub(path.width()));
                let mut row_style = Style::default().fg(theme.fg);
                if index == selected && focus == DetailFocus::Thread {
                    row_style = row_style.bg(theme.border).add_modifier(Modifier::BOLD);
                }
                lines.push(
                    Line::from(vec![
                        Span::styled(marker, muted),
                        Span::raw(path),
                        Span::raw(pad),
                        Span::styled(
                            counts,
                            Style::default().fg(if file.binary {
                                theme.muted
                            } else if file.removed > file.added {
                                theme.err
                            } else {
                                theme.ok
                            }),
                        ),
                    ])
                    .style(row_style),
                );
                if expanded {
                    for text in &file.lines {
                        let style = if text.starts_with("@@") {
                            Style::default().fg(theme.focus)
                        } else if text.starts_with('+') {
                            Style::default().fg(theme.ok)
                        } else if text.starts_with('-') {
                            Style::default().fg(theme.err)
                        } else {
                            Style::default().fg(theme.fg)
                        };
                        lines.push(Line::from(Span::styled(
                            truncate_display(
                                &sanitize_terminal_text(&text.replace('\t', "    ")),
                                inner_width,
                            ),
                            style,
                        )));
                    }
                }
            }
        }
    }
    let content_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let max_scroll = content_height.saturating_sub(visible_height);
    let scroll = {
        let view = &mut app.detail.as_mut().unwrap().diff;
        view.max_scroll = max_scroll;
        if view.follow_selected {
            view.follow_selected = false;
            if let Some(&(_, row)) = file_rows.iter().find(|(index, _)| *index == view.selected) {
                let row = u16::try_from(row).unwrap_or(u16::MAX);
                if row < view.scroll {
                    view.scroll = row;
                } else if row >= view.scroll.saturating_add(visible_height) {
                    view.scroll = row.saturating_sub(visible_height.saturating_sub(1));
                }
            }
        }
        view.scroll = view.scroll.min(max_scroll);
        view.scroll
    };
    for (index, row) in file_rows {
        let Some(offset) = row.checked_sub(usize::from(scroll)) else {
            continue;
        };
        if offset >= usize::from(visible_height) {
            continue;
        }
        app.hitboxes.push(Hitbox {
            area: Rect::new(
                area.x + 1,
                area.y + 1 + offset as u16,
                inner_width as u16,
                1,
            ),
            action: HitAction::Action(UiAction::ToggleDiffFile(index)),
        });
    }
    app.hitboxes.push(Hitbox {
        area,
        action: HitAction::DetailThread,
    });
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme.bg).fg(theme.fg))
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if focus == DetailFocus::Thread {
                        theme.focus
                    } else {
                        theme.border
                    })),
            )
            .scroll((scroll, 0)),
        area,
    );
    if max_scroll > 0 {
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll as usize).position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
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
    worktree_path: Option<String>,
    result_path: Option<String>,
    remote_label: Option<&'a str>,
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
            "Status: {}{} │ Session: {} │ Interactive: {}",
            task.status,
            task.run_phase
                .map(|phase| format!(" · {phase}"))
                .unwrap_or_default(),
            sanitize_terminal_text(
                meta_state
                    .remote_label
                    .unwrap_or_else(|| task.session.as_deref().unwrap_or("-"))
            ),
            task.interactive
        )),
        Line::from(format!(
            "Backend: {} │ Model: {} │ Effort: {} │ Agent: {} │ Chain: {}",
            sanitize_terminal_text(task.agent_backend.as_deref().unwrap_or("default")),
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
    if let Some(permission) = task.permission().filter(|p| *p != "full") {
        meta.push(Line::from(format!(
            "🔒 Permission: {}",
            sanitize_terminal_text(permission)
        )));
    }
    if task.silence {
        meta.push(Line::from(
            "🔇 Silence: questions refused — decide and record disputes as suggestions",
        ));
    }
    if let Some(launch_at) = task.launch_at {
        meta.push(Line::from(format!(
            "🕐 Planned launch at {}",
            timefmt::format(&launch_at)
        )));
    }
    // Graph state costs a row, so it only appears on tasks that have any:
    // an ordinary task's detail keeps the height it always had.
    if !task.depends_on.is_empty()
        || task.needs.is_some()
        || task.role_profile.is_some()
        || task.parent_task.is_some()
        || task.use_orchestrator
    {
        let mut line = format!(
            "Depends: {}",
            if task.depends_on.is_empty() {
                "-".to_string()
            } else {
                sanitize_terminal_text(&task.depends_on.join(", "))
            }
        );
        if let Some(profile) = &task.role_profile {
            line.push_str(&format!(
                " │ Role: {} #{}",
                sanitize_terminal_text(profile),
                task.roster_index + 1
            ));
        }
        if let Some(parent) = &task.parent_task {
            line.push_str(&format!(
                " │ {}: {}",
                if task.spawned {
                    "Spawned by"
                } else {
                    "Planned by"
                },
                sanitize_terminal_text(parent)
            ));
        }
        if task.use_orchestrator {
            line.push_str(if task.orchestrated {
                " │ Orchestrator: planned"
            } else {
                " │ Orchestrator: on"
            });
        }
        meta.push(Line::from(line));
        if let Some(needs) = task
            .needs
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            meta.push(Line::from(format!(
                "Needs: {}",
                sanitize_terminal_text(needs)
            )));
        }
    }
    if let Some(worktree) = &meta_state.worktree_path {
        let mut line = format!(
            "Worktree: {} │ Branch: {} │ Base: {}",
            sanitize_terminal_text(worktree),
            sanitize_terminal_text(task.branch.as_deref().unwrap_or("-")),
            task.base_commit
                .as_deref()
                .map(|commit| commit.chars().take(7).collect::<String>())
                .unwrap_or_else(|| "-".to_string())
        );
        if task.integration != IntegrationState::None {
            line.push_str(&format!(" │ Integration: {}", task.integration));
        }
        meta.push(Line::from(line));
    }
    if let Some(result) = &meta_state.result_path {
        meta.push(Line::from(vec![
            Span::styled(
                "Result: ",
                Style::default()
                    .fg(theme.review)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(sanitize_terminal_text(result)),
            Span::styled(" · o opens it", Style::default().fg(theme.muted)),
        ]));
    }
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
    meta.extend(alarm_spans(theme, task, meta_state));
    frame.render_widget(
        Paragraph::new(meta)
            .block(
                Block::default()
                    .title(META_TITLE_EXPANDED)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.focus)),
            )
            .style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );
}

/// One-row summary of everything the task's agent runs cost so far — run
/// time and tokens, cumulative over the first run, review passes, reruns and
/// crash restarts. Purely programmatic: closed sessions come from the stats
/// events file ([`TaskAnalytics`]), the live one from transcript telemetry.
/// The parent's "Сабагенты" section: one row per subagent with its state,
/// backend/model, run time, tokens and one line of summary. The selected row
/// is highlighted while the section has focus.
fn render_subagents(
    frame: &mut Frame<'_>,
    theme: &Theme,
    subagents: &[Task],
    analytics: &TaskAnalytics,
    selected: usize,
    focused: bool,
    area: Rect,
) {
    use crate::core::models::SubState;
    use crate::core::subagents::{short_elapsed, state_label, subagent_elapsed, summary_line};
    let now = timefmt::now();
    let selected = selected.min(subagents.len().saturating_sub(1));
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let offset = selected.saturating_sub(visible - 1);
    let width = usize::from(area.width.saturating_sub(2));
    let lines: Vec<Line> = subagents
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, sub)| {
            let (glyph, color) = match sub.sub_state {
                Some(SubState::Done) => ("✓", theme.ok),
                Some(SubState::Failed | SubState::Cancelled) => ("✗", theme.err),
                Some(SubState::Running) => ("▶", theme.focus),
                _ => ("…", theme.muted),
            };
            let agent = match (&sub.agent_backend, &sub.ai_model) {
                (Some(backend), Some(model)) => format!("{backend}/{model}"),
                (Some(backend), None) => backend.clone(),
                (None, _) => sub.role_profile.clone().unwrap_or_else(|| "-".into()),
            };
            let tokens = analytics
                .subagent_tokens
                .get(&sub.id)
                .map_or_else(|| "-".to_string(), |t| card::format_tokens(*t));
            let summary = summary_line(sub, 200);
            let text = format!(
                " {} {:<15} {} · {} · {} · {}{}",
                sub.id,
                state_label(sub),
                sanitize_terminal_text(&agent),
                short_elapsed(subagent_elapsed(sub, now)),
                tokens,
                sanitize_terminal_text(sub.title.trim()),
                if summary.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", sanitize_terminal_text(&summary))
                }
            );
            let mut style = Style::default().fg(theme.fg);
            if focused && index == selected {
                style = style.bg(theme.border).add_modifier(Modifier::BOLD);
            }
            Line::from(vec![
                Span::styled(glyph.to_string(), Style::default().fg(color)),
                Span::styled(truncate_display(&text, width.saturating_sub(1)), style),
            ])
        })
        .collect();
    let title = if focused {
        " Сабагенты · Enter thread · x cancel · m merge · d discard "
    } else {
        " Сабагенты · Tab "
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme.bg).fg(theme.fg))
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if focused {
                        theme.focus
                    } else {
                        theme.border
                    })),
            ),
        area,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_analytics(
    frame: &mut Frame<'_>,
    theme: &Theme,
    task: &Task,
    analytics: &TaskAnalytics,
    live: Option<&SessionProgress>,
    session_state: Option<SessionState>,
    costs: &TokenCosts,
    area: Rect,
) {
    let AnalyticsTotals {
        running_seconds,
        tokens,
        equivalent,
        empty,
        ..
    } = AnalyticsTotals::new(task, analytics, live, session_state, costs);

    let muted = Style::default().fg(theme.muted);
    let value = Style::default().add_modifier(Modifier::BOLD);
    let line = if empty {
        Line::from(Span::styled("No agent runs recorded yet", muted))
    } else {
        let mut spans = vec![
            Span::styled("Time ", muted),
            Span::styled(format_span(running_seconds), value),
            Span::styled(
                format!(
                    " · {} run{}",
                    analytics.runs,
                    if analytics.runs == 1 { "" } else { "s" }
                ),
                muted,
            ),
        ];
        if analytics.waiting_seconds > 0 {
            spans.push(Span::styled(
                format!(" · waited {}", format_span(analytics.waiting_seconds)),
                muted,
            ));
        }
        spans.push(Span::styled(" │ ", Style::default().fg(theme.border)));
        spans.push(Span::styled("Input ", muted));
        spans.push(Span::styled(format_tokens(tokens.input), value));
        spans.push(Span::styled(" · Cache R ", muted));
        spans.push(Span::styled(format_tokens(tokens.cache_read), value));
        spans.push(Span::styled(" · Cache W ", muted));
        spans.push(Span::styled(format_tokens(tokens.cache_write), value));
        spans.push(Span::styled(" · Output ", muted));
        spans.push(Span::styled(format_tokens(tokens.output), value));
        spans.push(Span::styled(" · Cache hit ", muted));
        spans.push(Span::styled(
            tokens
                .cache_hit_rate()
                .map(|rate| format!("{rate:.1}%"))
                .unwrap_or_else(|| "-".to_string()),
            value,
        ));
        spans.push(Span::styled(
            format!(" · ≈{} eq", format_tokens(equivalent.round() as i64)),
            value,
        ));
        if analytics.unsplit_tokens > 0 {
            spans.push(Span::styled(
                format!(" · +{} mixed tok", format_tokens(analytics.unsplit_tokens)),
                muted,
            ));
        }
        let subagents = analytics.subagents;
        if subagents.count > 0 {
            spans.push(Span::styled(
                format!(
                    " · subagents {}: {} · {} tok",
                    subagents.count,
                    format_span(subagents.running_seconds),
                    format_tokens(subagents.tokens)
                ),
                muted,
            ));
        }
        Line::from(spans)
    };
    let mut lines = vec![line];
    if !analytics.limit_shares.is_empty() {
        lines.push(limit_shares_line(theme, &analytics.limit_shares));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Analytics ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            )
            .style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );
}

/// `Limits claude 5h ✎0.4% ▶2.1% ⚖0.3% · 7d …`: the share of each provider
/// window the task consumed, one value per role in the role colors the cards
/// use (`card::phase_color`). `shares` arrive sorted by provider, window and
/// role.
fn limit_shares_line(theme: &Theme, shares: &[LimitShare]) -> Line<'static> {
    let muted = Style::default().fg(theme.muted);
    let mut spans = vec![Span::styled("Limits", muted)];
    let mut previous: Option<(&str, &str)> = None;
    for share in shares {
        match previous {
            Some((provider, label)) if provider == share.provider && label == share.label => {}
            Some((provider, _)) if provider == share.provider => {
                spans.push(Span::styled(format!(" · {}", share.label), muted));
            }
            prev => {
                if prev.is_some() {
                    spans.push(Span::styled(" │", Style::default().fg(theme.border)));
                }
                spans.push(Span::styled(
                    format!(" {} {}", share.provider, share.label),
                    muted,
                ));
            }
        }
        previous = Some((&share.provider, &share.label));
        let (glyph, phase) = match share.role.as_str() {
            "designer" => ("✎", Some(RunPhase::Design)),
            "reviewer" => ("⚖", Some(RunPhase::Review)),
            _ => ("▶", None),
        };
        let percent = if share.percent < 0.05 {
            "<0.1%".to_string()
        } else {
            format!("{:.1}%", share.percent)
        };
        spans.push(Span::styled(
            format!(" {glyph}{percent}"),
            Style::default()
                .fg(card::phase_color(theme, phase))
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn render_answer_panel(
    frame: &mut Frame<'_>,
    app: &mut App,
    theme: &Theme,
    open_questions: &[Message],
    area: Rect,
) {
    let (focused, index, input_text, cursor_col, selected, question_body, variants) = {
        let detail = app.detail.as_ref().unwrap();
        let index = detail.question_index.min(open_questions.len() - 1);
        let question = &open_questions[index];
        let lines = detail.answer_input.lines();
        let cursor = detail.answer_input.cursor();
        let (row, col) = (cursor.0, cursor.1);
        // Display column of the text cursor inside the single-line preview:
        // every preceding line contributes its width plus the ` ` joiner.
        let cursor_col = lines
            .iter()
            .take(row)
            .map(|line| UnicodeWidthStr::width(line.as_str()) + 1)
            .sum::<usize>()
            + lines
                .get(row)
                .map(|line| {
                    line.chars()
                        .take(col)
                        .map(|c| c.width().unwrap_or(0))
                        .sum::<usize>()
                })
                .unwrap_or(0);
        (
            detail.focus == DetailFocus::Answer,
            index,
            sanitize_terminal_text(&lines.join(" ")),
            cursor_col,
            detail.variant_selected,
            sanitize_terminal_text(&question.body),
            question.variants.clone(),
        )
    };

    let title = format!(
        " Answer question {}/{} · ↑↓ variant · Enter send ",
        index + 1,
        open_questions.len()
    );
    // The preview is one terminal row, so long answers scroll a window that
    // keeps the text cursor visible instead of hiding it past the truncation.
    let content_width = area.width.saturating_sub(19) as usize;
    let window_start = cursor_col.saturating_sub(content_width.saturating_sub(1));
    let display_text = width_slice_from(&input_text, window_start);
    let custom_label = if input_text.is_empty() {
        format!("{ANSWER_LABEL}(type to fill)")
    } else {
        format!("{ANSWER_LABEL}{display_text}")
    };
    let mut options = vec![option_line(
        &custom_label,
        selected == 0,
        focused,
        app.is_hovered(HitAction::DetailAnswerInput)
            || app.is_hovered(HitAction::DetailAnswerOption { index: 0 }),
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
    let show_switch = open_questions.len() > 1;
    let mut lines = vec![Line::from(Span::styled(
        question_body,
        Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
    ))];
    let visible_options = area
        .height
        .saturating_sub(3 + u16::from(show_switch))
        .max(1) as usize;
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
        // The custom-answer row is an editable text field as well as an
        // option: the mouse places the caret there (see
        // `App::handle_text_input_mouse`), so it registers the dedicated
        // input hitbox instead of a plain option one.
        let action = if option_index == 0 {
            HitAction::DetailAnswerInput
        } else {
            HitAction::DetailAnswerOption {
                index: option_index,
            }
        };
        app.hitboxes.push(Hitbox {
            area: Rect {
                x: area.x.saturating_add(1),
                y: area
                    .y
                    .saturating_add(1 + (first_option_row + visible_offset) as u16),
                width: area.width.saturating_sub(2),
                height: 1,
            },
            action,
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
    if show_switch {
        render_question_switch_buttons(app, frame, theme, area, index, open_questions.len());
    }
    if focused && selected == 0 && start == 0 {
        let cursor_x = area
            .x
            .saturating_add(3)
            .saturating_add(UnicodeWidthStr::width(ANSWER_LABEL) as u16)
            .saturating_add((cursor_col - window_start) as u16);
        frame.set_cursor_position((
            cursor_x.min(area.x.saturating_add(area.width.saturating_sub(2))),
            area.y.saturating_add(2),
        ));
    }
}

/// The `< previous question` / `next question >` row pinned to the bottom of
/// the answer panel. Question switching lives here because ←/→ belong to the
/// custom answer text while that panel is focused.
fn render_question_switch_buttons(
    app: &mut App,
    frame: &mut Frame<'_>,
    theme: &Theme,
    area: Rect,
    index: usize,
    count: usize,
) {
    let prev_label = "< previous question";
    let next_label = "next question >";
    let prev_enabled = index > 0;
    let next_enabled = index + 1 < count;
    let switch_style = |enabled: bool, hovered: bool| {
        if !enabled {
            Style::default().fg(theme.muted)
        } else if hovered {
            Style::default()
                .fg(theme.focus)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.focus)
        }
    };
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(area.height.saturating_sub(2)),
        width: area.width.saturating_sub(2),
        height: 1,
    };
    let gap = inner
        .width
        .saturating_sub((prev_label.len() + next_label.len()) as u16)
        .max(2);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                prev_label,
                switch_style(prev_enabled, app.is_hovered(HitAction::DetailPrevQuestion)),
            ),
            Span::raw(" ".repeat(gap as usize)),
            Span::styled(
                next_label,
                switch_style(next_enabled, app.is_hovered(HitAction::DetailNextQuestion)),
            ),
        ])),
        inner,
    );
    if prev_enabled {
        app.hitboxes.insert(
            0,
            Hitbox {
                area: Rect {
                    x: inner.x,
                    y: inner.y,
                    width: prev_label.len() as u16,
                    height: 1,
                },
                action: HitAction::DetailPrevQuestion,
            },
        );
    }
    if next_enabled {
        app.hitboxes.insert(
            0,
            Hitbox {
                area: Rect {
                    x: inner
                        .x
                        .saturating_add(inner.width.saturating_sub(next_label.len() as u16)),
                    y: inner.y,
                    width: next_label.len() as u16,
                    height: 1,
                },
                action: HitAction::DetailNextQuestion,
            },
        );
    }
}

/// Subslice of `text` starting at the first char boundary at or after
/// display-width offset `start_width` (wide chars are never split).
fn width_slice_from(text: &str, start_width: usize) -> &str {
    let mut acc = 0;
    for (offset, ch) in text.char_indices() {
        if acc >= start_width {
            return &text[offset..];
        }
        acc += ch.width().unwrap_or(0);
    }
    ""
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
    let hovered = app.is_hovered(HitAction::DetailEdits);
    let conflicted = detail
        .task
        .as_ref()
        .is_some_and(|task| task.integration == IntegrationState::Conflict);
    let title = if !editable {
        " Review edits (read-only outside Review) ".to_string()
    } else if conflicted {
        " Review edits — conflict report · resolve in the worktree · Ctrl+R re-run ".to_string()
    } else if focused {
        " Review edits [focused] · Ctrl+S save · Ctrl+V paste · Ctrl+R re-run · Esc thread "
            .to_string()
    } else if hovered {
        " Review edits [click to focus] · Ctrl+S save · Ctrl+R re-run ".to_string()
    } else {
        " Review edits · type or Tab to edit · Ctrl+S save · Ctrl+R re-run ".to_string()
    };
    app.hitboxes.push(Hitbox {
        area,
        action: HitAction::DetailEdits,
    });
    let review_edits = &mut app.detail.as_mut().unwrap().review_edits;
    set_cursor_visible(review_edits, focused);
    review_edits.set_block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if conflicted && editable {
                theme.err
            } else if focused || hovered {
                theme.focus
            } else if editable {
                theme.review
            } else {
                theme.muted
            })),
    );
    frame.render_widget(&*review_edits, area);
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
        let mut buttons: Vec<(String, UiAction)> =
            if app.primary_run_action_for(task) == UiAction::Revoke {
                vec![("↻ Revoke r".to_string(), UiAction::Revoke)]
            } else {
                vec![
                    ("▶ Queue r".to_string(), UiAction::Run),
                    ("⚡ Now F".to_string(), UiAction::RunNow),
                ]
            };
        if app.task_can_stop(task) {
            buttons.push(("Stop k".to_string(), UiAction::Stop));
        }
        if app.board.remote_runs.contains_key(&task.id) || task.runner.is_some() {
            buttons.push(("Log L".to_string(), UiAction::ViewLog));
        }
        if has_questions {
            buttons.push(("Answer w".to_string(), UiAction::AnswerQuestion));
        }
        if task.status == TaskStatus::Review {
            buttons.push(("Approve y".to_string(), UiAction::Approve));
            // Re-dispatch is how a conflict report gets acted on, so the
            // button is painted in the alarm color below.
            buttons.push(("Re-run ^R".to_string(), UiAction::Rerun));
        }
        if app.board.session_states.get(&task.id) == Some(&SessionState::Crashed) {
            buttons.push(("Recover u".to_string(), UiAction::Recover));
        }
        buttons.push(("Edit e".to_string(), UiAction::EditTask));
        buttons.push(("Move m".to_string(), UiAction::MoveTask));
        buttons.push(("+Ctx c".to_string(), UiAction::AddContext));
        // Read-only viewers, shown only when the task actually has a recorded
        // prompt / context to show.
        if app.detail.as_ref().is_some_and(|detail| detail.has_prompt) {
            buttons.push(("Prompt p".to_string(), UiAction::ViewPrompt));
        }
        if app
            .detail
            .as_ref()
            .is_some_and(|detail| detail.has_provenance)
        {
            buttons.push(("Inputs v".to_string(), UiAction::ViewContext));
        }
        if app.ops.task_has_backups(&task.id) {
            buttons.push(("Revert".to_string(), UiAction::Revert));
        }
        buttons.push(("Del d".to_string(), UiAction::DeleteTask));
        buttons
    };

    // While letters type into a text panel the hotkeys live on Alt, so the
    // trailing key of each label is shown as `M-y`.
    let typing = app
        .detail
        .as_ref()
        .is_some_and(|detail| detail.type_target().is_some());
    let mut spans = Vec::new();
    let mut x = area.x;
    let rerun_urgent =
        task.integration == IntegrationState::Conflict && task.status == TaskStatus::Review;
    for (label, action) in buttons {
        let label = match label.rsplit_once(' ') {
            Some((name, key)) if typing && key.chars().count() == 1 => format!("{name} M-{key}"),
            _ => label,
        };
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
        let urgent = rerun_urgent && action == UiAction::Rerun;
        let style = if app.is_hovered(HitAction::Action(action)) {
            Style::default()
                .fg(theme.focus)
                .bg(theme.border)
                .add_modifier(Modifier::BOLD)
        } else if urgent {
            Style::default()
                .fg(theme.err)
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

fn wrapped_row_count(lines: &[Line<'static>], width: u16) -> u16 {
    if lines.is_empty() || width == 0 {
        return 0;
    }
    u16::try_from(
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .line_count(width),
    )
    .unwrap_or(u16::MAX)
}

/// Which logical line every wrapped row of `lines` comes from at `width`, so
/// copying several rows can rejoin a paragraph the wrap split. The caller sets
/// the on-screen area and trims `rows` to the scrolled viewport.
fn wrapped_text_region(lines: &[Line<'static>], width: u16) -> TextRegion {
    let mut region = TextRegion::default();
    for (index, line) in lines.iter().enumerate() {
        let rows = wrapped_row_count(std::slice::from_ref(line), width).max(1);
        region
            .rows
            .extend(std::iter::repeat_n(index, usize::from(rows)));
        region.lines.push(
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect(),
        );
    }
    region
}

fn append_text_region(target: &mut TextRegion, source: TextRegion) {
    let offset = target.lines.len();
    target
        .rows
        .extend(source.rows.into_iter().map(|row| row + offset));
    target.lines.extend(source.lines);
}

fn thread_lines(
    messages: &[&Message],
    selected_id: Option<&str>,
    theme: &Theme,
    width: u16,
    links: &mut Vec<super::markdown::Link>,
    region: &mut TextRegion,
) -> Vec<Line<'static>> {
    if messages.is_empty() {
        return vec![Line::from("No thread messages")];
    }
    let mut lines = Vec::new();
    for message in messages {
        let start = lines.len();
        let selected = selected_id == Some(message.id.as_str());
        let rejected = message.status == MessageStatus::Rejected;
        let mut style = match message.kind {
            MessageKind::Question => Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            MessageKind::Suggestion => Style::default().fg(theme.focus),
            MessageKind::Context => Style::default().fg(theme.muted),
            MessageKind::ReviewEdit => Style::default().fg(theme.review),
            MessageKind::Task => Style::default().fg(theme.ok),
            MessageKind::System => Style::default().fg(theme.muted),
            MessageKind::AgentStep => Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        };
        if rejected {
            style = Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::DIM | Modifier::CROSSED_OUT);
        }
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let status = match message.status {
            MessageStatus::Open => "open",
            MessageStatus::Answered => "answered",
            MessageStatus::Resolved => "resolved",
            MessageStatus::Rejected => "rejected",
        };
        let marker = if selected { "» " } else { "" };
        let mut header = format!(
            "{marker}{} · {} · {} · {}",
            message.id,
            timefmt::human(&message.created_at),
            message.kind,
            status
        );
        if let Some(origin) = message
            .origin
            .as_deref()
            .filter(|origin| !origin.trim().is_empty())
        {
            if let Some(author) = message
                .author
                .as_deref()
                .filter(|author| !author.trim().is_empty())
            {
                header.push_str(" · by ");
                header.push_str(&sanitize_terminal_text(author.trim()));
            }
            header.push_str(" · origin=");
            header.push_str(&sanitize_terminal_text(origin.trim()));
        }
        lines.push(Line::from(Span::styled(header, style)));
        let mut body_style = Style::default();
        if rejected {
            body_style = body_style.add_modifier(Modifier::DIM | Modifier::CROSSED_OUT);
        }
        if selected {
            body_style = body_style.add_modifier(Modifier::REVERSED);
        }
        let sanitized_body = sanitize_terminal_text(&message.body);
        let mut rendered = super::markdown::render(&sanitized_body, width, body_style);
        append_text_region(region, wrapped_text_region(&lines[start..], width));
        let offset = region.rows.len();
        for link in &mut rendered.links {
            link.row += offset;
        }
        links.extend(rendered.links);
        let source_offset = region.lines.len();
        region.rows.extend(
            rendered
                .row_sources
                .into_iter()
                .map(|row| row + source_offset),
        );
        region.lines.extend(rendered.source_lines);
        lines.extend(rendered.lines);
        let tail_start = lines.len();
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
        append_text_region(region, wrapped_text_region(&lines[tail_start..], width));
    }
    lines
}
