use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::models::{IntegrationState, RunPhase, Task, TaskStatus};
use crate::core::session::SessionState;
use crate::core::telemetry::SessionProgress;

use super::app::App;

pub fn render_card(
    frame: &mut Frame<'_>,
    app: &App,
    task: &Task,
    area: Rect,
    focused: bool,
    hovered: bool,
    dragging: bool,
) {
    let session_state = app.board.session_states.get(&task.id).copied();
    let retrying = task.restart_at.is_some();
    let border = if session_state == Some(SessionState::Crashed) && !retrying {
        app.theme.err
    } else if retrying || task.review_unseen || (task.has_questions && !hovered) {
        app.theme.warn
    } else if focused || hovered {
        app.theme.focus
    } else {
        app.theme.border
    };
    let line_width = area.width.saturating_sub(2).max(1) as usize;
    let query = app.search.text();
    let id_style = if focused || hovered {
        Style::default()
            .fg(app.theme.focus)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.muted)
    };
    let visible_id = truncate_display(&sanitize_terminal_text(&task.id), line_width);
    let visible_title = truncate_display(&sanitize_terminal_text(&task.title), line_width);
    let mut title_spans = Vec::new();
    if task.review_unseen {
        title_spans.push(Span::styled("● ", Style::default().fg(app.theme.warn)));
    }
    title_spans.extend(highlight_matches(
        &visible_id,
        &query,
        id_style,
        app.theme.warn,
    ));
    title_spans.push(Span::raw(" "));
    title_spans.extend(highlight_matches(
        &visible_title,
        &query,
        Style::default(),
        app.theme.warn,
    ));
    let mut lines = vec![Line::from(title_spans)];
    let extra = app.board.extras.get(&task.id);
    // The open question goes right under the title: it is the thing the
    // human must act on, and short cards truncate from the bottom.
    if let Some(preview) = extra.and_then(|extra| extra.question_preview.as_deref()) {
        lines.push(Line::from(Span::styled(
            truncate_display(
                &format!("? {}", sanitize_terminal_text(preview)),
                line_width,
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
                    truncate_display(&badge, line_width),
                    Style::default().fg(color),
                )))
            })
            .collect::<Vec<_>>();
        lines.push(Line::from(badge_spans));
    }
    // Live agent telemetry replaces the description on a running card: what the
    // agent is doing now is more actionable than the static blurb, and it keeps
    // the card's height bounded.
    let progress = app.session_progress.get(&task.id).filter(|p| p.has_data());
    if let Some(progress) = progress {
        lines.extend(telemetry_lines(progress, app, line_width));
    } else if !task.description.is_empty() {
        let description = truncate_display(
            &sanitize_terminal_text(&task.description).replace('\n', " "),
            line_width,
        );
        lines.push(Line::from(highlight_matches(
            &description,
            &query,
            Style::default().fg(app.theme.muted),
            app.theme.warn,
        )));
    }
    // The card in flight is inverted and bold-bordered so the source of a drag
    // stays visible while the cursor is off over the drop column.
    let border_style = if dragging {
        Style::default().fg(border).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(border)
    };
    let mut card_style = Style::default().bg(app.theme.bg).fg(app.theme.fg);
    if dragging {
        card_style = card_style.add_modifier(Modifier::REVERSED);
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    frame.render_widget(Paragraph::new(lines).block(block).style(card_style), area);
}

#[cfg(test)]
pub fn highlight_title_matches(
    title: &str,
    query: &str,
    warn: ratatui::style::Color,
) -> Vec<Span<'static>> {
    highlight_matches(title, query, Style::default(), warn)
}

