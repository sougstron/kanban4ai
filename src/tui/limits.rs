//! The provider limits row, directly above the status bar, showing how much of
//! each AI subscription window is still available and when it resets. It is
//! two lines tall when any provider has both kinds of window: short ones (5h)
//! on the upper line, weekly/monthly ones on the lower. A provider with only
//! one kind keeps it on the upper line, level with its name.
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
//! Every provider segment is clickable: a click refreshes that provider on
//! the spot (claude force-polls the usage endpoint; grok renews its token via
//! the grok CLI; zai, synthetic, kimi, and gemini re-fetch — see
//! [`crate::core::limits::refresh_provider_async`]).

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

/// Providers whose segment of the row refreshes on click: all of them, so a
/// manual refresh is always one tap away even for the HTTPS-only providers.
const CLICKABLE: &[&str] = &limits::PROVIDERS;

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
        "zai" => Color::Rgb(112, 145, 219),
        "synthetic" => Color::Rgb(178, 142, 212),
        "kimi" => Color::Rgb(232, 196, 104),
        "gemini" => Color::Rgb(96, 156, 250),
        _ => app.theme.fg,
    }
}

pub fn provider_icon(provider: &str) -> &'static str {
    match provider {
        "claude" => "✳",
        "codex" => "✺",
        "grok" => "✕",
        "zai" => "◆",
        "synthetic" => "✦",
        "kimi" => "☾",
        "gemini" => "✧",
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

/// Two lines when any provider has both a short window and a long one to
/// stack, otherwise one.
pub fn row_height(app: &App) -> u16 {
    if !is_visible(app) {
        return 0;
    }
    let now = chrono::Utc::now().timestamp();
    let two_lines = app.limits.as_ref().is_some_and(|snapshot| {
        provider_blocks(app, snapshot, now, Detail::Full)
            .iter()
            .any(|block| !block.lower.is_empty())
    });
    if two_lines { 2 } else { 1 }
}

/// Whether the row draws at all. Only the displayed providers count: the
/// snapshot may still carry an entry for a parked one (codex), which must not
/// reserve an empty line.
fn has_any_provider(snapshot: &LimitsSnapshot) -> bool {
    limits::PROVIDERS
        .iter()
        .filter_map(|provider| snapshot.get(provider))
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
    let (lines, segments) = build_lines(app, snapshot, now, area.width);
    for segment in segments {
        if CLICKABLE.contains(&segment.provider) {
            app.hitboxes.push(Hitbox {
                area: Rect {
                    x: area.x.saturating_add(segment.x),
                    y: area.y,
                    width: segment.width.max(1),
                    height: area.height,
                },
                action: HitAction::Action(UiAction::RefreshLimits(segment.provider)),
            });
        }
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(app.theme.bg).fg(app.theme.muted)),
        area,
    );
}

/// A provider's slice of the rendered row, for click targeting.
struct Segment {
    provider: &'static str,
    x: u16,
    width: u16,
}

/// One provider's part of the row: its name, then its windows split by length
/// — short ones (5h, 24h, daily model quotas) on the upper line, long ones
/// (7d, mon, …) on the lower. A provider with only one kind keeps it on the
/// upper line, level with its name, so `lower` is empty unless both exist.
struct Block {
    provider: &'static str,
    name: Vec<Span<'static>>,
    upper: Vec<Span<'static>>,
    lower: Vec<Span<'static>>,
}

impl Block {
    fn width(&self) -> usize {
        spans_width(&self.name) + spans_width(&self.upper).max(spans_width(&self.lower))
    }
}

/// Assemble the row at the richest detail level that fits, dropping providers
/// from the right when even the compact form is too wide.
fn build_lines(
    app: &App,
    snapshot: &LimitsSnapshot,
    now: i64,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Segment>) {
    let width = usize::from(width);
    for detail in [Detail::Full, Detail::NoReset, Detail::Percent] {
        let mut blocks = provider_blocks(app, snapshot, now, detail);
        if blocks.is_empty() {
            break;
        }
        if joined_width(&blocks) <= width {
            return lay_out(blocks);
        }
        if detail == Detail::Percent {
            while blocks.len() > 1 && joined_width(&blocks) > width {
                blocks.pop();
            }
            return lay_out(blocks);
        }
    }
    (Vec::new(), Vec::new())
}

fn provider_blocks(app: &App, snapshot: &LimitsSnapshot, now: i64, detail: Detail) -> Vec<Block> {
    limits::PROVIDERS
        .into_iter()
        .filter_map(|provider| {
            let entry = snapshot.get(provider)?;
            provider_block(app, provider, entry, now, detail)
        })
        .collect()
}

