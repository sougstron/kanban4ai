use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use serde_yaml_ng::{Mapping, Value};
use tui_textarea::TextArea;

use crate::core::config::Config;
use crate::core::error::Result;
use crate::core::models::{Message, Task, TaskStatus};
use crate::core::operations::{Operations, TaskPatch};
use crate::core::session::SessionManager;
use crate::core::storage::NewTask;
use crate::core::thread::ThreadManager;

use super::card::sanitize_terminal_text;
use super::card::truncate_display;
use super::dialogs::{DialogField, Modal, ModalState, QuestionChoice, SelectOption};
use super::image;
use super::search::SearchState;
use super::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Board,
    Detail,
    Sessions,
    Archive,
    Help,
}

#[derive(Debug, Clone)]
pub struct TuiSettings {
    pub card_height_lines: u16,
    pub card_line_max_symbols: usize,
    pub max_tasks_per_column: usize,
    pub refresh_interval: Duration,
    pub theme_name: String,
}

#[derive(Debug, Clone)]
pub struct BoardColumn {
    pub id: String,
    pub name: String,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone)]
pub struct BoardSnapshot {
    pub columns: Vec<BoardColumn>,
    pub fingerprint: (u64, u128),
    pub loaded_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardHitbox {
    pub column: usize,
    pub card: usize,
    pub area: Rect,
}

#[derive(Clone)]
pub struct DetailState {
    pub task_id: String,
    pub task: Option<Task>,
    pub messages: Vec<Message>,
    pub scroll: u16,
    pub review_edits: TextArea<'static>,
    pub review_editing: bool,
}

pub struct App {
    pub ops: Operations,
    pub project_path: PathBuf,
    pub screen: Screen,
    pub board: BoardSnapshot,
    pub settings: TuiSettings,
    pub theme: Theme,
    pub focused_column: usize,
    pub focused_card: usize,
    pub column_offsets: Vec<usize>,
    pub search: SearchState,
    pub modal: Option<ModalState>,
    pub detail: Option<DetailState>,
    pub session_selected: usize,
    pub archive_selected: usize,
    pub should_quit: bool,
    pub status: String,
    pub card_hitboxes: Vec<CardHitbox>,
    pending_attach: Option<String>,
    pending_fs_reload: bool,
    fs_change_generation: u64,
}

impl App {
    pub fn new(project_path: &Path) -> Result<Self> {
        let ops = Operations::new(project_path);
        let settings = load_settings(&ops)?;
        let theme = Theme::named(&settings.theme_name);
        let board = BoardSnapshot::load(&ops)?;
        let column_offsets = vec![0; board.columns.len()];
        Ok(Self {
            ops,
            project_path: project_path.to_path_buf(),
            screen: Screen::Board,
            board,
            settings,
            theme,
            focused_column: 0,
            focused_card: 0,
            column_offsets,
            search: SearchState::default(),
            modal: None,
            detail: None,
            session_selected: 0,
            archive_selected: 0,
            should_quit: false,
            status: "TUI ready".to_string(),
            card_hitboxes: Vec::new(),
            pending_attach: None,
            pending_fs_reload: false,
            fs_change_generation: 0,
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.handle_modal_key(key)? {
            return Ok(());
        }
        if self.search.active {
            return self.handle_search_key(key);
        }
        if self.screen == Screen::Detail && key.code == KeyCode::Tab {
            if let Some(detail) = self.detail.as_mut() {
                detail.review_editing = !detail.review_editing;
                self.status = if detail.review_editing {
                    "Review editor focused".to_string()
                } else {
                    "Thread scrolling focused".to_string()
                };
            }
            return Ok(());
        }
        if self.screen == Screen::Detail && self.is_review_edit_key(key) {
            self.input_review_edits(key);
            return Ok(());
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('q') | KeyCode::Esc, _) => {
                if self.screen != Screen::Board {
                    self.screen = Screen::Board;
                    self.detail = None;
                } else {
                    self.should_quit = true;
                }
            }
            (KeyCode::Char('?'), _) => self.screen = Screen::Help,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.should_quit = true,
            (KeyCode::Char('t'), KeyModifiers::CONTROL) => self.cycle_theme()?,
            (KeyCode::Char('s'), KeyModifiers::CONTROL) if self.screen == Screen::Detail => {
                self.save_review_edits()?
            }
            (KeyCode::Char('/'), _) => self.search.active = true,
            (KeyCode::Enter, _) => self.open_focused_detail()?,
            (KeyCode::Char('n'), _) if self.screen == Screen::Board => self.open_new_dialog(),
            (KeyCode::Char('e'), _) if self.screen == Screen::Board => self.open_edit_dialog(),
            (KeyCode::Char('m'), _) if self.screen == Screen::Board => self.open_move_dialog(),
            (KeyCode::Char('s'), _) if self.screen == Screen::Board => self.open_delegate_dialog(),
            (KeyCode::Char('w'), _) if self.screen == Screen::Board => self.open_answer_dialog()?,
            (KeyCode::Char('d'), _) | (KeyCode::Delete, _) | (KeyCode::Backspace, _)
                if self.screen == Screen::Board =>
            {
                self.open_delete_dialog();
            }
            (KeyCode::Char('r'), _) if self.screen == Screen::Board => {
                self.recover_focused_task()?
            }
            (KeyCode::Char('a'), _) => self.open_archive()?,
            (KeyCode::Char('l'), _) => self.open_sessions(),
            (KeyCode::Left, _) | (KeyCode::BackTab, _) if self.screen == Screen::Board => {
                self.focus_prev_column()
            }
            (KeyCode::Right, _) | (KeyCode::Tab, _) if self.screen == Screen::Board => {
                self.focus_next_column()
            }
            (KeyCode::Up, _) => self.focus_up(),
            (KeyCode::Down, _) => self.focus_down(),
            (KeyCode::PageUp, _) => self.page_up(),
            (KeyCode::PageDown, _) => self.page_down(),
            (KeyCode::Home, _) => self.home(),
            (KeyCode::End, _) => self.end(),
            _ => {}
        }
        Ok(())
    }

    fn is_review_edit_key(&self, key: KeyEvent) -> bool {
        if !self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.review_editing)
        {
            return false;
        }
        matches!(
            key.code,
            KeyCode::Char(_)
                | KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Enter
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
        ) && !matches!(key.modifiers, KeyModifiers::CONTROL | KeyModifiers::ALT)
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if self.modal.is_some() || self.screen == Screen::Help {
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click_card(mouse.column, mouse.row)?,
            MouseEventKind::ScrollUp => self.page_up(),
            MouseEventKind::ScrollDown => self.page_down(),
            _ => {}
        }
        Ok(())
    }