pub fn highlight_matches(
    title: &str,
    query: &str,
    base: Style,
    warn: ratatui::style::Color,
) -> Vec<Span<'static>> {
    if query.is_empty() {
        return vec![Span::styled(title.to_string(), base)];
    }
    let chars = title.chars().collect::<Vec<_>>();
    let ranges = case_insensitive_match_ranges(&chars, query);
    let mut spans = Vec::new();
    let mut plain_start = 0;
    for (start, end) in ranges {
        if plain_start < start {
            spans.push(Span::styled(
                chars[plain_start..start].iter().collect::<String>(),
                base,
            ));
        }
        spans.push(Span::styled(
            chars[start..end].iter().collect::<String>(),
            base.fg(warn),
        ));
        plain_start = end;
    }
    if plain_start < chars.len() {
        spans.push(Span::styled(
            chars[plain_start..].iter().collect::<String>(),
            base,
        ));
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
    // Conflict is the one blocking isolation state (the report sits in
    // review_edits and a human must resolve it), so it leads the card and
    // displaces the plain worktree badge it implies.
    let conflicted = task.integration == IntegrationState::Conflict;
    if conflicted {
        badges.push(("⚠ conflict".to_string(), app.theme.err));
    }
    // Interactive is a persistent task flag, emitted first so a long volatile
    // session-state badge (`✖ crashed · u recover`) can never push it past the
    // card's right edge in a narrow column. It only matters while the task can
    // still run or is running; on Review/Done/Archive the flag is noise.
    if task.interactive && matches!(task.status, TaskStatus::Todo | TaskStatus::InProgress) {
        badges.push(("☑ interactive".to_string(), app.theme.ok));
    }
    // Design/review name the live bot. Queued never overlays a live session:
    // it means waiting for a slot. Live/waiting/crashed still come from
    // SessionState.
    let phase_badge = match task.run_phase {
        Some(RunPhase::Queued) => Some(("⏸ queued", app.theme.warn)),
        Some(RunPhase::Design) => Some(("✎ design", app.theme.focus)),
        Some(RunPhase::Review) => Some(("⚖ review", app.theme.review)),
        _ => None,
    };
    match session_state {
        Some(SessionState::Live) => {
            let (label, color) = match task.run_phase {
                Some(RunPhase::Design) => ("✎ design", app.theme.focus),
                Some(RunPhase::Review) => ("⚖ review", app.theme.review),
                _ => ("▶ running", app.theme.ok),
            };
            badges.push((label.to_string(), color));
        }
        Some(SessionState::Waiting) => {
            let label = app
                .board
                .session_wait_deadlines
                .get(&task.id)
                .map(|deadline| format!("⏳ until {}", deadline.format("%H:%M")))
                .unwrap_or_else(|| "⏳ wait mode".to_string());
            badges.push((label, app.theme.warn));
        }
        Some(SessionState::Crashed) => match task.restart_at {
            Some(restart_at) => badges.push((retry_badge(restart_at), app.theme.warn)),
            None => badges.push(("✖ crashed · u recover".to_string(), app.theme.err)),
        },
        None => {
            if let Some(restart_at) = task.restart_at {
                badges.push((retry_badge(restart_at), app.theme.warn));
            } else if let Some((label, color)) = phase_badge {
                badges.push((label.to_string(), color));
            }
        }
    }
    // Chaining only matters before the task runs: at In Progress the chain
    // already fired (or is moot) and past it the badge is noise, so the badge
    // lives on To Do alone and names the target task number.
    if task.status == TaskStatus::Todo
        && let Some(target) = task.chained_to.as_deref()
    {
        let number = target.rsplit('-').next().unwrap_or(target);
        badges.push((format!("↪ chain -> {number}"), app.theme.ok));
    }
    // Pending design/review stage marks, shown like the chain badge from the
    // moment the stage is scheduled (project-wide bot or the task's own
    // opt-in) until the stage completes: design until the plan is recorded
    // (`designed`), review until the verdict lands the task in human Review.
    // An active phase badge above already shows a running stage; it wins and
    // the pending mark stays off rather than duplicating it.
    if matches!(task.status, TaskStatus::Todo | TaskStatus::InProgress) {
        if (app.board.designer_bot || task.use_designer)
            && !task.designed
            && !badges.iter().any(|(label, _)| label == "✎ design")
        {
            badges.push(("✎ design".to_string(), app.theme.focus));
        }
        if (app.board.reviewer_bot || task.use_reviewer)
            && !badges.iter().any(|(label, _)| label == "⚖ review")
        {
            badges.push(("⚖ review".to_string(), app.theme.review));
        }
    }
    if task.has_questions {
        badges.push(("? questions".to_string(), app.theme.warn));
    }
    // Isolation is passive information, so its badge trails the action ones.
    // The fields only exist while the worktree does (cleared on land-with-
    // cleanup, Done and abandon; kept on Conflict), except that a Conflict
    // task keeps its unmerged worktree — there the err-colored conflict badge
    // above already says what matters.
    if task.worktree.is_some() && !conflicted {
        badges.push(("⑂ worktree".to_string(), app.theme.muted));
    }
    badges
}

pub fn retry_badge(restart_at: chrono::NaiveDateTime) -> String {
    format!("↻ retry {}", restart_at.format("%H:%M"))
}