/// One provider's block, or `None` when the provider is not set up here.
fn provider_block(
    app: &App,
    provider: &'static str,
    entry: &ProviderLimits,
    now: i64,
    detail: Detail,
) -> Option<Block> {
    if entry.state == ProviderState::NotConfigured {
        return None;
    }
    let mut name = vec![Span::styled(
        format!("{} ", provider_icon(&entry.provider)),
        Style::default().fg(provider_color(app, &entry.provider)),
    )];
    if detail != Detail::Percent {
        name.push(Span::styled(
            format!("{} ", entry.provider),
            Style::default().fg(app.theme.muted),
        ));
    }
    let single = |text: &str| Block {
        provider,
        name: name.clone(),
        upper: vec![Span::styled(
            text.to_string(),
            Style::default().fg(app.theme.muted),
        )],
        lower: Vec::new(),
    };
    if !entry.is_ready() {
        return Some(single(state_label(&entry.state, detail)));
    }
    // Windows that have already rolled over hold a percentage for a period
    // that is over; the row drops them until a fresh observation lands.
    let windows = entry.live_windows(now);
    if windows.is_empty() {
        return Some(single(if detail == Detail::Percent {
            "n/a"
        } else {
            "stale"
        }));
    }
    let mut upper = Vec::new();
    let mut lower = Vec::new();
    for window in windows {
        let line = if is_short_window(&window.label) {
            &mut upper
        } else {
            &mut lower
        };
        if !line.is_empty() {
            line.push(Span::styled(
                " · ".to_string(),
                Style::default().fg(app.theme.border),
            ));
        }
        if detail != Detail::Percent {
            line.push(Span::styled(
                format!("{} ", window.label),
                Style::default().fg(app.theme.muted),
            ));
        }
        // A spend tally (gemini's API key) has no ceiling: show the amount.
        line.push(match window.spent_usd {
            Some(usd) => Span::styled(limits::format_usd(usd), Style::default().fg(app.theme.fg)),
            None => Span::styled(
                format!("{:.0}%", window.remaining_percent),
                Style::default().fg(percent_color(app, window.remaining_percent)),
            ),
        });
        if detail == Detail::Full
            && let Some(seconds) = window.resets_in(now)
        {
            line.push(Span::styled(
                format!(" ↻{}", format_span(seconds)),
                Style::default().fg(app.theme.muted),
            ));
        }
    }
    if upper.is_empty() {
        std::mem::swap(&mut upper, &mut lower);
    }
    // codex reports whatever the last local session was told, so its numbers
    // carry their own age; a fetched-live provider has none.
    if detail == Detail::Full
        && let Some(age) = entry.data_age(now).filter(|age| *age >= 60)
    {
        let last = if lower.is_empty() {
            &mut upper
        } else {
            &mut lower
        };
        last.push(Span::styled(
            format!(" ({} old)", format_span(age)),
            Style::default().fg(app.theme.border),
        ));
    }
    Some(Block {
        provider,
        name,
        upper,
        lower,
    })
}

/// Windows of a day or less go on the upper line: hour/minute lengths and
/// gemini's daily per-model quotas. Weekly, monthly, and anything else go
/// below.
fn is_short_window(label: &str) -> bool {
    matches!(label, "pro" | "flash" | "lite" | "quota")
        || label
            .strip_suffix(['m', 'h'])
            .is_some_and(|count| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
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

/// Lay the blocks out left to right on two lines, padding each block so the
/// separators line up, and record each provider's columns.
fn lay_out(blocks: Vec<Block>) -> (Vec<Line<'static>>, Vec<Segment>) {
    let mut upper = vec![Span::raw(" ")];
    let mut lower = vec![Span::raw(" ")];
    let mut segments = Vec::new();
    let mut x: u16 = 1;
    let count = blocks.len();
    for (index, block) in blocks.into_iter().enumerate() {
        if index > 0 {
            upper.push(Span::raw(SEPARATOR));
            lower.push(Span::raw(SEPARATOR));
            x = x.saturating_add(UnicodeWidthStr::width(SEPARATOR) as u16);
        }
        let width = block.width();
        let name_width = spans_width(&block.name);
        let upper_width = name_width + spans_width(&block.upper);
        let lower_width = name_width + spans_width(&block.lower);
        upper.extend(block.name);
        upper.extend(block.upper);
        lower.push(Span::raw(" ".repeat(name_width)));
        lower.extend(block.lower);
        if index + 1 < count {
            upper.push(Span::raw(" ".repeat(width - upper_width)));
            lower.push(Span::raw(" ".repeat(width - lower_width)));
        }
        segments.push(Segment {
            provider: block.provider,
            x,
            width: width as u16,
        });
        x = x.saturating_add(width as u16);
    }
    (vec![Line::from(upper), Line::from(lower)], segments)
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn joined_width(blocks: &[Block]) -> usize {
    let content: usize = blocks.iter().map(Block::width).sum();
    let separators = blocks.len().saturating_sub(1) * UnicodeWidthStr::width(SEPARATOR);
    content + separators + 1
}
