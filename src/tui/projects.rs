//! Projects list screen: registered boards plus the optional create-for-cwd row.
//!
//! The list is laid out as a table: fixed columns measured once per frame, a
//! labelled header with a rule under it, and rows two lines tall (name, then
//! the work folder) separated by a blank line. Rows are sliced by hand instead
//! of being handed to `List`, so the mouse hitboxes and the scroll offset are
//! computed from the same geometry the cells are drawn with.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use serde::Deserialize;

use crate::core::models::RunPhase;
use crate::core::project::{Project, ProjectStore, default_name};
use crate::core::session::SessionManager;
use crate::core::timefmt;

use super::app::{App, HitAction, Hitbox, UiAction};
use super::card::{sanitize_terminal_text, truncate_display};

/// `o folder` is deliberately absent: the title already fills a 96-column
/// terminal, so the open-folder hint lives in the status bar (where it is
/// clickable and drops on narrow terminals) and in the help overlay.
const TITLE: &str = " Projects · Enter open · n new · r rename · p path · s settings · d delete · / filter · q quit ";

/// Column labels plus the rule drawn under them.
const HEADER_HEIGHT: u16 = 2;
/// Name line and folder line; the third line of a row is left blank so rows
/// read as blocks rather than as a wall of text.
const ROW_CONTENT_HEIGHT: u16 = 2;
const ROW_HEIGHT: u16 = ROW_CONTENT_HEIGHT + 1;
const COLUMN_SPACING: u16 = 2;
/// Below these widths the trailing columns are dropped rather than squeezing
/// the name and folder down to nothing.
const AGENTS_MIN_WIDTH: u16 = 64;
const OPENED_MIN_WIDTH: u16 = 82;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectCounts {
    pub todo: u32,
    pub in_progress: u32,
    pub review: u32,
    pub done: u32,
    pub sessions: u32,
    /// Tasks that are not consuming a running-agent slot: queued/retrying
    /// tasks and agents in a declared wait.
    pub paused: u32,
    pub review_unseen: u32,
    pub questions: u32,
}

impl ProjectCounts {
    pub fn total_tasks(self) -> u32 {
        self.todo + self.in_progress + self.review + self.done
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub project: Project,
    pub counts: ProjectCounts,
    pub missing: bool,
    /// What the list shows: the board's own `tui.name` when its settings carry
    /// one, otherwise the registry name. The registry name starts out as the
    /// folder basename, so without this a project renamed in its settings kept
    /// showing the folder it lives in.
    pub display_name: String,
}

impl ProjectRow {
    pub fn from_project(project: Project) -> Self {
        Self {
            counts: scan_counts(&project.data_root),
            missing: project.work_path_missing(),
            display_name: display_name(&project),
            project,
        }
    }
}

/// The name the projects list, the rename dialog and the delete confirmation
/// all speak: project settings first, the registry entry second.
pub fn display_name(project: &Project) -> String {
    crate::core::migrate::board_display_name(&project.data_root)
        .unwrap_or_else(|| project.name.clone())
}

/// Visible rows: optional pinned create-cwd action, then matching projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectListItem {
    CreateCwd { path: PathBuf },
    Project(ProjectRow),
}

pub fn load_rows(store: &ProjectStore) -> crate::core::error::Result<Vec<ProjectRow>> {
    Ok(store
        .list()?
        .into_iter()
        .map(ProjectRow::from_project)
        .collect())
}

/// Order the rows for the projects list, mirroring the board's task sorting.
///
/// - `name`: alphabetical by the name the list displays (default)
/// - `newest`: most recently created project first
/// - `smart`: rows with unread work (unseen review or open questions) first,
///   then rows with running agents, then most recently opened first within
///   every tier — the order of the visible "Last opened" column
/// - `smart_name`: the same tiers, but alphabetical by display name within
///   every tier
///
/// The id is the stable tie-break, and the pinned create-cwd row stays above
/// whatever this produces (the caller prepends it).
pub fn sort_rows(rows: &mut [ProjectRow], mode: &str) {
    use crate::core::global::{PROJECT_SORT_NEWEST, PROJECT_SORT_SMART, PROJECT_SORT_SMART_NAME};
    match crate::core::global::normalize_project_sort(mode) {
        PROJECT_SORT_NEWEST => rows.sort_by(|a, b| {
            b.project
                .created_at
                .cmp(&a.project.created_at)
                .then_with(|| a.project.id.cmp(&b.project.id))
        }),
        PROJECT_SORT_SMART => rows.sort_by(|a, b| {
            smart_rank(a)
                .cmp(&smart_rank(b))
                .then_with(|| {
                    b.project
                        .last_opened_at
                        .cmp(&a.project.last_opened_at)
                        .then_with(|| b.project.created_at.cmp(&a.project.created_at))
                })
                .then_with(|| a.project.id.cmp(&b.project.id))
        }),
        PROJECT_SORT_SMART_NAME => rows.sort_by(|a, b| {
            smart_rank(a)
                .cmp(&smart_rank(b))
                .then_with(|| {
                    a.display_name
                        .to_lowercase()
                        .cmp(&b.display_name.to_lowercase())
                })
                .then_with(|| a.project.id.cmp(&b.project.id))
        }),
        // `name`, and any unknown value, is the registry's own order
        // (alphabetical by display name with the id as tie-break).
        _ => rows.sort_by(|a, b| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
                .then_with(|| a.project.id.cmp(&b.project.id))
        }),
    }
}

