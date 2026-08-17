use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui_textarea::{TextArea, WrapMode};

use crate::core::models::Task;
use crate::core::operations::QuestionRef;

use super::app::{App, HitAction, Hitbox};
use super::card::{sanitize_paste_text, sanitize_terminal_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    NewTask {
        target_status: Option<String>,
    },
    EditTask {
        task_id: String,
    },
    MoveTask {
        task_id: String,
    },
    DeleteConfirm {
        task_id: String,
    },
    RevertConfirm {
        task_id: String,
    },
    BulkConfirm {
        action: BulkAction,
        task_ids: Vec<String>,
    },
    KillSessionConfirm {
        session_id: String,
    },
    RestoreConfirm {
        task_id: String,
    },
    AddMessage {
        task_id: String,
    },
    AnswerQuestion {
        task_id: String,
        questions: Vec<QuestionChoice>,
    },
    Settings,
    /// Machine-wide settings (`<store>/config.yaml`), edited from the
    /// projects list where no board context exists.
    GlobalSettings,
    NewProject,
    RenameProject {
        id: String,
    },
    SetProjectPath {
        id: String,
    },
    DeleteProject {
        id: String,
        name: String,
        task_count: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkAction {
    ArchiveAllDone,
    MarkReviewDone,
}

/// Kinds offered by the Add-message dialog, indexed by `kind_selected`.
pub const MESSAGE_KIND_OPTIONS: [&str; 2] = ["context", "suggestion"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub label: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionChoice {
    pub message_id: String,
    pub body: String,
    pub variants: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogField {
    Title,
    Description,
    Backend,
    Model,
    Effort,
    Agent,
    ChainTo,
    Interactive,
    TargetStatus,
    MessageKind,
    Question,
    Variant,
    Answer,
    Theme,
    TaskSort,
    EscapeToProjects,
    ProjectSort,
    Confirm,
    Cancel,
    PurgeData,
}

const TASK_FORM_FIELDS: [DialogField; 8] = [
    DialogField::Title,
    DialogField::Description,
    DialogField::Backend,
    DialogField::Model,
    DialogField::Effort,
    DialogField::Agent,
    DialogField::ChainTo,
    DialogField::Interactive,
];

const SETTINGS_FORM_FIELDS: [DialogField; 7] = [
    DialogField::Title,
    DialogField::Backend,
    DialogField::Model,
    DialogField::Effort,
    DialogField::Agent,
    DialogField::Theme,
    DialogField::TaskSort,
];

const GLOBAL_SETTINGS_FORM_FIELDS: [DialogField; 2] =
    [DialogField::EscapeToProjects, DialogField::ProjectSort];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalButton {
    Save,
    Cancel,
    Yes,
    No,
}

pub struct ModalState {
    pub modal: Modal,
    pub field_index: usize,
    pub title: TextArea<'static>,
    pub description: TextArea<'static>,
    pub backend: TextArea<'static>,
    pub model: TextArea<'static>,
    pub effort: TextArea<'static>,
    pub agent: TextArea<'static>,
    pub chain_to: TextArea<'static>,
    pub target_status: TextArea<'static>,
    pub answer: TextArea<'static>,
    pub theme: TextArea<'static>,
    pub task_sort: TextArea<'static>,
    pub interactive: bool,
    pub escape_to_projects: bool,
    pub project_sort: TextArea<'static>,
    pub form_scroll: usize,
    pub error: Option<String>,
    pub discard_confirm: bool,
    pub confirm_yes_selected: bool,
    initial_values: Option<String>,
    pub backend_options: Vec<SelectOption>,
    pub backend_selected: usize,
    pub model_options: Vec<SelectOption>,
    pub model_selected: usize,
    pub effort_options: Vec<SelectOption>,
    pub effort_selected: usize,
    pub agent_options: Vec<SelectOption>,
    pub agent_selected: usize,
    pub chain_options: Vec<SelectOption>,
    pub chain_selected: usize,
    pub status_options: Vec<SelectOption>,
    pub status_selected: usize,
    pub kind_selected: usize,
    pub question_selected: usize,
    pub variant_selected: Option<usize>,
    pub theme_options: Vec<SelectOption>,
    pub theme_selected: usize,
    pub task_sort_options: Vec<SelectOption>,
    pub task_sort_selected: usize,
    pub project_sort_options: Vec<SelectOption>,
    pub project_sort_selected: usize,
    pub purge_data: bool,
}

impl ModalState {
    pub fn new(modal: Modal) -> Self {
        let wraps_description = matches!(&modal, Modal::NewTask { .. } | Modal::EditTask { .. });
        Self {
            modal,
            field_index: 0,
            title: one_line(""),
            description: if wraps_description {
                wrapped_description(Vec::new())
            } else {
                TextArea::default()
            },
            backend: one_line(""),
            model: one_line(""),
            effort: one_line(""),
            agent: one_line(""),
            chain_to: one_line(""),
            target_status: one_line("todo"),
            answer: TextArea::default(),
            theme: one_line("dark"),
            task_sort: one_line("task_number"),
            interactive: false,
            escape_to_projects: false,
            project_sort: one_line("name"),
            form_scroll: 0,
            error: None,
            discard_confirm: false,
            confirm_yes_selected: false,
            initial_values: None,
            backend_options: Vec::new(),
            backend_selected: 0,
            model_options: Vec::new(),
            model_selected: 0,
            effort_options: Vec::new(),
            effort_selected: 0,
            agent_options: Vec::new(),
            agent_selected: 0,
            chain_options: Vec::new(),
            chain_selected: 0,
            status_options: Vec::new(),
            status_selected: 0,
            kind_selected: 0,
            question_selected: 0,
            variant_selected: None,
            theme_options: Vec::new(),
            theme_selected: 0,
            task_sort_options: Vec::new(),
            task_sort_selected: 0,
            project_sort_options: Vec::new(),
            project_sort_selected: 0,
            purge_data: false,
        }
    }

    pub fn for_task(modal: Modal, task: &Task) -> Self {
        let mut state = Self::new(modal);
        state.title = one_line(&task.title);
        state.description = wrapped_description(lines_or_empty(&task.description));
        state.backend = one_line(task.agent_backend.as_deref().unwrap_or(""));
        state.model = one_line(task.ai_model.as_deref().unwrap_or(""));
        state.effort = one_line(task.ai_effort.as_deref().unwrap_or(""));
        state.agent = one_line(task.agent_name.as_deref().unwrap_or(""));
        state.chain_to = one_line(task.chained_to.as_deref().unwrap_or(""));
        state.interactive = task.interactive;
        state
    }

    pub fn fields(&self) -> &'static [DialogField] {
        match self.modal {
            Modal::NewTask { .. } | Modal::EditTask { .. } => &[
                DialogField::Title,
                DialogField::Description,
                DialogField::Backend,
                DialogField::Model,
                DialogField::Effort,
                DialogField::Agent,
                DialogField::ChainTo,
                DialogField::Interactive,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
            Modal::MoveTask { .. } => &[
                DialogField::TargetStatus,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
            Modal::AddMessage { .. } => &[
                DialogField::MessageKind,
                DialogField::Description,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
            Modal::DeleteConfirm { .. }
            | Modal::RevertConfirm { .. }
            | Modal::BulkConfirm { .. }
            | Modal::KillSessionConfirm { .. }
            | Modal::RestoreConfirm { .. } => &[DialogField::Confirm, DialogField::Cancel],
            Modal::AnswerQuestion { .. } => &[
                DialogField::Question,
                DialogField::Variant,
                DialogField::Answer,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
            Modal::Settings => &[
                DialogField::Title,
                DialogField::Backend,
                DialogField::Model,
                DialogField::Effort,
                DialogField::Agent,
                DialogField::Theme,
                DialogField::TaskSort,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
            Modal::GlobalSettings => &[
                DialogField::EscapeToProjects,
                DialogField::ProjectSort,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
            Modal::NewProject => &[
                DialogField::Description,
                DialogField::Title,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
            Modal::RenameProject { .. } => &[
                DialogField::Title,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
            Modal::SetProjectPath { .. } => &[
                DialogField::Description,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
            Modal::DeleteProject { .. } => &[
                DialogField::PurgeData,
                DialogField::Confirm,
                DialogField::Cancel,
            ],
        }
    }

    pub fn active_field(&self) -> DialogField {
        self.fields()[self.field_index.min(self.fields().len().saturating_sub(1))]
    }

    pub fn next_field(&mut self) {
        let len = self.fields().len();
        if len > 0 {
            self.field_index = (self.field_index + 1) % len;
            self.ensure_active_field_visible();
        }
    }

    pub fn prev_field(&mut self) {
        let len = self.fields().len();
        if len > 0 {
            self.field_index = if self.field_index == 0 {
                len - 1
            } else {
                self.field_index - 1
            };
            self.ensure_active_field_visible();
        }
    }

    pub fn submit_on_enter(&self) -> bool {
        self.active_field() == DialogField::Confirm
    }

    pub fn cancel_on_enter(&self) -> bool {
        self.active_field() == DialogField::Cancel
    }

    /// Ctrl+S submits form-style dialogs from any field; pure confirmation
    /// dialogs are excluded so a save reflex cannot trigger a destructive
    /// action.
    pub fn submit_on_ctrl_s(&self) -> bool {
        !matches!(
            self.modal,
            Modal::DeleteConfirm { .. }
                | Modal::RevertConfirm { .. }
                | Modal::BulkConfirm { .. }
                | Modal::KillSessionConfirm { .. }
                | Modal::RestoreConfirm { .. }
                | Modal::DeleteProject { .. }
        )
    }

    pub fn focus_field(&mut self, field: DialogField) {
        if let Some(index) = self
            .fields()
            .iter()
            .position(|candidate| *candidate == field)
        {
            self.field_index = index;
            self.ensure_active_field_visible();
        }
    }

    pub fn capture_initial_values(&mut self) {
        self.initial_values = Some(self.editable_signature());
    }

    pub fn is_dirty(&self) -> bool {
        self.initial_values
            .as_ref()
            .is_some_and(|initial| initial != &self.editable_signature())
    }

    pub fn select_option(&mut self, field: DialogField, index: usize) {
        let kind = match field {
            DialogField::Backend => Some(SelectorKind::Backend),
            DialogField::Model => Some(SelectorKind::Model),
            DialogField::Effort => Some(SelectorKind::Effort),
            DialogField::Agent => Some(SelectorKind::Agent),
            DialogField::ChainTo => Some(SelectorKind::ChainTo),
            DialogField::TargetStatus => Some(SelectorKind::TargetStatus),
            DialogField::MessageKind => Some(SelectorKind::MessageKind),
            DialogField::Question => Some(SelectorKind::Question),
            DialogField::Variant => Some(SelectorKind::Variant),
            DialogField::Theme => Some(SelectorKind::Theme),
            DialogField::TaskSort => Some(SelectorKind::TaskSort),
            DialogField::ProjectSort => Some(SelectorKind::ProjectSort),
            _ => None,
        };
        if let Some(kind) = kind {
            let len = self.selection_len(kind);
            if len > 0 {
                let selected = index.min(len - 1);
                if self.selection_value(kind) == selected {
                    return;
                }
                *self.selection_mut(kind) = selected;
                self.apply_selection(kind);
                self.error = None;
            }
        }
    }

    pub fn input(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let before = self.editable_signature();
        match self.active_field() {
            DialogField::Title => input_single_line(&mut self.title, key),
            DialogField::Description => {
                self.description.input(key);
            }
            DialogField::Backend => self.input_select(key, SelectorKind::Backend),
            DialogField::Model => self.input_select(key, SelectorKind::Model),
            DialogField::Effort => self.input_select(key, SelectorKind::Effort),
            DialogField::Agent => self.input_select(key, SelectorKind::Agent),
            DialogField::ChainTo => self.input_select(key, SelectorKind::ChainTo),
            DialogField::Interactive => match key.code {
                ratatui::crossterm::event::KeyCode::Char(' ')
                | ratatui::crossterm::event::KeyCode::Enter => self.interactive = !self.interactive,
                _ => {}
            },
            DialogField::EscapeToProjects => match key.code {
                ratatui::crossterm::event::KeyCode::Char(' ')
                | ratatui::crossterm::event::KeyCode::Enter => {
                    self.escape_to_projects = !self.escape_to_projects
                }
                _ => {}
            },
            DialogField::ProjectSort => self.input_select(key, SelectorKind::ProjectSort),
            DialogField::PurgeData => match key.code {
                ratatui::crossterm::event::KeyCode::Char(' ')
                | ratatui::crossterm::event::KeyCode::Enter => self.purge_data = !self.purge_data,
                _ => {}
            },
            DialogField::TargetStatus => self.input_select(key, SelectorKind::TargetStatus),
            DialogField::MessageKind => self.input_select(key, SelectorKind::MessageKind),
            DialogField::Question => self.input_select(key, SelectorKind::Question),
            DialogField::Variant => self.input_select(key, SelectorKind::Variant),
            DialogField::Answer => {
                self.answer.input(key);
            }
            DialogField::Theme => self.input_select(key, SelectorKind::Theme),
            DialogField::TaskSort => self.input_select(key, SelectorKind::TaskSort),
            DialogField::Confirm | DialogField::Cancel => {}
        }
        if self.editable_signature() != before {
            self.error = None;
        }
    }

    /// Insert clipboard text into the focused field as one edit.
    ///
    /// Bracketed paste keeps the terminal from replaying the clipboard as
    /// keystrokes, where embedded tabs would hop between fields and newlines
    /// could trigger the focused button.
    pub fn paste(&mut self, text: &str) -> bool {
        let before = self.editable_signature();
        let text = sanitize_paste_text(text);
        match self.active_field() {
            DialogField::Title => {
                self.title.insert_str(text.replace('\n', " "));
            }
            DialogField::Description => {
                self.description.insert_str(&text);
            }
            DialogField::Answer => {
                self.answer.insert_str(&text);
            }
            _ => return false,
        }
        if self.editable_signature() != before {
            self.error = None;
        }
        true
    }

    pub fn active_textarea_mut(&mut self) -> &mut TextArea<'static> {
        match self.active_field() {
            DialogField::Title => &mut self.title,
            DialogField::Description => &mut self.description,
            DialogField::Backend => &mut self.backend,
            DialogField::Model => &mut self.model,
            DialogField::Effort => &mut self.effort,
            DialogField::Agent => &mut self.agent,
            DialogField::ChainTo => &mut self.chain_to,
            DialogField::Interactive | DialogField::EscapeToProjects => &mut self.answer,
            DialogField::ProjectSort => &mut self.project_sort,
            DialogField::TargetStatus => &mut self.target_status,
            DialogField::MessageKind => &mut self.description,
            DialogField::Question | DialogField::Variant => &mut self.answer,
            DialogField::Answer => &mut self.answer,
            DialogField::Theme => &mut self.theme,
            DialogField::TaskSort => &mut self.task_sort,
            DialogField::Confirm | DialogField::Cancel | DialogField::PurgeData => &mut self.answer,
        }
    }

    pub fn set_backend_options(&mut self, options: Vec<SelectOption>) {
        self.backend_options = options;
        self.backend_selected =
            select_matching(&self.backend_options, self.backend_text().as_deref());
        self.apply_selection(SelectorKind::Backend);
    }

    pub fn set_model_options(&mut self, options: Vec<SelectOption>) {
        self.model_options = options;
        self.model_selected = select_matching(&self.model_options, self.model_text().as_deref());
        self.apply_selection(SelectorKind::Model);
    }

    pub fn set_effort_options(&mut self, options: Vec<SelectOption>) {
        self.effort_options = options;
        self.effort_selected = select_matching(&self.effort_options, self.effort_text().as_deref());
        self.apply_selection(SelectorKind::Effort);
    }

    pub fn set_agent_options(&mut self, options: Vec<SelectOption>) {
        self.agent_options = options;
        self.agent_selected = select_matching(&self.agent_options, self.agent_text().as_deref());
        self.apply_selection(SelectorKind::Agent);
    }

    pub fn set_chain_options(&mut self, options: Vec<SelectOption>) {
        self.chain_options = options;
        self.chain_selected = select_matching(&self.chain_options, self.chain_text().as_deref());
        self.apply_selection(SelectorKind::ChainTo);
    }

    pub fn set_status_options(&mut self, options: Vec<SelectOption>, current: Option<&str>) {
        self.status_options = options;
        self.status_selected = select_matching(&self.status_options, current);
        self.apply_selection(SelectorKind::TargetStatus);
    }

    pub fn set_theme_options(&mut self, options: Vec<SelectOption>) {
        self.theme_options = options;
        self.theme_selected = select_matching(&self.theme_options, self.theme_text().as_deref());
        self.apply_selection(SelectorKind::Theme);
    }

    pub fn set_task_sort_options(&mut self, options: Vec<SelectOption>) {
        self.task_sort_options = options;
        self.task_sort_selected =
            select_matching(&self.task_sort_options, self.task_sort_text().as_deref());
        self.apply_selection(SelectorKind::TaskSort);
    }

    pub fn set_project_sort_options(&mut self, options: Vec<SelectOption>) {
        self.project_sort_options = options;
        self.project_sort_selected = select_matching(
            &self.project_sort_options,
            self.project_sort_text().as_deref(),
        );
        self.apply_selection(SelectorKind::ProjectSort);
    }

    pub fn title_text(&self) -> String {
        textarea_text(&self.title)
    }

    pub fn description_text(&self) -> String {
        textarea_text(&self.description)
    }

    pub fn backend_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.backend))
    }

    pub fn model_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.model))
    }

    pub fn effort_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.effort))
    }

    pub fn agent_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.agent))
    }

    pub fn chain_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.chain_to))
    }

    pub fn target_text(&self) -> String {
        textarea_text(&self.target_status)
    }

    pub fn theme_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.theme))
    }

    pub fn task_sort_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.task_sort))
    }

    pub fn project_sort_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.project_sort))
    }

    pub fn answer_text(&self) -> String {
        let custom = textarea_text(&self.answer);
        if !custom.trim().is_empty() {
            return custom;
        }
        self.selected_variant().unwrap_or_default()
    }

    pub fn selected_question_ref(&self) -> Option<QuestionRef> {
        match &self.modal {
            Modal::AnswerQuestion { questions, .. } => questions
                .get(
                    self.question_selected
                        .min(questions.len().saturating_sub(1)),
                )
                .map(|question| QuestionRef::MsgId(question.message_id.clone())),
            _ => None,
        }
    }

    fn input_select(&mut self, key: ratatui::crossterm::event::KeyEvent, kind: SelectorKind) {
        use ratatui::crossterm::event::KeyCode;
        match key.code {
            KeyCode::Up | KeyCode::Left => self.move_selection(kind, -1),
            KeyCode::Down | KeyCode::Right => self.move_selection(kind, 1),
            KeyCode::Enter | KeyCode::Char(' ') => {}
            _ => {}
        }
    }

    fn move_selection(&mut self, kind: SelectorKind, delta: isize) {
        let len = self.selection_len(kind);
        if len == 0 {
            return;
        }
        let selected = self.selection_value(kind);
        let next = if delta.is_negative() {
            selected.saturating_sub(delta.unsigned_abs())
        } else {
            selected.saturating_add(delta as usize).min(len - 1)
        };
        if next == selected {
            return;
        }
        *self.selection_mut(kind) = next;
        self.apply_selection(kind);
    }

    fn selection_len(&self, kind: SelectorKind) -> usize {
        match kind {
            SelectorKind::Backend => self.backend_options.len(),
            SelectorKind::Model => self.model_options.len(),
            SelectorKind::Effort => self.effort_options.len(),
            SelectorKind::Agent => self.agent_options.len(),
            SelectorKind::ChainTo => self.chain_options.len(),
            SelectorKind::TargetStatus => self.status_options.len(),
            SelectorKind::MessageKind => MESSAGE_KIND_OPTIONS.len(),
            SelectorKind::Question => match &self.modal {
                Modal::AnswerQuestion { questions, .. } => questions.len(),
                _ => 0,
            },
            SelectorKind::Variant => self.current_variants().len() + 1,
            SelectorKind::Theme => self.theme_options.len(),
            SelectorKind::TaskSort => self.task_sort_options.len(),
            SelectorKind::ProjectSort => self.project_sort_options.len(),
        }
    }

    fn selection_mut(&mut self, kind: SelectorKind) -> &mut usize {
        match kind {
            SelectorKind::Backend => &mut self.backend_selected,
            SelectorKind::Model => &mut self.model_selected,
            SelectorKind::Effort => &mut self.effort_selected,
            SelectorKind::Agent => &mut self.agent_selected,
            SelectorKind::ChainTo => &mut self.chain_selected,
            SelectorKind::TargetStatus => &mut self.status_selected,
            SelectorKind::MessageKind => &mut self.kind_selected,
            SelectorKind::Question => &mut self.question_selected,
            SelectorKind::Variant => self.variant_selected.get_or_insert(0),
            SelectorKind::Theme => &mut self.theme_selected,
            SelectorKind::TaskSort => &mut self.task_sort_selected,
            SelectorKind::ProjectSort => &mut self.project_sort_selected,
        }
    }

    fn selection_value(&self, kind: SelectorKind) -> usize {
        match kind {
            SelectorKind::Backend => self.backend_selected,
            SelectorKind::Model => self.model_selected,
            SelectorKind::Effort => self.effort_selected,
            SelectorKind::Agent => self.agent_selected,
            SelectorKind::ChainTo => self.chain_selected,
            SelectorKind::TargetStatus => self.status_selected,
            SelectorKind::MessageKind => self.kind_selected,
            SelectorKind::Question => self.question_selected,
            SelectorKind::Variant => self.variant_selected.unwrap_or(0),
            SelectorKind::Theme => self.theme_selected,
            SelectorKind::TaskSort => self.task_sort_selected,
            SelectorKind::ProjectSort => self.project_sort_selected,
        }
    }

    fn apply_selection(&mut self, kind: SelectorKind) {
        match kind {
            SelectorKind::Backend => {
                let text = selected_value(&self.backend_options, self.backend_selected);
                self.backend = one_line(text.as_deref().unwrap_or(""));
            }
            SelectorKind::Model => {
                let text = selected_value(&self.model_options, self.model_selected);
                self.model = one_line(text.as_deref().unwrap_or(""));
            }
            SelectorKind::Effort => {
                let text = selected_value(&self.effort_options, self.effort_selected);
                self.effort = one_line(text.as_deref().unwrap_or(""));
            }
            SelectorKind::Agent => {
                let text = selected_value(&self.agent_options, self.agent_selected);
                self.agent = one_line(text.as_deref().unwrap_or(""));
            }
            SelectorKind::ChainTo => {
                let text = selected_value(&self.chain_options, self.chain_selected);
                self.chain_to = one_line(text.as_deref().unwrap_or(""));
            }
            SelectorKind::TargetStatus => {
                let text = selected_value(&self.status_options, self.status_selected);
                self.target_status = one_line(text.as_deref().unwrap_or(""));
            }
            // The kind lives in `kind_selected` itself; nothing to sync.
            SelectorKind::MessageKind => {}
            SelectorKind::Question => {
                self.variant_selected = None;
                self.answer = TextArea::default();
            }
            SelectorKind::Variant => {
                if let Some(answer) = self.selected_variant() {
                    self.answer = TextArea::new(vec![answer]);
                } else {
                    self.answer = TextArea::default();
                }
            }
            SelectorKind::Theme => {
                let text = selected_value(&self.theme_options, self.theme_selected);
                self.theme = one_line(text.as_deref().unwrap_or("dark"));
            }
            SelectorKind::TaskSort => {
                let text = selected_value(&self.task_sort_options, self.task_sort_selected);
                self.task_sort = one_line(text.as_deref().unwrap_or("task_number"));
            }
            SelectorKind::ProjectSort => {
                let text = selected_value(&self.project_sort_options, self.project_sort_selected);
                self.project_sort = one_line(text.as_deref().unwrap_or("name"));
            }
        }
    }

    fn current_variants(&self) -> &[String] {
        match &self.modal {
            Modal::AnswerQuestion { questions, .. } => questions
                .get(
                    self.question_selected
                        .min(questions.len().saturating_sub(1)),
                )
                .map(|question| question.variants.as_slice())
                .unwrap_or(&[]),
            _ => &[],
        }
    }

    fn selected_variant(&self) -> Option<String> {
        let selected = self.variant_selected?;
        if selected == 0 {
            return None;
        }
        self.current_variants().get(selected - 1).cloned()
    }

    fn ensure_active_field_visible(&mut self) {
        let fields = match self.modal {
            Modal::NewTask { .. } | Modal::EditTask { .. } => Some(&TASK_FORM_FIELDS[..]),
            Modal::Settings => Some(&SETTINGS_FORM_FIELDS[..]),
            Modal::GlobalSettings => Some(&GLOBAL_SETTINGS_FORM_FIELDS[..]),
            _ => None,
        };
        if let Some(fields) = fields {
            self.form_scroll = self.field_index.min(fields.len() - 1);
        }
    }

    fn editable_signature(&self) -> String {
        [
            raw_textarea_text(&self.title),
            raw_textarea_text(&self.description),
            raw_textarea_text(&self.backend),
            raw_textarea_text(&self.model),
            raw_textarea_text(&self.effort),
            raw_textarea_text(&self.agent),
            raw_textarea_text(&self.chain_to),
            raw_textarea_text(&self.target_status),
            self.interactive.to_string(),
            self.backend_selected.to_string(),
            self.model_selected.to_string(),
            self.effort_selected.to_string(),
            self.agent_selected.to_string(),
            self.chain_selected.to_string(),
            self.status_selected.to_string(),
            self.kind_selected.to_string(),
            self.question_selected.to_string(),
            self.variant_selected.unwrap_or_default().to_string(),
            raw_textarea_text(&self.answer),
            raw_textarea_text(&self.theme),
            self.theme_selected.to_string(),
            raw_textarea_text(&self.task_sort),
            self.task_sort_selected.to_string(),
            self.escape_to_projects.to_string(),
            raw_textarea_text(&self.project_sort),
            self.project_sort_selected.to_string(),
            self.purge_data.to_string(),
        ]
        .join("\u{1f}")
    }
}

