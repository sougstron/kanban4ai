//! Projects list screen: registered boards plus the optional create-for-cwd row.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use serde::Deserialize;

use crate::core::project::{Project, ProjectStore, default_name};
use crate::core::session::SessionManager;

use super::app::{App, HitAction, Hitbox, UiAction};
use super::card::{sanitize_terminal_text, truncate_display};

const TITLE: &str =
    " Projects · Enter open · n new · r rename · p path · d delete · / filter · q quit ";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectCounts {
    pub todo: u32,
    pub in_progress: u32,
    pub review: u32,
    pub done: u32,
    pub sessions: u32,
    pub review_unseen: u32,
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
}

impl ProjectRow {
    pub fn from_project(project: Project) -> Self {
        let counts = scan_counts(&project.data_root);
        let missing = project.work_path_missing();
        Self {
            project,
            counts,
            missing,
        }
    }
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

pub fn cwd_create_path(store: &ProjectStore, cwd: &Path) -> Option<PathBuf> {
    match store.resolve_from_cwd(cwd) {
        Ok(None) => Some(cwd.to_path_buf()),
        _ => None,
    }
}

pub fn scan_counts(data_root: &Path) -> ProjectCounts {
    let tasks = data_root.join(".kanban").join("tasks");
    let review_dir = tasks.join("review");
    ProjectCounts {
        todo: count_task_files(&tasks.join("todo")),
        in_progress: count_task_files(&tasks.join("in_progress")),
        review: count_task_files(&review_dir),
        done: count_task_files(&tasks.join("done")),
        sessions: SessionManager::new(data_root).list_active_sessions().len() as u32,
        review_unseen: count_review_unseen(&review_dir),
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

pub fn render(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let items = app.visible_project_items();
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("No projects. Press n to add this folder or another path.")
                .block(Block::default().title(TITLE).borders(Borders::ALL)),
            area,
        );
        return;
    }
    let selected = app.project_selected.min(items.len().saturating_sub(1));
    let list_items = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let line = item_line(app, item, area.width.saturating_sub(4));
            if matches!(item, ProjectListItem::CreateCwd { .. }) {
                app.hitboxes.push(Hitbox {
                    area: row_area(area, index, items.len()),
                    action: HitAction::Action(UiAction::CreateCwdProject),
                });
            } else {
                app.hitboxes.push(Hitbox {
                    area: row_area(area, index, items.len()),
                    action: HitAction::FocusProject { index },
                });
            }
            ListItem::new(line)
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(Some(selected));
    frame.render_stateful_widget(
        List::new(list_items)
            .block(
                Block::default()
                    .title(TITLE)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.theme.focus)),
            )
            .highlight_style(
                Style::default()
                    .fg(app.theme.focus)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut state,
    );
}

fn item_line(app: &App, item: &ProjectListItem, width: u16) -> Line<'static> {
    match item {
        ProjectListItem::CreateCwd { path } => Line::from(vec![
            Span::styled("+ ", Style::default().fg(app.theme.ok)),
            Span::raw(truncate_display(
                &format!(
                    "Create project for {}",
                    sanitize_terminal_text(&shorten_path(path))
                ),
                width.saturating_sub(2) as usize,
            )),
        ]),
        ProjectListItem::Project(row) => project_line(app, row, width),
    }
}

fn project_line(app: &App, row: &ProjectRow, width: u16) -> Line<'static> {
    let name = sanitize_terminal_text(&row.project.name);
    let path = shorten_path(&row.project.work_path);
    let counts = format!(
        "{}/{}/{}/{}",
        row.counts.todo, row.counts.in_progress, row.counts.review, row.counts.done
    );
    let sessions = if row.counts.sessions == 0 {
        String::new()
    } else {
        format!("  ▶{}", row.counts.sessions)
    };
    let opened = format!("  {}", format_opened(row.project.last_opened_at));
    let path_style = if row.missing {
        Style::default()
            .fg(app.theme.muted)
            .add_modifier(Modifier::CROSSED_OUT)
    } else {
        Style::default().fg(app.theme.muted)
    };
    let unseen = row.counts.review_unseen > 0;
    let prefix_width = if unseen { 2 } else { 0 };
    let path_width = (width as usize)
        .saturating_sub(
            name.len()
                .saturating_add(counts.len())
                .saturating_add(18 + prefix_width),
        )
        .max(8);
    let mut spans = Vec::new();
    if unseen {
        spans.push(Span::styled("● ", Style::default().fg(app.theme.warn)));
    }
    spans.push(Span::raw(format!("{:<20} ", truncate_display(&name, 20))));
    spans.push(Span::styled(
        format!(
            "{:<path_width$} ",
            truncate_display(&sanitize_terminal_text(&path), path_width)
        ),
        path_style,
    ));
    spans.push(Span::raw(counts));
    spans.push(Span::styled(sessions, Style::default().fg(app.theme.ok)));
    spans.push(Span::styled(opened, Style::default().fg(app.theme.muted)));
    Line::from(spans)
}

fn row_area(list: Rect, index: usize, total: usize) -> Rect {
    let inner_y = list.y.saturating_add(1);
    let inner_h = list.height.saturating_sub(2);
    let row_y = inner_y.saturating_add(index as u16);
    if row_y >= inner_y.saturating_add(inner_h) || index >= total {
        return Rect::new(list.x, list.y, 0, 0);
    }
    Rect {
        x: list.x.saturating_add(1),
        y: row_y,
        width: list.width.saturating_sub(2),
        height: 1,
    }
}

fn count_task_files(dir: &Path) -> u32 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("TASK-") && name.ends_with(".md")
        })
        .count() as u32
}

/// Count review tasks whose `review_unseen` frontmatter flag is set: the agent
/// moved them into Review and the user has not yet opened the detail. Drives
/// the project-level notifier dot in the projects list.
fn count_review_unseen(review_dir: &Path) -> u32 {
    #[derive(Deserialize)]
    struct Flag {
        #[serde(default)]
        review_unseen: bool,
    }
    let Ok(entries) = fs::read_dir(review_dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            (name.starts_with("TASK-") && name.ends_with(".md")).then(|| entry.path())
        })
        .filter_map(|path| {
            let content = fs::read_to_string(&path).ok()?;
            let mut parts = content.splitn(3, "---");
            let frontmatter = parts.nth(1)?;
            serde_yaml_ng::from_str::<Flag>(frontmatter)
                .ok()
                .map(|flag| flag.review_unseen)
        })
        .filter(|unseen| *unseen)
        .count() as u32
}