    fn click_card(&mut self, x: u16, y: u16) -> Result<()> {
        let Some(hitbox) = self
            .card_hitboxes
            .iter()
            .find(|hitbox| contains(hitbox.area, x, y))
            .copied()
        else {
            return Ok(());
        };
        let already_focused = self.screen == Screen::Board
            && self.focused_column == hitbox.column
            && self.focused_card == hitbox.card;
        self.screen = Screen::Board;
        self.focused_column = hitbox.column;
        self.focused_card = hitbox.card;
        self.ensure_focused_visible();
        if already_focused {
            self.open_focused_detail()?;
        }
        Ok(())
    }

    pub fn note_fs_changed(&mut self) -> u64 {
        self.pending_fs_reload = true;
        self.fs_change_generation = self.fs_change_generation.wrapping_add(1);
        self.fs_change_generation
    }

    pub fn reload_debounced_change(&mut self, generation: u64) -> Result<()> {
        if !self.pending_fs_reload || generation != self.fs_change_generation {
            return Ok(());
        }
        self.pending_fs_reload = false;
        self.reload_if_changed()?;
        Ok(())
    }

    pub fn stop_after_input_error(&mut self, message: String) {
        self.status = message;
        self.should_quit = true;
    }

    pub fn reload_if_changed(&mut self) -> Result<()> {
        let fingerprint = self.ops.storage.tui_fingerprint();
        if fingerprint != self.board.fingerprint {
            self.board = BoardSnapshot::load(&self.ops)?;
            if let Some(detail) = self.detail.as_ref() {
                let task_id = detail.task_id.clone();
                self.load_detail(&task_id)?;
            }
            self.clamp_focus();
            self.status = "Board refreshed from disk".to_string();
        }
        Ok(())
    }