#[derive(Debug, Clone, Copy)]
enum SelectorKind {
    Backend,
    Model,
    Effort,
    Agent,
    ChainTo,
    TargetStatus,
    MessageKind,
    Question,
    Variant,
    Theme,
    TaskSort,
    ProjectSort,
}

pub fn render(frame: &mut Frame<'_>, app: &App, modal: &mut ModalState, area: Rect) -> Vec<Hitbox> {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(modal_title(&modal.modal))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.focus))
        .style(Style::default().bg(app.theme.bg).fg(app.theme.fg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hitboxes = Vec::new();
    if modal.discard_confirm {
        render_discard_confirm(frame, app, modal, inner, &mut hitboxes);
        return hitboxes;
    }
    match &modal.modal {
        Modal::NewTask { .. } | Modal::EditTask { .. } => {
            render_task_form(frame, app, modal, inner, &mut hitboxes)
        }
        Modal::MoveTask { task_id } => {
            render_move(frame, app, modal, inner, task_id, &mut hitboxes)
        }
        Modal::DeleteConfirm { task_id } => render_confirm(
            frame,
            app,
            modal,
            inner,
            "Delete permanently",
            task_id,
            &mut hitboxes,
        ),
        Modal::RevertConfirm { task_id } => {
            render_confirm(frame, app, modal, inner, "Revert", task_id, &mut hitboxes)
        }
        Modal::BulkConfirm { action, task_ids } => render_bulk_confirm(
            frame,
            app,
            modal,
            inner,
            *action,
            task_ids.len(),
            &mut hitboxes,
        ),
        Modal::KillSessionConfirm { session_id } => render_simple_confirm(
            frame,
            app,
            modal,
            inner,
            app.theme.err,
            vec![
                Line::from(format!(
                    "Kill session {}?",
                    sanitize_terminal_text(session_id)
                )),
                Line::from(
                    "Stops the agent process and closes the session. The task keeps its status.",
                ),
            ],
            &mut hitboxes,
        ),
        Modal::RestoreConfirm { task_id } => render_simple_confirm(
            frame,
            app,
            modal,
            inner,
            app.theme.warn,
            vec![
                Line::from(format!(
                    "Restore task {} from Archive?",
                    sanitize_terminal_text(task_id)
                )),
                Line::from("The task returns to To Do and keeps its last session record."),
            ],
            &mut hitboxes,
        ),
        Modal::AddMessage { task_id } => {
            render_add_message(frame, app, modal, inner, task_id, &mut hitboxes)
        }
        Modal::AnswerQuestion { task_id, questions } => {
            render_answer(frame, app, modal, inner, task_id, questions, &mut hitboxes)
        }
        Modal::Settings => render_settings_form(frame, app, modal, inner, &mut hitboxes),
        Modal::GlobalSettings => {
            render_global_settings_form(frame, app, modal, inner, &mut hitboxes)
        }
        Modal::NewProject => render_project_form(
            frame,
            app,
            modal,
            inner,
            "Folder path",
            Some("Display name"),
            &mut hitboxes,
        ),
        Modal::RenameProject { .. } => render_project_form(
            frame,
            app,
            modal,
            inner,
            "Display name",
            None,
            &mut hitboxes,
        ),
        Modal::SetProjectPath { .. } => {
            render_project_form(frame, app, modal, inner, "Folder path", None, &mut hitboxes)
        }
        Modal::DeleteProject {
            name, task_count, ..
        } => render_delete_project(frame, app, modal, inner, name, *task_count, &mut hitboxes),
    }
    hitboxes
}

fn render_settings_form(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    render_selector_form(frame, app, modal, area, hitboxes, &SETTINGS_FORM_FIELDS);
}

fn render_global_settings_form(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    render_selector_form(
        frame,
        app,
        modal,
        area,
        hitboxes,
        &GLOBAL_SETTINGS_FORM_FIELDS,
    );
}

fn render_simple_confirm(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    border: ratatui::style::Color,
    text: Vec<Line<'static>>,
    hitboxes: &mut Vec<Hitbox>,
) {
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
    render_confirm_buttons(frame, app, modal, area, hitboxes);
}

fn render_bulk_confirm(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    action: BulkAction,
    count: usize,
    hitboxes: &mut Vec<Hitbox>,
) {
    let prompt = match action {
        BulkAction::ArchiveAllDone => format!("Archive {count} Done task(s)?"),
        BulkAction::MarkReviewDone => format!("Mark {count} Review task(s) Done?"),
    };
    let text = vec![
        Line::from(prompt),
        Line::from("This changes every currently matching task."),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.theme.warn)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
    render_confirm_buttons(frame, app, modal, area, hitboxes);
}

fn render_add_message(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    task_id: &str,
    hitboxes: &mut Vec<Hitbox>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(4),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "Add a thread message to {}",
            sanitize_terminal_text(task_id)
        )),
        rows[0],
    );
    let kind_options = MESSAGE_KIND_OPTIONS
        .iter()
        .map(|kind| SelectOption {
            label: (*kind).to_string(),
            value: Some((*kind).to_string()),
        })
        .collect::<Vec<_>>();
    render_select(
        frame,
        app,
        "Kind",
        &kind_options,
        modal.kind_selected,
        rows[1],
        modal.active_field() == DialogField::MessageKind
            || app.is_hovered(HitAction::ModalField(DialogField::MessageKind)),
    );
    register_field(hitboxes, rows[1], DialogField::MessageKind);
    register_options(
        hitboxes,
        rows[1],
        DialogField::MessageKind,
        MESSAGE_KIND_OPTIONS.len(),
        modal.kind_selected,
    );
    render_textarea(
        frame,
        app,
        &modal.description,
        rows[2],
        "Text",
        modal.active_field() == DialogField::Description
            || app.is_hovered(HitAction::ModalField(DialogField::Description)),
    );
    register_field(hitboxes, rows[2], DialogField::Description);
    render_form_buttons(frame, app, modal, rows[3], hitboxes);
}