/// The two live-telemetry rows for a running agent: a stats line (todo progress
/// bar, tokens, cost) and, when known, a `→ <last tool>` activity line. Either
/// may be empty; callers only reach here when [`SessionProgress::has_data`].
fn telemetry_lines(progress: &SessionProgress, app: &App, line_width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut stats: Vec<String> = Vec::new();
    if let Some((done, total)) = progress.todos() {
        stats.push(format!("{} {done}/{total}", progress_bar(done, total, 5)));
    }
    if let Some(tokens) = progress.tokens {
        stats.push(format!("{} tok", format_tokens(tokens)));
    }
    if let Some(cost) = progress.cost_usd {
        stats.push(format!("${cost:.2}"));
    }
    if !stats.is_empty() {
        rows.push(Line::from(Span::styled(
            truncate_display(&stats.join("  "), line_width),
            Style::default().fg(app.theme.ok),
        )));
    }
    if let Some(activity) = progress.last_activity.as_deref() {
        rows.push(Line::from(Span::styled(
            truncate_display(
                &format!("→ {}", sanitize_terminal_text(activity)),
                line_width,
            ),
            Style::default().fg(app.theme.muted),
        )));
    }
    rows
}

/// A fixed-width block-glyph progress bar (`▓▓▓░░`). An empty list renders all
/// hollow rather than dividing by zero.
fn progress_bar(done: usize, total: usize, width: usize) -> String {
    // Round to the nearest cell so a nearly-complete bar is not shown full;
    // `checked_div` yields `None` (→ 0 filled) when the list is empty.
    let filled = ((done * width) + total / 2)
        .checked_div(total)
        .unwrap_or(0)
        .min(width);
    "▓".repeat(filled) + &"░".repeat(width - filled)
}

/// Compact token count: `842`, `12.4k`, `1.2M`.
fn format_tokens(tokens: i64) -> String {
    match tokens {
        n if n < 1_000 => n.to_string(),
        n if n < 1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        n => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Inner content-line count a card needs (excludes the border). The column
/// layout grows every card to the tallest card in that column, so this must
/// mirror the line-pushing order in [`render_card`] exactly.
pub(crate) fn card_line_count(app: &App, task: &Task) -> u16 {
    let session_state = app.board.session_states.get(&task.id).copied();
    let extra = app.board.extras.get(&task.id);
    let mut lines: u16 = 1; // title
    if extra
        .and_then(|extra| extra.question_preview.as_deref())
        .is_some()
    {
        lines += 1;
    }
    let mut badge_count = badges(task, session_state, app).len();
    if extra.is_some_and(|extra| extra.waiting) {
        badge_count += 1;
    }
    if badge_count > 0 {
        lines += 1;
    }
    // The description is deliberately excluded: it stays the one row allowed to
    // clip, so a column of plain described cards keeps the configured height and
    // only questioned/running cards grow. Telemetry rows, by contrast, are
    // must-see and force growth.
    if let Some(progress) = app.session_progress.get(&task.id).filter(|p| p.has_data()) {
        if progress.todos().is_some() || progress.tokens.is_some() || progress.cost_usd.is_some() {
            lines += 1;
        }
        if progress.last_activity.is_some() {
            lines += 1;
        }
    }
    lines
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

/// Clipboard text made safe to insert into a text field: line endings collapse
/// to `\n`, everything else goes through the terminal sanitizer.
pub fn sanitize_paste_text(text: &str) -> String {
    sanitize_terminal_text(&text.replace("\r\n", "\n").replace('\r', "\n"))
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

    use chrono::NaiveDate;

    use super::{
        case_insensitive_match, format_tokens, highlight_title_matches, progress_bar, retry_badge,
        sanitize_terminal_text, truncate_display,
    };

    #[test]
    fn retry_badge_shows_the_deadline_clock() {
        let at = NaiveDate::from_ymd_opt(2026, 8, 23)
            .and_then(|d| d.and_hms_opt(14, 32, 0))
            .expect("valid fixture timestamp");
        assert_eq!(retry_badge(at), "↻ retry 14:32");
    }

    #[test]
    fn progress_bar_fills_and_rounds() {
        assert_eq!(progress_bar(0, 5, 5), "░░░░░");
        assert_eq!(progress_bar(5, 5, 5), "▓▓▓▓▓");
        assert_eq!(progress_bar(3, 5, 5), "▓▓▓░░");
        // 4/5 rounds up to 4 cells but must never overfill.
        assert_eq!(progress_bar(4, 5, 5), "▓▓▓▓░");
        // An empty list divides safely to all-hollow.
        assert_eq!(progress_bar(0, 0, 5), "░░░░░");
    }

    #[test]
    fn format_tokens_is_compact() {
        assert_eq!(format_tokens(842), "842");
        assert_eq!(format_tokens(12_400), "12.4k");
        assert_eq!(format_tokens(1_200_000), "1.2M");
    }

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