    fn focus_prev_column(&mut self) {
        self.focused_column = self.focused_column.saturating_sub(1);
        self.clamp_focus();
    }

    fn focus_next_column(&mut self) {
        if self.focused_column + 1 < self.board.columns.len() {
            self.focused_column += 1;
        }
        self.clamp_focus();
    }

    fn focus_prev_card(&mut self) {
        self.focused_card = self.focused_card.saturating_sub(1);
        self.ensure_focused_visible();
    }

    fn focus_next_card(&mut self) {
        let len = self.visible_tasks_for_column(self.focused_column).len();
        if self.focused_card + 1 < len {
            self.focused_card += 1;
        }
        self.ensure_focused_visible();
    }

    fn focus_up(&mut self) {
        match self.screen {
            Screen::Board => self.focus_prev_card(),
            Screen::Detail => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.scroll = detail.scroll.saturating_sub(1);
                }
            }
            Screen::Sessions => self.session_selected = self.session_selected.saturating_sub(1),
            Screen::Archive => self.archive_selected = self.archive_selected.saturating_sub(1),
            Screen::Help => {}
        }
    }

    fn focus_down(&mut self) {
        match self.screen {
            Screen::Board => self.focus_next_card(),
            Screen::Detail => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.scroll = detail.scroll.saturating_add(1);
                }
            }
            Screen::Sessions => {
                let len = SessionManager::new(&self.project_path)
                    .list_active_sessions()
                    .len();
                self.session_selected = next_index(self.session_selected, len);
            }
            Screen::Archive => {
                let len = self
                    .ops
                    .list_archived_tasks(None)
                    .map_or(0, |tasks| tasks.len());
                self.archive_selected = next_index(self.archive_selected, len);
            }
            Screen::Help => {}
        }
    }

    fn page_up(&mut self) {
        match self.screen {
            Screen::Board => {
                self.focused_card = self.focused_card.saturating_sub(5);
                self.ensure_focused_visible();
            }
            Screen::Detail => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.scroll = detail.scroll.saturating_sub(5);
                }
            }
            _ => self.focus_up(),
        }
    }

    fn page_down(&mut self) {
        match self.screen {
            Screen::Board => {
                let len = self.visible_tasks_for_column(self.focused_column).len();
                self.focused_card = self
                    .focused_card
                    .saturating_add(5)
                    .min(len.saturating_sub(1));
                self.ensure_focused_visible();
            }
            Screen::Detail => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.scroll = detail.scroll.saturating_add(5);
                }
            }
            _ => self.focus_down(),
        }
    }

    fn home(&mut self) {
        match self.screen {
            Screen::Board => {
                self.focused_card = 0;
                self.ensure_focused_visible();
            }
            Screen::Detail => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.scroll = 0;
                }
            }
            Screen::Sessions => self.session_selected = 0,
            Screen::Archive => self.archive_selected = 0,
            Screen::Help => {}
        }
    }

    fn end(&mut self) {
        match self.screen {
            Screen::Board => self.focus_last_card(),
            Screen::Sessions => {
                self.session_selected = SessionManager::new(&self.project_path)
                    .list_active_sessions()
                    .len()
                    .saturating_sub(1);
            }
            Screen::Archive => {
                self.archive_selected = self
                    .ops
                    .list_archived_tasks(None)
                    .map_or(0, |tasks| tasks.len().saturating_sub(1));
            }
            _ => {}
        }
    }

    fn focus_last_card(&mut self) {
        self.focused_card = self
            .board
            .columns
            .get(self.focused_column)
            .map(|_| self.visible_tasks_for_column(self.focused_column).len())
            .and_then(|len| len.checked_sub(1))
            .unwrap_or(0);
        self.ensure_focused_visible();
    }

    pub fn clamp_focus(&mut self) {
        if self.board.columns.is_empty() {
            self.focused_column = 0;
            self.focused_card = 0;
            return;
        }
        self.focused_column = self.focused_column.min(self.board.columns.len() - 1);
        let len = self.visible_tasks_for_column(self.focused_column).len();
        self.focused_card = self.focused_card.min(len.saturating_sub(1));
        self.column_offsets.resize(self.board.columns.len(), 0);
        self.ensure_focused_visible();
    }

    pub fn visible_tasks_for_column(&self, column_index: usize) -> Vec<&Task> {
        let Some(column) = self.board.columns.get(column_index) else {
            return Vec::new();
        };
        let query = self.search.text().to_lowercase();
        column
            .tasks
            .iter()
            .filter(|task| {
                query.is_empty()
                    || task.id.to_lowercase().contains(&query)
                    || task.title.to_lowercase().contains(&query)
                    || task.description.to_lowercase().contains(&query)
            })
            .take(self.settings.max_tasks_per_column)
            .collect()
    }

    pub fn focused_task(&self) -> Option<&Task> {
        self.visible_tasks_for_column(self.focused_column)
            .get(self.focused_card)
            .copied()
    }

    fn focused_task_id(&self) -> Option<String> {
        self.focused_task().map(|task| task.id.clone())
    }

    fn ensure_focused_visible(&mut self) {
        let Some(offset) = self.column_offsets.get_mut(self.focused_column) else {
            return;
        };
        if self.focused_card < *offset {
            *offset = self.focused_card;
        }
        if self.focused_card > *offset {
            *offset = self.focused_card;
        }
    }

    fn open_focused_detail(&mut self) -> Result<()> {
        if self.screen == Screen::Sessions {
            let sessions = SessionManager::new(&self.project_path).list_active_sessions();
            if let Some(session) = sessions.get(self.session_selected) {
                self.pending_attach = Some(session.id.clone());
                self.status = format!("Attaching to {}", session.id);
            } else {
                self.status = "No active session selected".to_string();
            }
            return Ok(());
        }
        if self.screen == Screen::Archive {
            let tasks = self.ops.list_archived_tasks(None)?;
            if let Some(task) = tasks.get(self.archive_selected) {
                let task_id = task.id.clone();
                self.load_detail(&task_id)?;
                self.screen = Screen::Detail;
            }
            return Ok(());
        }
        if let Some(task_id) = self.focused_task_id() {
            self.load_detail(&task_id)?;
            self.screen = Screen::Detail;
        }
        Ok(())
    }

    fn load_detail(&mut self, task_id: &str) -> Result<()> {
        let task = self.ops.get_task(task_id)?;
        let messages = ThreadManager::new(&self.project_path)?
            .load(task_id)?
            .messages;
        let review_edits = TextArea::from(
            task.as_ref()
                .map(|task| lines_or_empty(&task.review_edits))
                .unwrap_or_else(|| vec![String::new()]),
        );
        self.detail = Some(DetailState {
            task_id: task_id.to_string(),
            task,
            messages,
            scroll: 0,
            review_edits,
            review_editing: true,
        });
        Ok(())
    }

    fn open_new_dialog(&mut self) {
        let mut modal = ModalState::new(Modal::NewTask);
        self.populate_task_form_options(&mut modal, None);
        self.modal = Some(modal);
    }

    fn open_edit_dialog(&mut self) {
        let Some(task) = self.focused_task().cloned() else {
            self.status = "No task selected".to_string();
            return;
        };
        let mut modal = ModalState::for_task(
            Modal::EditTask {
                task_id: task.id.clone(),
            },
            &task,
        );
        self.populate_task_form_options(&mut modal, Some(&task.id));
        self.modal = Some(modal);
    }

    fn open_move_dialog(&mut self) {
        if let Some(task) = self.focused_task().cloned() {
            let mut modal = ModalState::new(Modal::MoveTask {
                task_id: task.id.clone(),
            });
            let options = self
                .board
                .columns
                .iter()
                .map(|column| SelectOption {
                    label: format!("{} ({})", column.name, column.id),
                    value: Some(column.id.clone()),
                })
                .collect();
            modal.set_status_options(options, Some(task.status.as_str()));
            self.modal = Some(modal);
        }
    }

    fn open_delete_dialog(&mut self) {
        if let Some(task_id) = self.focused_task_id() {
            self.modal = Some(ModalState::new(Modal::DeleteConfirm { task_id }));
        }
    }

    fn open_delegate_dialog(&mut self) {
        if let Some(task_id) = self.focused_task_id() {
            self.modal = Some(ModalState::new(Modal::DelegateConfirm { task_id }));
        }
    }

    fn open_answer_dialog(&mut self) -> Result<()> {
        let Some(task_id) = self.focused_task_id() else {
            return Ok(());
        };
        let questions = self.ops.list_open_messages(&task_id)?;
        if questions.is_empty() {
            self.status = format!("{task_id} has no open questions");
            return Ok(());
        }
        let choices = questions
            .into_iter()
            .map(|question| QuestionChoice {
                message_id: question.id,
                body: question.body,
                variants: question.variants,
            })
            .collect();
        self.modal = Some(ModalState::new(Modal::AnswerQuestion {
            task_id,
            questions: choices,
        }));
        Ok(())
    }

    fn populate_task_form_options(&self, modal: &mut ModalState, edited_task_id: Option<&str>) {
        let Ok(config) = self.ops.config.load() else {
            return;
        };
        let backend_options = config
            .agents
            .keys()
            .filter_map(|key| key.as_str())
            .map(|backend| SelectOption {
                label: backend.to_string(),
                value: Some(backend.to_string()),
            })
            .collect::<Vec<_>>();
        modal.set_backend_options(backend_options);
        let backend = selected_backend(&config.auto_launch, modal);
        let backend_settings = config
            .agents
            .get(Value::String(backend.clone()))
            .and_then(Value::as_mapping);
        let model_options = options_from_sequence(backend_settings, "models")
            .or_else(|| options_from_sequence(Some(&config.auto_launch), "models"))
            .unwrap_or_default();
        modal.set_model_options(model_options);
        let agent_options = optional_empty("Default agent")
            .into_iter()
            .chain(options_from_sequence(backend_settings, "agent_options").unwrap_or_default())
            .collect();
        modal.set_agent_options(agent_options);
        let mut chain_options = vec![SelectOption {
            label: "No chain".to_string(),
            value: None,
        }];
        let mut tasks = self
            .board
            .columns
            .iter()
            .flat_map(|column| column.tasks.iter())
            .filter(|task| Some(task.id.as_str()) != edited_task_id)
            .collect::<Vec<_>>();
        tasks.sort_by_key(|task| Reverse(task.created_at));
        chain_options.extend(tasks.into_iter().map(|task| SelectOption {
            label: format!(
                "{} ({})",
                task.id,
                truncate_display(&sanitize_terminal_text(&task.title), 25)
            ),
            value: Some(task.id.clone()),
        }));
        modal.set_chain_options(chain_options);
    }

    fn recover_focused_task(&mut self) -> Result<()> {
        let Some(task_id) = self.focused_task_id() else {
            return Ok(());
        };
        let Some(_) = self.ops.recover_task(&task_id)? else {
            self.status = format!("Task {task_id} not found");
            return Ok(());
        };
        self.board = BoardSnapshot::load(&self.ops)?;
        self.clamp_focus();
        self.status = format!("Task {task_id} recovered to To Do");
        Ok(())
    }

    fn open_archive(&mut self) -> Result<()> {
        self.archive_selected = 0;
        self.screen = Screen::Archive;
        self.status = format!(
            "Archive: {} tasks",
            self.ops.list_archived_tasks(None)?.len()
        );
        Ok(())
    }

    fn open_sessions(&mut self) {
        self.session_selected = 0;
        self.screen = Screen::Sessions;
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.search.active = false,
            KeyCode::Enter => {
                self.search.active = false;
                self.focused_card = 0;
                self.clamp_focus();
            }
            _ => {
                input_single_line(&mut self.search.query, key);
                self.focused_card = 0;
                self.clamp_focus();
            }
        }
        Ok(())
    }

    fn input_review_edits(&mut self, key: KeyEvent) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        detail.review_edits.input(key);
    }

    fn save_review_edits(&mut self) -> Result<()> {
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        let text = detail.review_edits.lines().join("\n");
        self.ops.set_review_edits(&detail.task_id, &text)?;
        let task_id = detail.task_id.clone();
        let status = detail.task.as_ref().map(|task| task.status);
        if status == Some(TaskStatus::Review) {
            self.status = if self.ops.rerun_review_task(&task_id, None)?.is_some() {
                format!("Saved review edits and re-ran {task_id}")
            } else {
                format!("Saved edits for {task_id}, but agent launch failed")
            };
            self.board = BoardSnapshot::load(&self.ops)?;
            self.screen = Screen::Board;
            self.detail = None;
            self.clamp_focus();
        } else {
            self.load_detail(&task_id)?;
            self.status = format!("Saved review edits for {task_id}");
        }
        Ok(())
    }

    pub fn take_attach_request(&mut self) -> Option<String> {
        self.pending_attach.take()
    }

    pub fn finish_attach(&mut self, session_id: &str, attached: bool) {
        self.status = if attached {
            format!("Detached from {session_id}")
        } else {
            format!("Could not attach to {session_id}")
        };
    }

    fn refresh_backend_options(&self, modal: &mut ModalState) {
        let Ok(config) = self.ops.config.load() else {
            return;
        };
        let backend = selected_backend(&config.auto_launch, modal);
        let backend_settings = config
            .agents
            .get(Value::String(backend))
            .and_then(Value::as_mapping);
        let models = optional_empty("Default model")
            .into_iter()
            .chain(
                options_from_sequence(backend_settings, "models")
                    .or_else(|| options_from_sequence(Some(&config.auto_launch), "models"))
                    .unwrap_or_default(),
            )
            .collect();
        modal.set_model_options(models);
        let agents = optional_empty("Default agent")
            .into_iter()
            .chain(options_from_sequence(backend_settings, "agent_options").unwrap_or_default())
            .collect();
        modal.set_agent_options(agents);
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(mut modal) = self.modal.take() else {
            return Ok(false);
        };
        match key.code {
            KeyCode::Esc => {
                self.status = "Dialog cancelled".to_string();
                return Ok(true);
            }
            KeyCode::Tab => modal.next_field(),
            KeyCode::BackTab => modal.prev_field(),
            KeyCode::Enter if modal.submit_on_enter() => {
                self.submit_modal(modal)?;
                return Ok(true);
            }
            KeyCode::Char('v') if key.modifiers == KeyModifiers::CONTROL => {
                if modal.active_field() == DialogField::Description {
                    match image::paste_image_markdown(&self.project_path) {
                        Ok(markdown) => {
                            modal.active_textarea_mut().insert_str(&markdown);
                        }
                        Err(err) => self.status = format!("Image paste failed: {err}"),
                    }
                }
            }
            _ => modal.input(key),
        }
        if modal.active_field() == DialogField::Backend {
            self.refresh_backend_options(&mut modal);
        }
        self.modal = Some(modal);
        Ok(true)
    }

    fn submit_modal(&mut self, modal: ModalState) -> Result<()> {
        match modal.modal.clone() {
            Modal::NewTask => {
                let title = modal.title_text();
                if title.trim().is_empty() {
                    self.status = "Task title cannot be empty".to_string();
                    self.modal = Some(modal);
                    return Ok(());
                }
                let task = self.ops.create_task(NewTask {
                    title,
                    description: modal.description_text(),
                    ai_model: modal.model_text(),
                    agent_backend: modal.backend_text(),
                    agent_name: modal.agent_text(),
                    interactive: modal.interactive,
                    chained_to: modal.chain_text(),
                })?;
                self.board = BoardSnapshot::load(&self.ops)?;
                self.clamp_focus();
                self.status = format!("Created {}", task.id);
            }
            Modal::EditTask { task_id } => {
                let updated = self.ops.update_task(
                    &task_id,
                    TaskPatch {
                        title: Some(modal.title_text()),
                        description: Some(modal.description_text()),
                        ai_model: Some(modal.model_text()),
                        agent_backend: Some(modal.backend_text()),
                        agent_name: Some(modal.agent_text()),
                        interactive: Some(modal.interactive),
                        chained_to: Some(modal.chain_text()),
                        ..Default::default()
                    },
                )?;
                self.board = BoardSnapshot::load(&self.ops)?;
                self.status = if updated.is_some() {
                    format!("Updated {task_id}")
                } else {
                    format!("Task {task_id} not found")
                };
            }
            Modal::MoveTask { task_id } => {
                let target = modal.target_text();
                if target.trim().is_empty() {
                    self.status = "Move target cannot be empty".to_string();
                    self.modal = Some(modal);
                    return Ok(());
                }
                self.ops.move_task(&task_id, &target, false)?;
                self.board = BoardSnapshot::load(&self.ops)?;
                self.clamp_focus();
                self.status = format!("Moved {task_id} to {target}");
            }
            Modal::DeleteConfirm { task_id } => {
                if modal.confirmed() {
                    self.ops.abandon_task(&task_id)?;
                    self.board = BoardSnapshot::load(&self.ops)?;
                    self.clamp_focus();
                    self.status = format!("Deleted {task_id}");
                } else {
                    self.status = "Delete cancelled".to_string();
                }
            }
            Modal::DelegateConfirm { task_id } => {
                if modal.confirmed() {
                    let session_id = format!(
                        "ses-tui-{}",
                        crate::core::timefmt::now().and_utc().timestamp()
                    );
                    self.ops.take_task(&task_id, &session_id, true)?;
                    self.board = BoardSnapshot::load(&self.ops)?;
                    self.clamp_focus();
                    self.status = format!("Delegated {task_id} as {session_id}");
                } else {
                    self.status = "Delegate cancelled".to_string();
                }
            }
            Modal::AnswerQuestion { task_id, .. } => {
                let answer = modal.answer_text();
                if answer.trim().is_empty() {
                    self.status = "Answer cannot be empty".to_string();
                    self.modal = Some(modal);
                    return Ok(());
                }
                let question_ref = modal.selected_question_ref().ok_or_else(|| {
                    crate::core::error::KanbanError::Invalid("No question selected".to_string())
                })?;
                self.ops.answer_question(&task_id, question_ref, &answer)?;
                self.board = BoardSnapshot::load(&self.ops)?;
                self.status = format!("Answered question on {task_id}");
            }
        }
        Ok(())
    }

    fn cycle_theme(&mut self) -> Result<()> {
        let next = Theme::next_name(&self.settings.theme_name).to_string();
        let mut config = self.ops.config.load()?;
        config.tui.insert(
            Value::String("theme".to_string()),
            Value::String(next.clone()),
        );
        let config_writer = Config::new(&self.project_path);
        if fs::symlink_metadata(&config_writer.config_file)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            self.status = "Refusing to save theme through symlinked config.yaml".to_string();
            return Ok(());
        }
        config_writer.save(&config)?;
        self.settings.theme_name = next.clone();
        self.theme = Theme::named(&next);
        self.status = format!("Theme switched to {next}");
        Ok(())
    }
}