fn render_task_form(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    render_selector_form(frame, app, modal, area, hitboxes, &TASK_FORM_FIELDS);
}

fn render_selector_form(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
    fields: &[DialogField],
) {
    let button_height = 4;
    let content_height = area.height.saturating_sub(button_height);
    let content = Rect {
        height: content_height,
        ..area
    };
    let button_area = Rect {
        y: area.y.saturating_add(content_height),
        height: button_height.min(area.height.saturating_sub(content_height)),
        ..area
    };
    let rows = selector_form_rows(modal, content.height, fields);
    let mut y = content.y;
    for (field, height) in rows {
        let row = Rect {
            x: content.x,
            y,
            width: content.width,
            height,
        };
        render_selector_field(frame, app, modal, field, row);
        register_field(hitboxes, row, field);
        register_task_options(hitboxes, modal, field, row);
        y = y.saturating_add(height);
    }
    render_form_buttons(frame, app, modal, button_area, hitboxes);
}

fn selector_form_rows(
    modal: &ModalState,
    content_height: u16,
    fields: &[DialogField],
) -> Vec<(DialogField, u16)> {
    let mut rows = Vec::new();
    let mut used: u16 = 0;
    for field in fields
        .iter()
        .copied()
        .skip(modal.form_scroll.min(fields.len() - 1))
    {
        let height = task_field_min_height(field);
        if used.saturating_add(height) > content_height {
            break;
        }
        rows.push((field, height));
        used = used.saturating_add(height);
    }

    let mut surplus = content_height.saturating_sub(used);
    if let Some((_, height)) = rows
        .iter_mut()
        .find(|(field, _)| *field == DialogField::Description)
    {
        let growth = (10 - *height).min(surplus);
        *height += growth;
        surplus -= growth;
    }
    while surplus > 0 {
        let mut grew = false;
        for (field, height) in &mut rows {
            let max_height = task_selector_max_height(modal, *field);
            if *height < max_height {
                *height += 1;
                surplus -= 1;
                grew = true;
                if surplus == 0 {
                    break;
                }
            }
        }
        if !grew {
            break;
        }
    }
    rows
}

