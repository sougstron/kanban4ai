//! CommonMark/GFM text rendering. Link coordinates travel with styled text,
//! never with source offsets (delimiters and destinations are not displayed).
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Debug)]
struct Piece {
    text: String,
    style: Style,
    url: Option<String>,
}
type RichLine = Vec<Piece>;
#[derive(Clone, Debug)]
pub(super) struct Link {
    pub row: usize,
    pub column: usize,
    pub width: usize,
    pub url: String,
}
#[derive(Default)]
pub(super) struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub links: Vec<Link>,
    pub source_lines: Vec<String>,
    pub row_sources: Vec<usize>,
}

pub(super) fn render(body: &str, width: u16, base: Style) -> Rendered {
    let mut lines: Vec<RichLine> = Vec::new();
    let mut current = RichLine::new();
    let mut stack: Vec<(Style, Option<String>)> = Vec::new();
    let mut style = base;
    let mut url = None;
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut quote = 0;
    let body = separate_table_rows(body);
    let mut table: Option<(Vec<Alignment>, Vec<Vec<RichLine>>)> = None;
    let mut row: Vec<RichLine> = Vec::new();
    let flush = |current: &mut RichLine, lines: &mut Vec<RichLine>| {
        if !current.is_empty() {
            lines.push(std::mem::take(current));
        }
    };
    let mut previous_end = 0;
    for (event, range) in Parser::new_ext(
        &body,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES,
    )
    .into_offset_iter()
    {
        if matches!(
            &event,
            Event::Start(
                Tag::Paragraph
                    | Tag::Heading { .. }
                    | Tag::CodeBlock(_)
                    | Tag::List(_)
                    | Tag::BlockQuote(_)
                    | Tag::Table(_)
            ) | Event::Rule
        ) && range.start >= previous_end
            && body[previous_end..range.start].contains("\n\n")
            && lines.last().is_some_and(|line| !line.is_empty())
        {
            lines.push(Vec::new());
        }
        if matches!(
            &event,
            Event::Text(_)
                | Event::Code(_)
                | Event::Html(_)
                | Event::InlineHtml(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::Rule
        ) {
            previous_end = previous_end.max(range.end);
        }
        match event {
            Event::Start(tag) => {
                stack.push((style, url.clone()));
                match tag {
                    Tag::Strong => style = style.add_modifier(Modifier::BOLD),
                    Tag::Emphasis => style = style.add_modifier(Modifier::ITALIC),
                    Tag::Strikethrough => style = style.add_modifier(Modifier::CROSSED_OUT),
                    Tag::Heading { .. } => {
                        flush(&mut current, &mut lines);
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                        url = safe_url(&dest_url).then(|| dest_url.to_string());
                    }
                    Tag::BlockQuote(_) => {
                        flush(&mut current, &mut lines);
                        quote += 1;
                    }
                    Tag::List(start) => {
                        flush(&mut current, &mut lines);
                        lists.push(start);
                    }
                    Tag::Item => {
                        flush(&mut current, &mut lines);
                        let indent = "  ".repeat(lists.len().saturating_sub(1));
                        let marker = match lists.last_mut() {
                            Some(Some(number)) => {
                                let value = format!("{number}. ");
                                *number += 1;
                                value
                            }
                            _ => "• ".to_owned(),
                        };
                        current.push(Piece {
                            text: format!("{indent}{marker}"),
                            style,
                            url: None,
                        });
                    }
                    Tag::CodeBlock(_) => {
                        flush(&mut current, &mut lines);
                        style = style.add_modifier(Modifier::DIM);
                    }
                    Tag::Paragraph if quote > 0 => {
                        current.push(Piece {
                            text: "│ ".repeat(quote),
                            style,
                            url: None,
                        });
                    }
                    Tag::Table(alignment) => {
                        flush(&mut current, &mut lines);
                        table = Some((alignment, Vec::new()));
                    }
                    Tag::TableHead | Tag::TableRow => row.clear(),
                    Tag::TableCell => current.clear(),
                    _ => {}
                }
            }
            Event::End(tag) => {
                match tag {
                    TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => {
                        flush(&mut current, &mut lines)
                    }
                    TagEnd::CodeBlock => {
                        flush(&mut current, &mut lines);
                    }
                    TagEnd::BlockQuote(_) => {
                        flush(&mut current, &mut lines);
                        quote = quote.saturating_sub(1);
                    }
                    TagEnd::List(_) => {
                        lists.pop();
                    }
                    TagEnd::TableCell => row.push(std::mem::take(&mut current)),
                    TagEnd::TableHead | TagEnd::TableRow => {
                        if let Some((_, rows)) = &mut table {
                            rows.push(std::mem::take(&mut row));
                        }
                    }
                    TagEnd::Table => {
                        if let Some((alignment, rows)) = table.take() {
                            lines.extend(render_table(rows, &alignment, usize::from(width), base));
                        }
                    }
                    _ => {}
                }
                if let Some(previous) = stack.pop() {
                    (style, url) = previous;
                }
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                for (index, part) in text.split('\n').enumerate() {
                    if index > 0 {
                        // Code and hard source line breaks retain their rows.
                        lines.push(std::mem::take(&mut current));
                    }
                    if !part.is_empty() {
                        current.push(Piece {
                            text: part.to_owned(),
                            style,
                            url: url.clone(),
                        });
                    }
                }
            }
            Event::Code(text) => current.push(Piece {
                text: text.to_string(),
                style: style.add_modifier(Modifier::DIM),
                url: url.clone(),
            }),
            Event::SoftBreak | Event::HardBreak => {
                if table.is_some() {
                    current.push(Piece {
                        text: " ".to_owned(),
                        style,
                        url: url.clone(),
                    });
                } else {
                    lines.push(std::mem::take(&mut current));
                    if quote > 0 {
                        current.push(raw("│ ".repeat(quote), style));
                    }
                }
            }
            Event::Rule => {
                flush(&mut current, &mut lines);
                lines.push(vec![Piece {
                    text: "─".repeat(usize::from(width)),
                    style,
                    url: None,
                }]);
            }
            Event::TaskListMarker(checked) => current.push(Piece {
                text: if checked { "☑ " } else { "☐ " }.to_owned(),
                style,
                url: None,
            }),
            _ => {}
        }
    }
    flush(&mut current, &mut lines);
    let mut result = Rendered::default();
    for line in lines {
        let logical = result.source_lines.len();
        result
            .source_lines
            .push(line.iter().map(|piece| piece.text.as_str()).collect());
        for wrapped in wrap(&line, usize::from(width).max(2)) {
            result.row_sources.push(logical);
            let row = result.lines.len();
            let mut column = 0;
            let mut spans = Vec::new();
            for piece in wrapped {
                let size = piece.text.width();
                if let Some(url) = piece.url {
                    result.links.push(Link {
                        row,
                        column,
                        width: size,
                        url,
                    });
                }
                column += size;
                spans.push(Span::styled(piece.text, piece.style));
            }
            result.lines.push(Line::from(spans));
        }
    }
    if result.lines.is_empty() {
        result.lines.push(Line::from(""));
        result.source_lines.push(String::new());
        result.row_sources.push(0);
    }
    result
}

// GFM treats any following prose as a short table row and discards extra
// cells. Keep TASK-400's lossless boundary: only equal-width pipe rows belong
// to a table; malformed rows remain visible as ordinary text.
fn separate_table_rows(body: &str) -> std::borrow::Cow<'_, str> {
    let mut output: Option<String> = None;
    let mut previous = None;
    let mut columns = None;
    let mut fence = None;
    let mut offset = 0;
    for source_line in body.split_inclusive('\n') {
        let line = source_line.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let run = trimmed.chars().take_while(|&ch| ch == marker).count();
        if matches!(marker, '`' | '~') && run >= 3 {
            match fence {
                None => fence = Some((marker, run)),
                Some((open, count))
                    if marker == open && run >= count && trimmed[run..].trim().is_empty() =>
                {
                    fence = None
                }
                _ => {}
            }
        }
        let cells = table_cells(line);
        if fence.is_none() {
            if let Some(count) = columns {
                if cells.as_ref().is_none_or(|cells| cells.len() != count) {
                    if !line.trim().is_empty() {
                        output
                            .get_or_insert_with(|| body[..offset].to_owned())
                            .push('\n');
                    }
                    columns = None;
                }
            } else if let (Some(header), Some(cells)) = (previous, &cells)
                && header == cells.len()
                && cells.iter().all(|cell| {
                    let dashes = cell.trim_matches(':');
                    dashes.len() >= 3 && dashes.bytes().all(|ch| ch == b'-')
                })
            {
                columns = Some(header);
            }
        }
        previous = if fence.is_none() {
            cells.map(|cells| cells.len())
        } else {
            None
        };
        if let Some(output) = &mut output {
            output.push_str(source_line);
        }
        offset += source_line.len();
    }
    output.map_or(std::borrow::Cow::Borrowed(body), std::borrow::Cow::Owned)
}