/// Smart tier of a row: 0 unread, 1 running, 2 the rest. Lower sorts first.
fn smart_rank(row: &ProjectRow) -> u8 {
    let counts = row.counts;
    if counts.review_unseen > 0 || counts.questions > 0 {
        0
    } else if counts.sessions > 0 {
        1
    } else {
        2
    }
}

pub fn cwd_create_path(store: &ProjectStore, cwd: &Path) -> Option<PathBuf> {
    match store.resolve_from_cwd(cwd) {
        Ok(None) => Some(cwd.to_path_buf()),
        _ => None,
    }
}

pub fn scan_counts(data_root: &Path) -> ProjectCounts {
    let tasks = data_root.join(".kanban").join("tasks");
    let [todo, in_progress, review, done] =
        ["todo", "in_progress", "review", "done"].map(|s| scan_task_dir(&tasks.join(s)));
    let active_sessions = SessionManager::new(data_root).list_active_sessions();
    let now = timefmt::now();
    let waiting_sessions = active_sessions
        .iter()
        .filter(|session| session.wait_until.is_some_and(|deadline| now <= deadline))
        .count() as u32;
    ProjectCounts {
        todo: todo.files,
        in_progress: in_progress.files,
        review: review.files,
        done: done.files,
        sessions: active_sessions.len() as u32 - waiting_sessions,
        paused: todo.paused + in_progress.paused + review.paused + done.paused + waiting_sessions,
        review_unseen: review.unseen,
        questions: todo.questions + in_progress.questions + review.questions + done.questions,
    }
}

pub fn shorten_path(path: &Path) -> String {
    let display = path.display().to_string();
    let Ok(home) = std::env::var("HOME") else {
        return display;
    };
    if home.is_empty() {
        return display;
    }
    match display.strip_prefix(&home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        Some("") => "~".to_string(),
        _ => display,
    }
}

pub fn default_project_name(path: &Path) -> String {
    default_name(path)
}

pub fn format_opened(at: Option<NaiveDateTime>) -> String {
    at.map(|stamp| stamp.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "never".to_string())
}

/// One table column. The order here is the order on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    /// Unreviewed-work and open-question markers, left of the name.
    Flags,
    Name,
    Todo,
    Doing,
    Review,
    Done,
    Agents,
    Opened,
}

impl Column {
    fn header(self) -> &'static str {
        match self {
            Column::Flags => "",
            Column::Name => "Project",
            Column::Todo => "To Do",
            Column::Doing => "Doing",
            Column::Review => "Review",
            Column::Done => "Done",
            Column::Agents => "Agents",
            Column::Opened => "Last opened",
        }
    }

    /// Every column but the name is fixed width, which is what keeps the
    /// numbers under their labels no matter how long a name or path is.
    fn constraint(self) -> Constraint {
        match self {
            Column::Flags => Constraint::Length(2),
            Column::Name => Constraint::Min(18),
            Column::Todo | Column::Doing => Constraint::Length(5),
            Column::Review => Constraint::Length(6),
            Column::Done => Constraint::Length(4),
            Column::Agents => Constraint::Length(6),
            Column::Opened => Constraint::Length(16),
        }
    }

    fn alignment(self) -> Alignment {
        match self {
            Column::Flags | Column::Name => Alignment::Left,
            _ => Alignment::Right,
        }
    }
}

/// The widest column set `width` can hold. The counts are what the list is
/// read for, so the trailing columns go first when the terminal is narrow.
fn columns_for(width: u16) -> Vec<Column> {
    let mut columns = vec![
        Column::Flags,
        Column::Name,
        Column::Todo,
        Column::Doing,
        Column::Review,
        Column::Done,
    ];
    if width >= AGENTS_MIN_WIDTH {
        columns.push(Column::Agents);
    }
    if width >= OPENED_MIN_WIDTH {
        columns.push(Column::Opened);
    }
    columns
}