fn task_field_min_height(field: DialogField) -> u16 {
    match field {
        DialogField::Title | DialogField::Interactive | DialogField::EscapeToProjects => 3,
        DialogField::Description => 5,
        _ => 4,
    }
}

fn task_selector_max_height(modal: &ModalState, field: DialogField) -> u16 {
    if field == DialogField::Description {
        return 10;
    }
    let count = match field {
        DialogField::Backend => modal.backend_options.len(),
        DialogField::Model => modal.model_options.len(),
        DialogField::Effort => modal.effort_options.len(),
        DialogField::Agent => modal.agent_options.len(),
        DialogField::Theme => modal.theme_options.len(),
        DialogField::TaskSort => modal.task_sort_options.len(),
        DialogField::ProjectSort => modal.project_sort_options.len(),
        DialogField::ChainTo => modal.chain_options.len(),
        _ => return task_field_min_height(field),
    };
    task_field_min_height(field).max((count.saturating_add(2)).min(u16::MAX as usize) as u16)
}

fn render_move(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    task_id: &str,
    hitboxes: &mut Vec<Hitbox>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Length(4),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!("Move {task_id} to status id:")),
        rows[0],
    );
    render_select(
        frame,
        app,
        "Status",
        &modal.status_options,
        modal.status_selected,
        rows[1],
        modal.active_field() == DialogField::TargetStatus
            || app.is_hovered(HitAction::ModalField(DialogField::TargetStatus)),
    );
    register_field(hitboxes, rows[1], DialogField::TargetStatus);
    register_options(
        hitboxes,
        rows[1],
        DialogField::TargetStatus,
        modal.status_options.len(),
        modal.status_selected,
    );
    render_form_buttons(frame, app, modal, rows[2], hitboxes);
}

