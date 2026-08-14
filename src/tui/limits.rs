//! The provider limits row: one line directly above the status bar showing how
//! much of each AI subscription window is still available, and when it resets.
//!
//! Numbers come from [`crate::core::limits`], which refreshes them on a
//! background thread; this module only draws whatever snapshot is cached, so a
//! slow or unreachable provider never touches the event loop. Providers with no
//! credentials on the machine are omitted entirely — the row lists what the
//! user actually has.
//!
//! The row degrades with width: reset times drop first, then window labels and
//! provider names, then whole providers from the right.
//!
//! The codex and grok segments are clickable: a click refreshes that provider
//! through its own CLI (fresh codex numbers via the app-server RPC, a renewed
//! grok token via the grok CLI — see [`crate::core::limits::refresh_provider_async`]).
//! Claude has no CLI-side refresh to offer, so its segment is display-only.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::core::limits::{self, LimitsSnapshot, ProviderLimits, ProviderState, format_span};

use super::app::{App, HitAction, Hitbox, Screen, UiAction};

/// Between two providers; matches the status bar's divider.
const SEPARATOR: &str = "  │  ";

/// Providers whose segment of the row refreshes on click; claude has no
/// CLI-side refresh, so its segment stays display-only.
const CLICKABLE: [&str; 2] = ["codex", "grok"];

/// How much of each provider is spelled out. Tried in order until the row fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detail {
    /// `✳ claude 5h 49% ↻3h13m · 7d 95% ↻6d11h`
    Full,
    /// `✳ claude 5h 49% · 7d 95%`
    NoReset,
    /// `✳ 49% 95%`
    Percent,
}

/// Brand-ish accent per provider so the row is scannable without reading names.
fn provider_color(app: &App, provider: &str) -> Color {
    match provider {
        "claude" => Color::Rgb(203, 123, 93),
        "codex" => Color::Rgb(90, 190, 160),
        _ => app.theme.fg,
    }
}

pub fn provider_icon(provider: &str) -> &'static str {
    match provider {
        "claude" => "✳",
        "codex" => "✺",
        "grok" => "✕",
        _ => "•",
    }
}

/// The row is drawn on the two list screens the user works from, plus the help
/// overlay (which renders one of them underneath), and only once the event loop
/// has pulled in a snapshot so the layout never reserves a blank line.
pub fn is_visible(app: &App) -> bool {
    app.settings.show_limits
        && matches!(app.screen, Screen::Board | Screen::Projects | Screen::Help)
        && app
            .limits
            .as_ref()
            .is_some_and(|snapshot| has_any_provider(snapshot))
}

pub fn row_height(app: &App) -> u16 {
    u16::from(is_visible(app))
}

fn has_any_provider(snapshot: &LimitsSnapshot) -> bool {
    snapshot
        .providers
        .iter()
        .any(|entry| entry.state != ProviderState::NotConfigured)
}

pub fn render(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let Some(snapshot) = app.limits.as_ref() else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    let (line, segments) = build_line(app, snapshot, now, area.width);
    for segment in segments {
        if CLICKABLE.contains(&segment.provider) {
            app.hitboxes.push(Hitbox {
                area: Rect {
                    x: area.x.saturating_add(segment.x),
                    y: area.y,
                    width: segment.width.max(1),
                    height: 1,
                },
                action: HitAction::Action(UiAction::RefreshLimits(segment.provider)),
            });
        }
    }
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(app.theme.bg).fg(app.theme.muted)),
        area,
    );
}

/// A provider's slice of the rendered row, for click targeting.
struct Segment {
    provider: &'static str,
    x: u16,
    width: u16,
}

/// Assemble the row at the richest detail level that fits, dropping providers
/// from the right when even the compact form is too wide.
fn build_line(
    app: &App,
    snapshot: &LimitsSnapshot,
    now: i64,
    width: u16,
) -> (Line<'static>, Vec<Segment>) {
    let width = usize::from(width);
    for detail in [Detail::Full, Detail::NoReset, Detail::Percent] {
        let groups = provider_groups(app, snapshot, now, detail);
        if groups.is_empty() {
            return (Line::default(), Vec::new());
        }
        if joined_width(&groups) <= width {
            return lay_out(groups);
        }
        if detail == Detail::Percent {
            let mut groups = groups;
            while groups.len() > 1 && joined_width(&groups) > width {
                groups.pop();
            }
            return lay_out(groups);
        }
    }
    (Line::default(), Vec::new())
}