fn column_rects(columns: &[Column], area: Rect) -> Vec<Rect> {
    Layout::horizontal(columns.iter().map(|column| column.constraint()))
        .spacing(COLUMN_SPACING)
        .split(area)
        .to_vec()
}

pub fn render(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let items = app.visible_project_items();
    let block = Block::default()
        .title(TITLE)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.focus));
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("No projects. Press n to add this folder or another path.").block(block),
            area,
        );
        return;
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height <= HEADER_HEIGHT {
        return;
    }

    let columns = columns_for(inner.width);
    let rects = column_rects(&columns, inner);
    render_header(frame, app, &columns, &rects, area, inner);

    let body = Rect {
        y: inner.y + HEADER_HEIGHT,
        height: inner.height - HEADER_HEIGHT,
        ..inner
    };
    // The last visible row needs no separator line under it.
    let per_page = ((body.height + 1) / ROW_HEIGHT).max(1) as usize;
    let selected = app.project_selected.min(items.len() - 1);
    app.project_scroll = scroll_offset(app.project_scroll, selected, per_page, items.len());

    for (slot, index) in (app.project_scroll..items.len()).take(per_page).enumerate() {
        let top = body.y + slot as u16 * ROW_HEIGHT;
        let height = ROW_CONTENT_HEIGHT.min(body.bottom().saturating_sub(top));
        if height == 0 {
            break;
        }
        let row = Rect {
            x: inner.x,
            y: top,
            width: inner.width,
            height,
        };
        let focused = index == selected;
        let action = match &items[index] {
            ProjectListItem::CreateCwd { .. } => HitAction::Action(UiAction::CreateCwdProject),
            ProjectListItem::Project(_) => HitAction::FocusProject { index },
        };
        // The selection owns the strong background; the row under the mouse
        // gets a fainter one so preselection reads as a hint, not a second
        // selection.
        let background = if focused {
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
        match &items[index] {
            ProjectListItem::CreateCwd { path } => {
                render_create_row(frame, app, path, name_area(&rects, row));
            }
            ProjectListItem::Project(project) => {
                render_project_row(frame, app, project, &columns, &rects, row, focused);
            }
        }
    }
}

/// Keep the selected row on screen while leaving the offset alone otherwise,
/// so scrolling does not jump when rows are added, removed or filtered out.
fn scroll_offset(current: usize, selected: usize, per_page: usize, total: usize) -> usize {
    let mut offset = current.min(total.saturating_sub(per_page));
    if selected < offset {
        offset = selected;
    }
    if selected >= offset + per_page {
        offset = selected + 1 - per_page;
    }
    offset
}

fn render_header(
    frame: &mut Frame<'_>,
    app: &App,
    columns: &[Column],
    rects: &[Rect],
    area: Rect,
    inner: Rect,
) {
    let style = Style::default()
        .fg(app.theme.muted)
        .add_modifier(Modifier::BOLD);
    for (column, rect) in columns.iter().zip(rects) {
        frame.render_widget(
            Paragraph::new(Span::styled(column.header(), style)).alignment(column.alignment()),
            Rect { height: 1, ..*rect },
        );
    }
    let border = Style::default().fg(app.theme.border);
    let rule_y = inner.y + 1;
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize)).style(border),
        Rect {
            y: rule_y,
            height: 1,
            ..inner
        },
    );
    // Tie the rule into the surrounding block so the header reads as the top
    // of a table rather than as a stray line inside a list.
    let buffer = frame.buffer_mut();
    for (x, symbol) in [(area.x, "├"), (area.right().saturating_sub(1), "┤")] {
        if let Some(cell) = buffer.cell_mut((x, rule_y)) {
            cell.set_symbol(symbol).set_style(border);
        }
    }
}

/// The create-project row is one line of prose, not a set of cells: it starts
/// where the names start and runs to the end of the row.
fn name_area(rects: &[Rect], row: Rect) -> Rect {
    let x = rects.get(1).map_or(row.x, |name| name.x);
    Rect {
        x,
        width: row.right().saturating_sub(x),
        ..row
    }
}

fn render_create_row(frame: &mut Frame<'_>, app: &App, path: &Path, row: Rect) {
    let width = row.width.saturating_sub(2) as usize;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("+ ", Style::default().fg(app.theme.ok)),
            Span::raw(truncate_display(
                &format!(
                    "Create project for {}",
                    sanitize_terminal_text(&shorten_path(path))
                ),
                width,
            )),
        ])),
        row,
    );
}