fn render_confirm(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    action: &str,
    task_id: &str,
    hitboxes: &mut Vec<Hitbox>,
) {
    let border = if action == "Delete permanently" {
        app.theme.err
    } else {
        app.theme.warn
    };
    let text = vec![
        Line::from(format!(
            "{action} task {}?",
            sanitize_terminal_text(task_id)
        )),
        Line::from(if action == "Delete permanently" {
            "Removes the task, its thread, backups, session logs, pasted assets, and context."
        } else {
            "Restores files from this task's saved backups."
        }),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
    render_confirm_buttons(frame, app, modal, area, hitboxes);
}

fn render_answer(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    task_id: &str,
    questions: &[QuestionChoice],
    hitboxes: &mut Vec<Hitbox>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Min(4),
            Constraint::Length(4),
        ])
        .split(area);
    let question = questions.get(
        modal
            .question_selected
            .min(questions.len().saturating_sub(1)),
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Answer question on {}",
            sanitize_terminal_text(task_id)
        )),
        rows[0],
    );
    let question_items = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let style = option_hover_style(
                app,
                HitAction::ModalOption {
                    field: DialogField::Question,
                    index,
                },
            );
            ListItem::new(format!(
                "{}  {}",
                sanitize_terminal_text(&question.message_id),
                super::card::truncate_display(&sanitize_terminal_text(&question.body), 64)
            ))
            .style(style)
        })
        .collect::<Vec<_>>();
    render_list(
        frame,
        app,
        " Questions ",
        question_items,
        modal.question_selected,
        rows[1],
        modal.active_field() == DialogField::Question
            || select_field_hovered(app, DialogField::Question, questions.len()),
    );
    let mut variant_items =
        vec![
            ListItem::new("Custom answer textarea").style(option_hover_style(
                app,
                HitAction::ModalOption {
                    field: DialogField::Variant,
                    index: 0,
                },
            )),
        ];
    if let Some(question) = question {
        variant_items.extend(question.variants.iter().enumerate().map(
            |(variant_index, variant)| {
                let index = variant_index + 1;
                ListItem::new(sanitize_terminal_text(variant)).style(option_hover_style(
                    app,
                    HitAction::ModalOption {
                        field: DialogField::Variant,
                        index,
                    },
                ))
            },
        ));
    }
    let variant_count = variant_items.len();
    render_list(
        frame,
        app,
        " Variants ",
        variant_items,
        modal.variant_selected.unwrap_or(0),
        rows[2],
        modal.active_field() == DialogField::Variant
            || select_field_hovered(app, DialogField::Variant, variant_count),
    );
    render_textarea(
        frame,
        app,
        &modal.answer,
        rows[3],
        "Custom answer / selected variant",
        modal.active_field() == DialogField::Answer
            || app.is_hovered(HitAction::ModalField(DialogField::Answer)),
    );
    register_field(hitboxes, rows[1], DialogField::Question);
    register_options(
        hitboxes,
        rows[1],
        DialogField::Question,
        questions.len(),
        modal.question_selected,
    );
    register_field(hitboxes, rows[2], DialogField::Variant);
    register_options(
        hitboxes,
        rows[2],
        DialogField::Variant,
        variant_count,
        modal.variant_selected.unwrap_or(0),
    );
    register_field(hitboxes, rows[3], DialogField::Answer);
    render_form_buttons(frame, app, modal, rows[4], hitboxes);
}