impl BoardSnapshot {
    pub fn load(ops: &Operations) -> Result<Self> {
        let config = ops.config.load()?;
        let ids = config.column_ids();
        let names = config.column_names();
        let mut grouped = ids
            .iter()
            .map(|id| (id.clone(), Vec::new()))
            .collect::<BTreeMap<_, _>>();
        for task in ops.list_tasks(None, None, "created", "asc")? {
            grouped
                .entry(task.status.as_str().to_string())
                .or_default()
                .push(task);
        }
        let columns = ids
            .iter()
            .enumerate()
            .map(|(index, id)| BoardColumn {
                id: id.clone(),
                name: names.get(index).cloned().unwrap_or_else(|| id.clone()),
                tasks: grouped.remove(id).unwrap_or_default(),
            })
            .collect();
        Ok(Self {
            columns,
            fingerprint: ops.storage.tui_fingerprint(),
            loaded_at: Instant::now(),
        })
    }
}

fn next_index(current: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        current.saturating_add(1).min(len - 1)
    }
}

fn lines_or_empty(text: &str) -> Vec<String> {
    let lines = text.lines().map(sanitize_terminal_text).collect::<Vec<_>>();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn input_single_line(textarea: &mut TextArea<'static>, key: KeyEvent) {
    if matches!(key.code, KeyCode::Enter) {
        return;
    }
    textarea.input(key);
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn optional_empty(label: &str) -> Vec<SelectOption> {
    vec![SelectOption {
        label: label.to_string(),
        value: None,
    }]
}

fn options_from_sequence(mapping: Option<&Mapping>, key: &str) -> Option<Vec<SelectOption>> {
    let values = mapping?
        .get(Value::String(key.to_string()))?
        .as_sequence()?;
    Some(
        values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| SelectOption {
                label: value.to_string(),
                value: Some(value.to_string()),
            })
            .collect(),
    )
}

fn selected_backend(auto_launch: &Mapping, modal: &ModalState) -> String {
    modal
        .backend_text()
        .or_else(|| {
            auto_launch
                .get("default_agent")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "opencode".to_string())
}

fn load_settings(ops: &Operations) -> Result<TuiSettings> {
    let config = ops.config.load()?;
    let card_height_lines = tui_int(&config.tui, "card_height_lines", 4).max(1) as u16;
    Ok(TuiSettings {
        card_height_lines,
        card_line_max_symbols: tui_int(&config.tui, "card_line_max_symbols", 40).max(1) as usize,
        max_tasks_per_column: tui_int(&config.tui, "max_tasks_per_column", 100).max(1) as usize,
        refresh_interval: Duration::from_secs(
            ops.config.get_threshold("tui_refresh_interval")?.max(1) as u64,
        ),
        theme_name: tui_string(&config.tui, "theme", "dark"),
    })
}

fn tui_int(map: &Mapping, key: &str, default: i64) -> i64 {
    map.get(Value::String(key.to_string()))
        .and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.trim().parse().ok(),
            Value::Bool(value) => Some(i64::from(*value)),
            _ => None,
        })
        .unwrap_or(default)
}

fn tui_string(map: &Mapping, key: &str, default: &str) -> String {
    map.get(Value::String(key.to_string()))
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}