fn render_project_row(
    frame: &mut Frame<'_>,
    app: &App,
    row: &ProjectRow,
    columns: &[Column],
    rects: &[Rect],
    area: Rect,
    focused: bool,
) {
    for (column, rect) in columns.iter().zip(rects) {
        frame.render_widget(
            Paragraph::new(project_cell(app, *column, row, rect.width, focused))
                .alignment(column.alignment()),
            Rect {
                y: area.y,
                height: area.height,
                ..*rect
            },
        );
    }
}

/// One cell of a project row: the name column carries both content lines, the
/// rest sit on the first line and leave the folder line clear.
fn project_cell(
    app: &App,
    column: Column,
    row: &ProjectRow,
    width: u16,
    focused: bool,
) -> Text<'static> {
    let counts = row.counts;
    match column {
        Column::Flags => Text::from(Line::from(vec![
            marker(counts.review_unseen > 0, '●', app.theme.warn),
            marker(counts.questions > 0, '?', app.theme.warn),
        ])),
        Column::Name => {
            let name_style = if focused {
                Style::default()
                    .fg(app.theme.focus)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let mut path_style = Style::default().fg(app.theme.muted);
            if row.missing {
                path_style = path_style.add_modifier(Modifier::CROSSED_OUT);
            }
            let name = truncate_display(&sanitize_terminal_text(&row.display_name), width as usize);
            let path = truncate_display(
                &sanitize_terminal_text(&shorten_path(&row.project.work_path)),
                width as usize,
            );
            Text::from(vec![
                Line::from(Span::styled(name, name_style)),
                Line::from(Span::styled(path, path_style)),
            ])
        }
        Column::Todo => count_cell(app, counts.todo, Style::default()),
        Column::Doing => count_cell(
            app,
            counts.in_progress,
            Style::default().fg(app.theme.focus),
        ),
        Column::Review => {
            let style = if counts.review_unseen > 0 {
                Style::default().fg(app.theme.warn)
            } else {
                Style::default().fg(app.theme.review)
            };
            count_cell(app, counts.review, style)
        }
        Column::Done => count_cell(app, counts.done, Style::default().fg(app.theme.ok)),
        Column::Agents => agent_cell(app, counts.sessions, counts.paused),
        Column::Opened => Text::from(Span::styled(
            format_opened(row.project.last_opened_at),
            Style::default().fg(app.theme.muted),
        )),
    }
}

fn marker(shown: bool, glyph: char, color: ratatui::style::Color) -> Span<'static> {
    if shown {
        Span::styled(glyph.to_string(), Style::default().fg(color))
    } else {
        Span::raw(" ")
    }
}

fn agent_cell(app: &App, running: u32, paused: u32) -> Text<'static> {
    let mut spans = Vec::new();
    if running > 0 {
        spans.push(Span::styled(
            format!("▶{running}"),
            Style::default().fg(app.theme.ok),
        ));
    }
    if paused > 0 {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("⏸{paused}"),
            Style::default().fg(app.theme.warn),
        ));
    }
    if spans.is_empty() {
        Text::from(Span::styled("·", Style::default().fg(app.theme.muted)))
    } else {
        Text::from(Line::from(spans))
    }
}

/// Zero reads as background noise, so it stays muted while real work does not.
fn count_cell(app: &App, count: u32, style: Style) -> Text<'static> {
    let style = if count == 0 {
        Style::default().fg(app.theme.muted)
    } else {
        style
    };
    Text::from(Span::styled(count.to_string(), style))
}

#[derive(Default)]
struct DirScan {
    files: u32,
    unseen: u32,
    questions: u32,
    paused: u32,
}

#[derive(Deserialize, Default)]
struct TaskFlags {
    #[serde(default)]
    review_unseen: bool,
    #[serde(default)]
    has_questions: bool,
    #[serde(default)]
    run_phase: Option<RunPhase>,
    #[serde(default)]
    restart_at: Option<serde_yaml_ng::Value>,
}

fn scan_task_dir(dir: &Path) -> DirScan {
    let Ok(entries) = fs::read_dir(dir) else {
        return DirScan::default();
    };
    let mut acc = DirScan::default();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("TASK-") || !name.ends_with(".md") {
            continue;
        }
        acc.files += 1;
        if let Ok(content) = fs::read_to_string(entry.path())
            && let Some(frontmatter) = content.split("---").nth(1)
            && let Ok(flags) = serde_yaml_ng::from_str::<TaskFlags>(frontmatter)
        {
            acc.unseen += u32::from(flags.review_unseen);
            acc.questions += u32::from(flags.has_questions);
            acc.paused +=
                u32::from(flags.run_phase == Some(RunPhase::Queued) || flags.restart_at.is_some());
        }
    }
    acc
}