fn render_selector_field(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    field: DialogField,
    area: Rect,
) {
    match field {
        DialogField::Title => render_textarea(
            frame,
            app,
            &modal.title,
            area,
            if matches!(modal.modal, Modal::Settings) {
                "Project name"
            } else {
                "Title"
            },
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::Description => render_description_textarea(
            frame,
            app,
            modal,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::Backend => render_select(
            frame,
            app,
            "Backend",
            &modal.backend_options,
            modal.backend_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::Model => render_select(
            frame,
            app,
            "Model",
            &modal.model_options,
            modal.model_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::Effort => render_select(
            frame,
            app,
            "Effort",
            &modal.effort_options,
            modal.effort_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::Agent => render_select(
            frame,
            app,
            "Agent",
            &modal.agent_options,
            modal.agent_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::ChainTo => render_select(
            frame,
            app,
            "Chain to",
            &modal.chain_options,
            modal.chain_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::Interactive => render_interactive(frame, app, modal, area),
        DialogField::EscapeToProjects => render_escape_to_projects(frame, app, modal, area),
        DialogField::Theme => render_select(
            frame,
            app,
            "Theme",
            &modal.theme_options,
            modal.theme_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::TaskSort => render_select(
            frame,
            app,
            "Task sorting",
            &modal.task_sort_options,
            modal.task_sort_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::ProjectSort => render_select(
            frame,
            app,
            "Project sorting",
            &modal.project_sort_options,
            modal.project_sort_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        _ => {}
    }
}

fn register_task_options(
    hitboxes: &mut Vec<Hitbox>,
    modal: &ModalState,
    field: DialogField,
    area: Rect,
) {
    let (count, selected) = match field {
        DialogField::Backend => (modal.backend_options.len(), modal.backend_selected),
        DialogField::Model => (modal.model_options.len(), modal.model_selected),
        DialogField::Effort => (modal.effort_options.len(), modal.effort_selected),
        DialogField::Agent => (modal.agent_options.len(), modal.agent_selected),
        DialogField::Theme => (modal.theme_options.len(), modal.theme_selected),
        DialogField::TaskSort => (modal.task_sort_options.len(), modal.task_sort_selected),
        DialogField::ProjectSort => (
            modal.project_sort_options.len(),
            modal.project_sort_selected,
        ),
        DialogField::ChainTo => (modal.chain_options.len(), modal.chain_selected),
        _ => (0, 0),
    };
    register_options(hitboxes, area, field, count, selected);
}

fn render_select(
    frame: &mut Frame<'_>,
    app: &App,
    title: &str,
    options: &[SelectOption],
    selected: usize,
    area: Rect,
    active: bool,
) {
    let field = select_field_from_title(title);
    let active = field
        .map(|field| active || select_field_hovered(app, field, options.len()))
        .unwrap_or(active);
    let items = if options.is_empty() {
        vec![ListItem::new("-")]
    } else {
        options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let mut item = ListItem::new(sanitize_terminal_text(&option.label));
                if let Some(field) = field {
                    item = item.style(option_hover_style(
                        app,
                        HitAction::ModalOption { field, index },
                    ));
                }
                item
            })
            .collect()
    };
    render_list(
        frame,
        app,
        &format!(" {title} "),
        items,
        selected,
        area,
        active,
    );
}

fn select_field_hovered(app: &App, field: DialogField, option_count: usize) -> bool {
    app.is_hovered(HitAction::ModalField(field))
        || (0..option_count).any(|index| app.is_hovered(HitAction::ModalOption { field, index }))
}

fn select_field_from_title(title: &str) -> Option<DialogField> {
    match title {
        "Kind" => Some(DialogField::MessageKind),
        "Status" => Some(DialogField::TargetStatus),
        "Backend" => Some(DialogField::Backend),
        "Model" => Some(DialogField::Model),
        "Effort" => Some(DialogField::Effort),
        "Agent" => Some(DialogField::Agent),
        "Chain to" => Some(DialogField::ChainTo),
        "Theme" => Some(DialogField::Theme),
        "Task sorting" => Some(DialogField::TaskSort),
        "Project sorting" => Some(DialogField::ProjectSort),
        _ => None,
    }
}

fn option_hover_style(app: &App, action: HitAction) -> Style {
    if app.is_hovered(action) {
        Style::default()
            .fg(app.theme.focus)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn render_list(
    frame: &mut Frame<'_>,
    app: &App,
    title: &str,
    items: Vec<ListItem<'static>>,
    selected: usize,
    area: Rect,
    active: bool,
) {
    let border = if active {
        app.theme.focus
    } else {
        app.theme.border
    };
    let mut state = ListState::default();
    let selected = selected.min(items.len().saturating_sub(1));
    state.select(Some(selected));
    *state.offset_mut() = list_viewport_start(
        selected,
        items.len(),
        area.height.saturating_sub(2) as usize,
    );
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(app.theme.focus)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_form_buttons(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    let (buttons, error_area) = if modal.error.is_some() && area.height >= 4 {
        (
            Rect {
                y: area.y.saturating_add(1),
                height: area.height.saturating_sub(1),
                ..area
            },
            Some(Rect { height: 1, ..area }),
        )
    } else {
        (area, None)
    };
    if let (Some(error), Some(error_area)) = (&modal.error, error_area) {
        frame.render_widget(
            Paragraph::new(sanitize_terminal_text(error)).style(Style::default().fg(app.theme.err)),
            error_area,
        );
    }
    let save_active = modal.active_field() == DialogField::Confirm
        || app.is_hovered(HitAction::ModalButton(ModalButton::Save));
    let cancel_active = modal.active_field() == DialogField::Cancel
        || app.is_hovered(HitAction::ModalButton(ModalButton::Cancel));
    render_buttons(
        frame,
        app,
        buttons,
        "Save",
        save_active,
        "Cancel",
        cancel_active,
    );
    register_buttons(hitboxes, buttons, ModalButton::Save, ModalButton::Cancel);
}

fn render_confirm_buttons(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    let buttons = Rect {
        y: area.y.saturating_add(area.height.saturating_sub(3)),
        height: area.height.min(3),
        ..area
    };
    render_buttons(
        frame,
        app,
        buttons,
        "Yes",
        modal.confirm_yes_selected || app.is_hovered(HitAction::ModalButton(ModalButton::Yes)),
        "No",
        !modal.confirm_yes_selected || app.is_hovered(HitAction::ModalButton(ModalButton::No)),
    );
    register_buttons(hitboxes, buttons, ModalButton::Yes, ModalButton::No);
}

fn render_discard_confirm(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    frame.render_widget(
        Paragraph::new("Discard changes?")
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.theme.warn)),
            )
            .style(Style::default().bg(app.theme.bg).fg(app.theme.fg)),
        area,
    );
    render_confirm_buttons(frame, app, modal, area, hitboxes);
}

fn render_buttons(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    left: &str,
    left_active: bool,
    right: &str,
    right_active: bool,
) {
    let left_style = button_style(app, left_active);
    let right_style = button_style(app, right_active);
    let content = if left == "Save" && right == "Cancel" {
        let save_hint = "(Ctrl + S)";
        let nav_hint = "use Tab or Shift + Tab to navigate";
        let hint_width = area.width.saturating_sub(2) as usize;
        let hint_gap = hint_width
            .saturating_sub(save_hint.len() + nav_hint.len())
            .max(1);
        vec![
            Line::from(vec![
                ratatui::text::Span::styled(format!("[ {left} ]"), left_style),
                ratatui::text::Span::raw("  "),
                ratatui::text::Span::styled(format!("[ {right} ]"), right_style),
            ]),
            Line::from(vec![
                ratatui::text::Span::styled(save_hint, Style::default().fg(app.theme.muted)),
                ratatui::text::Span::raw(" ".repeat(hint_gap)),
                ratatui::text::Span::styled(nav_hint, Style::default().fg(app.theme.muted)),
            ]),
        ]
    } else {
        vec![Line::from(vec![
            ratatui::text::Span::styled(format!("[ {left} ]"), left_style),
            ratatui::text::Span::raw("  "),
            ratatui::text::Span::styled(format!("[ {right} ]"), right_style),
        ])]
    };
    frame.render_widget(
        Paragraph::new(content).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.border)),
        ),
        area,
    );
}

fn button_style(app: &App, active: bool) -> Style {
    if active {
        Style::default()
            .fg(app.theme.focus)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.muted)
    }
}

fn register_field(hitboxes: &mut Vec<Hitbox>, area: Rect, field: DialogField) {
    hitboxes.push(Hitbox {
        area,
        action: HitAction::ModalField(field),
    });
}

fn register_options(
    hitboxes: &mut Vec<Hitbox>,
    area: Rect,
    field: DialogField,
    count: usize,
    selected: usize,
) {
    let visible = area.height.saturating_sub(2) as usize;
    let start = list_viewport_start(selected, count, visible);
    for index in start..(start + visible).min(count) {
        hitboxes.insert(
            0,
            Hitbox {
                area: Rect {
                    x: area.x.saturating_add(1),
                    y: area.y.saturating_add(1 + (index - start) as u16),
                    width: area.width.saturating_sub(2),
                    height: 1,
                },
                action: HitAction::ModalOption { field, index },
            },
        );
    }
}

fn list_viewport_start(selected: usize, count: usize, visible: usize) -> usize {
    if visible == 0 || count <= visible {
        return 0;
    }
    selected
        .min(count.saturating_sub(1))
        .saturating_add(1)
        .saturating_sub(visible)
        .min(count - visible)
}

fn register_buttons(hitboxes: &mut Vec<Hitbox>, area: Rect, left: ModalButton, right: ModalButton) {
    let y = area.y.saturating_add(1);
    let left_width = match left {
        ModalButton::Save => 8,
        ModalButton::Yes => 7,
        _ => 8,
    };
    let right_width = match right {
        ModalButton::Cancel => 10,
        ModalButton::No => 6,
        _ => 8,
    };
    hitboxes.push(Hitbox {
        area: Rect {
            x: area.x.saturating_add(1),
            y,
            width: left_width,
            height: 1,
        },
        action: HitAction::ModalButton(left),
    });
    hitboxes.push(Hitbox {
        area: Rect {
            x: area.x.saturating_add(1 + left_width + 2),
            y,
            width: right_width,
            height: 1,
        },
        action: HitAction::ModalButton(right),
    });
}
fn render_interactive(frame: &mut Frame<'_>, app: &App, modal: &ModalState, area: Rect) {
    let active = modal.active_field() == DialogField::Interactive
        || app.is_hovered(HitAction::ModalField(DialogField::Interactive));
    let border = if active {
        app.theme.focus
    } else {
        app.theme.border
    };
    let mark = if modal.interactive { "☑" } else { "☐" };
    frame.render_widget(
        Paragraph::new(format!("{mark} interactive (Space/Enter to toggle)")).block(
            Block::default()
                .title(" Interactive ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        ),
        area,
    );
}

fn render_escape_to_projects(frame: &mut Frame<'_>, app: &App, modal: &ModalState, area: Rect) {
    let active = modal.active_field() == DialogField::EscapeToProjects
        || app.is_hovered(HitAction::ModalField(DialogField::EscapeToProjects));
    let border = if active {
        app.theme.focus
    } else {
        app.theme.border
    };
    let mark = if modal.escape_to_projects {
        "☑"
    } else {
        "☐"
    };
    frame.render_widget(
        Paragraph::new(format!("{mark} Esc from board opens projects")).block(
            Block::default()
                .title(" Escape ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        ),
        area,
    );
}

fn render_textarea(
    frame: &mut Frame<'_>,
    app: &App,
    textarea: &TextArea<'static>,
    area: Rect,
    title: &str,
    active: bool,
) {
    let border = if active {
        app.theme.focus
    } else {
        app.theme.border
    };
    let mut widget = textarea.clone();
    widget.set_block(
        Block::default()
            .title(format!(" {title} "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border)),
    );
    frame.render_widget(&widget, area);
}

fn render_description_textarea(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    area: Rect,
    active: bool,
) {
    let border = if active {
        app.theme.focus
    } else {
        app.theme.border
    };
    modal.description.set_block(
        Block::default()
            .title(" Description (Ctrl+V image paste) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border)),
    );
    frame.render_widget(&modal.description, area);
}

fn render_project_form(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    area: Rect,
    path_label: &str,
    name_label: Option<&str>,
    hitboxes: &mut Vec<Hitbox>,
) {
    let show_path = matches!(
        modal.modal,
        Modal::NewProject | Modal::SetProjectPath { .. }
    );
    let show_name = matches!(modal.modal, Modal::NewProject | Modal::RenameProject { .. });
    let mut constraints = Vec::new();
    if show_path {
        constraints.push(Constraint::Length(3));
    }
    if show_name {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Min(1));
    constraints.push(Constraint::Length(3));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let path_active = modal.active_field() == DialogField::Description
        || app.is_hovered(HitAction::ModalField(DialogField::Description));
    let name_active = modal.active_field() == DialogField::Title
        || app.is_hovered(HitAction::ModalField(DialogField::Title));
    let mut row = 0;
    if show_path {
        render_labeled_textarea(
            frame,
            app,
            &mut modal.description,
            rows[row],
            path_label,
            path_active,
        );
        hitboxes.push(Hitbox {
            area: rows[row],
            action: HitAction::ModalField(DialogField::Description),
        });
        row += 1;
    }
    if show_name {
        let label = name_label.unwrap_or("Display name");
        render_labeled_textarea(frame, app, &mut modal.title, rows[row], label, name_active);
        hitboxes.push(Hitbox {
            area: rows[row],
            action: HitAction::ModalField(DialogField::Title),
        });
        row += 1;
    }
    let _ = row;
    render_form_buttons(
        frame,
        app,
        modal,
        *rows.last().expect("buttons row"),
        hitboxes,
    );
}

fn render_delete_project(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    name: &str,
    task_count: u32,
    hitboxes: &mut Vec<Hitbox>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(area);
    let confirm = if modal.purge_data {
        format!(
            "Delete project {} and its board data?",
            sanitize_terminal_text(name)
        )
    } else {
        format!(
            "Unregister project {}? Board data stays in the store.",
            sanitize_terminal_text(name)
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(confirm),
            Line::from("Space toggles whether board data is deleted."),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );
    let mark = if modal.purge_data { "☑" } else { "☐" };
    let active = modal.active_field() == DialogField::PurgeData
        || app.is_hovered(HitAction::ModalField(DialogField::PurgeData));
    let style = if active {
        Style::default().fg(app.theme.focus)
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{mark} also delete board data ({task_count} tasks)"
        ))
        .style(style),
        rows[1],
    );
    hitboxes.push(Hitbox {
        area: rows[1],
        action: HitAction::ModalField(DialogField::PurgeData),
    });
    let yes = if modal.purge_data {
        "Delete data"
    } else {
        "Unregister"
    };
    render_buttons(
        frame,
        app,
        rows[2],
        yes,
        modal.confirm_yes_selected || app.is_hovered(HitAction::ModalButton(ModalButton::Yes)),
        "Cancel",
        !modal.confirm_yes_selected || app.is_hovered(HitAction::ModalButton(ModalButton::No)),
    );
    register_buttons(hitboxes, rows[2], ModalButton::Yes, ModalButton::No);
}

fn render_labeled_textarea(
    frame: &mut Frame<'_>,
    app: &App,
    textarea: &mut TextArea<'static>,
    area: Rect,
    title: &str,
    active: bool,
) {
    let border = if active {
        app.theme.focus
    } else {
        app.theme.border
    };
    textarea.set_block(
        Block::default()
            .title(format!(" {title} "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border)),
    );
    frame.render_widget(&*textarea, area);
}

fn modal_title(modal: &Modal) -> &'static str {
    match modal {
        Modal::NewTask { .. } => " New task ",
        Modal::EditTask { .. } => " Edit task ",
        Modal::MoveTask { .. } => " Move task ",
        Modal::DeleteConfirm { .. } => " Delete permanently ",
        Modal::RevertConfirm { .. } => " Revert task ",
        Modal::BulkConfirm { .. } => " Confirm bulk action ",
        Modal::KillSessionConfirm { .. } => " Kill session ",
        Modal::RestoreConfirm { .. } => " Restore task ",
        Modal::AddMessage { .. } => " Add to thread ",
        Modal::AnswerQuestion { .. } => " Answer question ",
        Modal::Settings => " Project settings ",
        Modal::GlobalSettings => " Global settings ",
        Modal::NewProject => " New project ",
        Modal::RenameProject { .. } => " Rename project ",
        Modal::SetProjectPath { .. } => " Change project path ",
        Modal::DeleteProject { .. } => " Remove project ",
    }
}

pub(super) fn one_line(text: &str) -> TextArea<'static> {
    TextArea::new(vec![sanitize_terminal_text(
        &text.replace(['\n', '\r'], " "),
    )])
}

fn wrapped_description(lines: Vec<String>) -> TextArea<'static> {
    let mut textarea = TextArea::new(lines);
    textarea.set_wrap_mode(WrapMode::WordOrGlyph);
    textarea
}

fn input_single_line(textarea: &mut TextArea<'static>, key: ratatui::crossterm::event::KeyEvent) {
    if matches!(key.code, ratatui::crossterm::event::KeyCode::Enter) {
        return;
    }
    textarea.input(key);
}

fn textarea_text(textarea: &TextArea<'static>) -> String {
    sanitize_terminal_text(&textarea.lines().join("\n"))
        .trim()
        .to_string()
}

fn raw_textarea_text(textarea: &TextArea<'static>) -> String {
    textarea.lines().join("\n")
}

fn non_empty(text: String) -> Option<String> {
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn selected_value(options: &[SelectOption], selected: usize) -> Option<String> {
    options
        .get(selected)
        .and_then(|option| option.value.clone())
}

fn select_matching(options: &[SelectOption], value: Option<&str>) -> usize {
    value
        .and_then(|value| {
            options
                .iter()
                .position(|option| option.value.as_deref() == Some(value))
        })
        .unwrap_or(0)
}

fn lines_or_empty(text: &str) -> Vec<String> {
    let lines = text.lines().map(sanitize_terminal_text).collect::<Vec<_>>();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}