fn provider_groups(
    app: &App,
    snapshot: &LimitsSnapshot,
    now: i64,
    detail: Detail,
) -> Vec<(&'static str, Vec<Span<'static>>)> {
    limits::PROVIDERS
        .into_iter()
        .filter_map(|provider| {
            let entry = snapshot.get(provider)?;
            Some((provider, provider_spans(app, entry, now, detail)?))
        })
        .collect()
}

/// One provider's spans, or `None` when the provider is not set up here.
fn provider_spans(
    app: &App,
    entry: &ProviderLimits,
    now: i64,
    detail: Detail,
) -> Option<Vec<Span<'static>>> {
    if entry.state == ProviderState::NotConfigured {
        return None;
    }
    let mut spans = vec![Span::styled(
        format!("{} ", provider_icon(&entry.provider)),
        Style::default().fg(provider_color(app, &entry.provider)),
    )];
    if detail != Detail::Percent {
        spans.push(Span::styled(
            format!("{} ", entry.provider),
            Style::default().fg(app.theme.muted),
        ));
    }
    if !entry.is_ready() {
        spans.push(Span::styled(
            state_label(&entry.state, detail).to_string(),
            Style::default().fg(app.theme.muted),
        ));
        return Some(spans);
    }
    for (index, window) in entry.windows.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " · ".to_string(),
                Style::default().fg(app.theme.border),
            ));
        }
        if detail != Detail::Percent {
            spans.push(Span::styled(
                format!("{} ", window.label),
                Style::default().fg(app.theme.muted),
            ));
        }
        spans.push(Span::styled(
            format!("{:.0}%", window.remaining_percent),
            Style::default().fg(percent_color(app, window.remaining_percent)),
        ));
        if detail == Detail::Full
            && let Some(seconds) = window.resets_in(now)
        {
            spans.push(Span::styled(
                format!(" ↻{}", format_span(seconds)),
                Style::default().fg(app.theme.muted),
            ));
        }
    }
    // codex reports whatever the last local session was told, so its numbers
    // carry their own age; a fetched-live provider has none.
    if detail == Detail::Full
        && let Some(age) = entry.data_age(now).filter(|age| *age >= 60)
    {
        spans.push(Span::styled(
            format!(" ({} old)", format_span(age)),
            Style::default().fg(app.theme.border),
        ));
    }
    Some(spans)
}

fn state_label(state: &ProviderState, detail: Detail) -> &'static str {
    match state {
        ProviderState::SignedOut if detail != Detail::Percent => "signed out",
        ProviderState::SignedOut | ProviderState::Unavailable(_) => "n/a",
        _ => "—",
    }
}

fn percent_color(app: &App, remaining: f64) -> Color {
    if remaining < 15.0 {
        app.theme.err
    } else if remaining < 40.0 {
        app.theme.warn
    } else {
        app.theme.ok
    }
}

/// Lay the groups out left to right, recording each provider's columns.
fn lay_out(groups: Vec<(&'static str, Vec<Span<'static>>)>) -> (Line<'static>, Vec<Segment>) {
    let mut spans = vec![Span::raw(" ")];
    let mut segments = Vec::new();
    let mut x: u16 = 1;
    for (index, (provider, group)) in groups.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(SEPARATOR));
            x = x.saturating_add(UnicodeWidthStr::width(SEPARATOR) as u16);
        }
        let width = group
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()) as u16)
            .fold(0u16, u16::saturating_add);
        spans.extend(group);
        segments.push(Segment { provider, x, width });
        x = x.saturating_add(width);
    }
    (Line::from(spans), segments)
}

fn joined_width(groups: &[(&'static str, Vec<Span<'static>>)]) -> usize {
    let content: usize = groups
        .iter()
        .map(|(_, group)| {
            group
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
        })
        .sum();
    let separators = groups.len().saturating_sub(1) * UnicodeWidthStr::width(SEPARATOR);
    content + separators + 1
}