fn table_cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if !line.contains('|') {
        return None;
    }
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.chars().peekable();
    let mut has_pipe = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => cell.push(chars.next().expect("peeked pipe")),
            '|' => {
                cells.push(cell.trim().to_owned());
                cell.clear();
                has_pipe = true;
            }
            _ => cell.push(ch),
        }
    }
    if !has_pipe {
        return None;
    }
    cells.push(cell.trim().to_owned());
    if line.starts_with('|') {
        cells.remove(0);
    }
    if line.ends_with('|') && cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    (!cells.is_empty()).then_some(cells)
}

pub(super) fn safe_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:"))
        && !url.chars().any(|ch| ch.is_control() || ch.is_whitespace())
}

fn rich_width(line: &RichLine) -> usize {
    line.iter().map(|piece| piece.text.width()).sum()
}
/// Use the desktop URL handler, with no shell interpolation or terminal output.
pub(super) fn open_url(url: &str) -> std::io::Result<()> {
    if !safe_url(url) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unsupported URL",
        ));
    }
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}
fn raw(text: impl Into<String>, style: Style) -> Piece {
    Piece {
        text: text.into(),
        style,
        url: None,
    }
}

// Wrapping preserves span styles and link destinations across display rows.
fn wrap(line: &RichLine, width: usize) -> Vec<RichLine> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut used = 0;
    for piece in line {
        let mut text = String::new();
        for word in piece.text.split_inclusive(char::is_whitespace) {
            if used > 0 && word.width() <= width && used + word.width() > width {
                if !text.is_empty() {
                    row.push(Piece {
                        text: std::mem::take(&mut text),
                        style: piece.style,
                        url: piece.url.clone(),
                    });
                }
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            for ch in word.chars() {
                let size = ch.width().unwrap_or(0);
                if used + size > width {
                    if !text.is_empty() {
                        row.push(Piece {
                            text: std::mem::take(&mut text),
                            style: piece.style,
                            url: piece.url.clone(),
                        });
                    }
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                text.push(ch);
                used += size;
            }
        }
        if !text.is_empty() {
            row.push(Piece {
                text,
                style: piece.style,
                url: piece.url.clone(),
            });
        }
    }
    rows.push(row);
    rows
}

fn render_table(
    rows: Vec<Vec<RichLine>>,
    alignment: &[Alignment],
    width: usize,
    style: Style,
) -> Vec<RichLine> {
    let mut result = Vec::new();
    let Some(header) = rows.first() else {
        return result;
    };
    let columns = header.len();
    let overhead = columns * 3 + 1;
    if width < overhead + columns * 2 {
        for row in rows.iter().skip(1) {
            for (header, cell) in header.iter().zip(row) {
                let mut line = header.clone();
                line.push(raw(": ", style));
                line.extend(cell.clone());
                result.push(line);
            }
            result.push(Vec::new());
        }
        if rows.len() == 1 {
            for cell in header {
                result.push(cell.clone());
            }
        }
        return result;
    }
    let mut desired = vec![2; columns];
    for row in &rows {
        for (size, cell) in desired.iter_mut().zip(row) {
            *size = (*size).max(rich_width(cell));
        }
    }
    let mut widths = vec![2; columns];
    let mut remaining = width - overhead - columns * 2;
    while remaining > 0 {
        let mut changed = false;
        for (size, desired) in widths.iter_mut().zip(&desired) {
            if *size < *desired && remaining > 0 {
                *size += 1;
                remaining -= 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let border = |left: char, middle: char, right: char| {
        let mut text = left.to_string();
        for (index, size) in widths.iter().enumerate() {
            text.push_str(&"─".repeat(size + 2));
            text.push(if index + 1 == columns { right } else { middle });
        }
        vec![raw(text, style)]
    };
    result.push(border('┌', '┬', '┐'));
    for (index, row) in rows.iter().enumerate() {
        let cells: Vec<_> = row
            .iter()
            .zip(&widths)
            .map(|(cell, size)| wrap(cell, *size))
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for y in 0..height {
            let mut line = vec![raw("│", style)];
            for (column, cell) in cells.iter().enumerate() {
                let value = cell.get(y).cloned().unwrap_or_default();
                let padding = widths[column] - rich_width(&value);
                let left = match alignment[column] {
                    Alignment::Right => padding,
                    Alignment::Center => padding / 2,
                    _ => 0,
                };
                line.push(raw(format!(" {}", " ".repeat(left)), style));
                line.extend(value.into_iter().map(|mut piece| {
                    if index == 0 {
                        piece.style = piece.style.add_modifier(Modifier::BOLD);
                    }
                    piece
                }));
                line.push(raw(format!("{} │", " ".repeat(padding - left)), style));
            }
            result.push(line);
        }
        if index == 0 {
            result.push(border('├', '┼', '┤'));
        }
    }
    result.push(border('└', '┴', '┘'));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inline_styles_links_and_literal_code() {
        let rendered = render(
            "### Header\n\n**bold** *italic* ~~old~~ `**literal**` [Benchmark methodology](https://example.org/a#b)",
            100,
            Style::default(),
        );
        let spans: Vec<_> = rendered.lines.iter().flat_map(|line| &line.spans).collect();
        for (text, modifier) in [
            ("Header", Modifier::BOLD),
            ("bold", Modifier::BOLD),
            ("italic", Modifier::ITALIC),
            ("old", Modifier::CROSSED_OUT),
        ] {
            assert!(
                spans
                    .iter()
                    .any(|span| span.content == text && span.style.add_modifier.contains(modifier))
            );
        }
        assert!(spans.iter().any(|span| span.content == "**literal**"));
        assert_eq!(rendered.links[0].url, "https://example.org/a#b");
        assert!(
            !rendered
                .lines
                .iter()
                .any(|line| line.to_string().contains("https://"))
        );
    }
    #[test]
    fn wrapped_unicode_link_keeps_destination_and_columns() {
        let rendered = render("[界界界](https://example.org)", 4, Style::default());
        assert_eq!(
            rendered
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["界界", "界"]
        );
        assert_eq!(
            (
                rendered.links[0].row,
                rendered.links[0].column,
                rendered.links[0].width
            ),
            (0, 0, 4)
        );
        assert_eq!(
            (
                rendered.links[1].row,
                rendered.links[1].column,
                rendered.links[1].width
            ),
            (1, 0, 2)
        );
    }
    #[test]
    fn unsafe_destinations_are_not_clickable_and_fences_are_literal() {
        assert!(
            render("[bad](javascript:alert(1))", 80, Style::default())
                .links
                .is_empty()
        );
        let rendered = render(
            "```rust\n**bold** [x](https://example.org)\n```",
            80,
            Style::default(),
        );
        assert!(rendered.links.is_empty());
        assert_eq!(
            rendered.lines[0].to_string(),
            "**bold** [x](https://example.org)"
        );
    }
    #[test]
    fn lists_quotes_rules_and_reference_links() {
        let rendered = render(
            "- [x] done\n- open\n\n1. first\n2. second\n\n> quote\n\n---\n\n[ref][target]\n\n[target]: https://example.org",
            30,
            Style::default(),
        );
        let text: Vec<_> = rendered.lines.iter().map(ToString::to_string).collect();
        assert!(text.contains(&"• ☑ done".to_owned()));
        assert!(text.contains(&"1. first".to_owned()));
        assert!(text.contains(&"│ quote".to_owned()));
        assert!(text.contains(&"─".repeat(30)));
        assert_eq!(rendered.links.last().unwrap().url, "https://example.org");
    }
}
