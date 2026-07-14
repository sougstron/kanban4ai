use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use tui_textarea::TextArea;

use crate::core::models::Task;
use crate::core::operations::QuestionRef;

use super::app::App;
use super::card::sanitize_terminal_text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    NewTask,
    EditTask {
        task_id: String,
    },
    MoveTask {
        task_id: String,
    },
    DeleteConfirm {
        task_id: String,
    },
    DelegateConfirm {
        task_id: String,
    },
    AnswerQuestion {
        task_id: String,
        questions: Vec<QuestionChoice>,
    },
}

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
    Agent,
    ChainTo,
    Interactive,
    TargetStatus,
    Question,
    Variant,
    Answer,
    Confirm,
}

pub struct ModalState {
    pub modal: Modal,
    pub field_index: usize,
    pub title: TextArea<'static>,
    pub description: TextArea<'static>,
    pub backend: TextArea<'static>,
    pub model: TextArea<'static>,
    pub agent: TextArea<'static>,
    pub chain_to: TextArea<'static>,
    pub target_status: TextArea<'static>,
    pub answer: TextArea<'static>,
    pub interactive: bool,
    pub confirm_text: String,
    pub backend_options: Vec<SelectOption>,
    pub backend_selected: usize,
    pub model_options: Vec<SelectOption>,
    pub model_selected: usize,
    pub agent_options: Vec<SelectOption>,
    pub agent_selected: usize,
    pub chain_options: Vec<SelectOption>,
    pub chain_selected: usize,
    pub status_options: Vec<SelectOption>,
    pub status_selected: usize,
    pub question_selected: usize,
    pub variant_selected: Option<usize>,
}

impl ModalState {
    pub fn new(modal: Modal) -> Self {
        Self {
            modal,
            field_index: 0,
            title: one_line(""),
            description: TextArea::default(),
            backend: one_line(""),
            model: one_line(""),
            agent: one_line(""),
            chain_to: one_line(""),
            target_status: one_line("todo"),
            answer: TextArea::default(),
            interactive: false,
            confirm_text: String::new(),
            backend_options: Vec::new(),
            backend_selected: 0,
            model_options: Vec::new(),
            model_selected: 0,
            agent_options: Vec::new(),
            agent_selected: 0,
            chain_options: Vec::new(),
            chain_selected: 0,
            status_options: Vec::new(),
            status_selected: 0,
            question_selected: 0,
            variant_selected: None,
        }
    }

    pub fn for_task(modal: Modal, task: &Task) -> Self {
        let mut state = Self::new(modal);
        state.title = one_line(&task.title);
        state.description = TextArea::new(lines_or_empty(&task.description));
        state.backend = one_line(task.agent_backend.as_deref().unwrap_or(""));
        state.model = one_line(task.ai_model.as_deref().unwrap_or(""));
        state.agent = one_line(task.agent_name.as_deref().unwrap_or(""));
        state.chain_to = one_line(task.chained_to.as_deref().unwrap_or(""));
        state.interactive = task.interactive;
        state
    }

    pub fn fields(&self) -> &'static [DialogField] {
        match self.modal {
            Modal::NewTask | Modal::EditTask { .. } => &[
                DialogField::Title,
                DialogField::Description,
                DialogField::Backend,
                DialogField::Model,
                DialogField::Agent,
                DialogField::ChainTo,
                DialogField::Interactive,
                DialogField::Confirm,
            ],
            Modal::MoveTask { .. } => &[DialogField::TargetStatus, DialogField::Confirm],
            Modal::DeleteConfirm { .. } | Modal::DelegateConfirm { .. } => &[DialogField::Confirm],
            Modal::AnswerQuestion { .. } => &[
                DialogField::Question,
                DialogField::Variant,
                DialogField::Answer,
                DialogField::Confirm,
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
        }
    }

    pub fn submit_on_enter(&self) -> bool {
        self.active_field() == DialogField::Confirm
    }

    pub fn input(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match self.active_field() {
            DialogField::Title => input_single_line(&mut self.title, key),
            DialogField::Description => {
                self.description.input(key);
            }
            DialogField::Backend => self.input_select(key, SelectorKind::Backend),
            DialogField::Model => self.input_select(key, SelectorKind::Model),
            DialogField::Agent => self.input_select(key, SelectorKind::Agent),
            DialogField::ChainTo => self.input_select(key, SelectorKind::ChainTo),
            DialogField::Interactive => match key.code {
                ratatui::crossterm::event::KeyCode::Char(' ')
                | ratatui::crossterm::event::KeyCode::Enter => self.interactive = !self.interactive,
                _ => {}
            },
            DialogField::TargetStatus => self.input_select(key, SelectorKind::TargetStatus),
            DialogField::Question => self.input_select(key, SelectorKind::Question),
            DialogField::Variant => self.input_select(key, SelectorKind::Variant),
            DialogField::Answer => {
                self.answer.input(key);
            }
            DialogField::Confirm => match key.code {
                ratatui::crossterm::event::KeyCode::Char('y')
                | ratatui::crossterm::event::KeyCode::Char('Y') => {
                    self.confirm_text = "yes".to_string()
                }
                ratatui::crossterm::event::KeyCode::Char('n')
                | ratatui::crossterm::event::KeyCode::Char('N') => {
                    self.confirm_text = "no".to_string()
                }
                _ => {}
            },
        }
    }

    pub fn active_textarea_mut(&mut self) -> &mut TextArea<'static> {
        match self.active_field() {
            DialogField::Title => &mut self.title,
            DialogField::Description => &mut self.description,
            DialogField::Backend => &mut self.backend,
            DialogField::Model => &mut self.model,
            DialogField::Agent => &mut self.agent,
            DialogField::ChainTo => &mut self.chain_to,
            DialogField::Interactive => &mut self.answer,
            DialogField::TargetStatus => &mut self.target_status,
            DialogField::Question | DialogField::Variant => &mut self.answer,
            DialogField::Answer => &mut self.answer,
            DialogField::Confirm => &mut self.answer,
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

    pub fn agent_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.agent))
    }

    pub fn chain_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.chain_to))
    }

    pub fn target_text(&self) -> String {
        textarea_text(&self.target_status)
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

    pub fn confirmed(&self) -> bool {
        self.confirm_text.trim().eq_ignore_ascii_case("yes")
    }

    fn input_select(&mut self, key: ratatui::crossterm::event::KeyEvent, kind: SelectorKind) {
        use ratatui::crossterm::event::KeyCode;
        match key.code {
            KeyCode::Up | KeyCode::Left => self.move_selection(kind, -1),
            KeyCode::Down | KeyCode::Right => self.move_selection(kind, 1),
            KeyCode::Enter | KeyCode::Char(' ') => self.apply_selection(kind),
            _ => {}
        }
    }

    fn move_selection(&mut self, kind: SelectorKind, delta: isize) {
        let len = self.selection_len(kind);
        if len == 0 {
            return;
        }
        let selected = self.selection_mut(kind);
        *selected = if delta.is_negative() {
            selected.saturating_sub(delta.unsigned_abs())
        } else {
            selected.saturating_add(delta as usize).min(len - 1)
        };
        self.apply_selection(kind);
    }

    fn selection_len(&self, kind: SelectorKind) -> usize {
        match kind {
            SelectorKind::Backend => self.backend_options.len(),
            SelectorKind::Model => self.model_options.len(),
            SelectorKind::Agent => self.agent_options.len(),
            SelectorKind::ChainTo => self.chain_options.len(),
            SelectorKind::TargetStatus => self.status_options.len(),
            SelectorKind::Question => match &self.modal {
                Modal::AnswerQuestion { questions, .. } => questions.len(),
                _ => 0,
            },
            SelectorKind::Variant => self.current_variants().len() + 1,
        }
    }

    fn selection_mut(&mut self, kind: SelectorKind) -> &mut usize {
        match kind {
            SelectorKind::Backend => &mut self.backend_selected,
            SelectorKind::Model => &mut self.model_selected,
            SelectorKind::Agent => &mut self.agent_selected,
            SelectorKind::ChainTo => &mut self.chain_selected,
            SelectorKind::TargetStatus => &mut self.status_selected,
            SelectorKind::Question => &mut self.question_selected,
            SelectorKind::Variant => self.variant_selected.get_or_insert(0),
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
}

#[derive(Debug, Clone, Copy)]
enum SelectorKind {
    Backend,
    Model,
    Agent,
    ChainTo,
    TargetStatus,
    Question,
    Variant,
}

pub fn render(frame: &mut Frame<'_>, app: &App, modal: &ModalState, area: Rect) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(modal_title(&modal.modal))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.focus))
        .style(Style::default().bg(app.theme.bg).fg(app.theme.fg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match &modal.modal {
        Modal::NewTask | Modal::EditTask { .. } => render_task_form(frame, app, modal, inner),
        Modal::MoveTask { task_id } => render_move(frame, app, modal, inner, task_id),
        Modal::DeleteConfirm { task_id } => {
            render_confirm(frame, app, modal, inner, "Delete", task_id, true)
        }
        Modal::DelegateConfirm { task_id } => {
            render_confirm(frame, app, modal, inner, "Delegate", task_id, false)
        }
        Modal::AnswerQuestion { task_id, questions } => {
            render_answer(frame, app, modal, inner, task_id, questions)
        }
    }
}

fn render_task_form(frame: &mut Frame<'_>, app: &App, modal: &ModalState, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(area);
    render_textarea(
        frame,
        app,
        &modal.title,
        rows[0],
        "Title",
        modal.active_field() == DialogField::Title,
    );
    render_textarea(
        frame,
        app,
        &modal.description,
        rows[1],
        "Description (Ctrl+V image paste)",
        modal.active_field() == DialogField::Description,
    );
    render_select(
        frame,
        app,
        "Backend",
        &modal.backend_options,
        modal.backend_selected,
        rows[2],
        modal.active_field() == DialogField::Backend,
    );
    render_select(
        frame,
        app,
        "Model",
        &modal.model_options,
        modal.model_selected,
        rows[3],
        modal.active_field() == DialogField::Model,
    );
    render_select(
        frame,
        app,
        "Agent",
        &modal.agent_options,
        modal.agent_selected,
        rows[4],
        modal.active_field() == DialogField::Agent,
    );
    render_select(
        frame,
        app,
        "Chain to",
        &modal.chain_options,
        modal.chain_selected,
        rows[5],
        modal.active_field() == DialogField::ChainTo,
    );
    render_interactive(frame, app, modal, rows[6]);
    render_submit(frame, app, modal, rows[7]);
}

fn render_move(frame: &mut Frame<'_>, app: &App, modal: &ModalState, area: Rect, task_id: &str) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(3),
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
        modal.active_field() == DialogField::TargetStatus,
    );
    render_submit(frame, app, modal, rows[2]);
}

fn render_confirm(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    action: &str,
    task_id: &str,
    destructive: bool,
) {
    let border = if destructive {
        app.theme.err
    } else {
        app.theme.warn
    };
    let text = vec![
        Line::from(format!(
            "{action} task {}?",
            sanitize_terminal_text(task_id)
        )),
        Line::from("Type y then Enter to confirm, n/Esc to cancel."),
        Line::from(format!("Current answer: {}", modal.confirm_text)),
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
}

fn render_answer(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    task_id: &str,
    questions: &[QuestionChoice],
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Min(4),
            Constraint::Length(3),
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
        .map(|question| {
            ListItem::new(format!(
                "{}  {}",
                sanitize_terminal_text(&question.message_id),
                super::card::truncate_display(&sanitize_terminal_text(&question.body), 64)
            ))
        })
        .collect::<Vec<_>>();
    render_list(
        frame,
        app,
        " Questions ",
        question_items,
        modal.question_selected,
        rows[1],
        modal.active_field() == DialogField::Question,
    );
    let mut variant_items = vec![ListItem::new("Custom answer textarea")];
    if let Some(question) = question {
        variant_items.extend(
            question
                .variants
                .iter()
                .map(|variant| ListItem::new(sanitize_terminal_text(variant))),
        );
    }
    render_list(
        frame,
        app,
        " Variants ",
        variant_items,
        modal.variant_selected.unwrap_or(0),
        rows[2],
        modal.active_field() == DialogField::Variant,
    );
    render_textarea(
        frame,
        app,
        &modal.answer,
        rows[3],
        "Custom answer / selected variant",
        modal.active_field() == DialogField::Answer,
    );
    render_submit(frame, app, modal, rows[4]);
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
    let items = if options.is_empty() {
        vec![ListItem::new("-")]
    } else {
        options
            .iter()
            .map(|option| ListItem::new(sanitize_terminal_text(&option.label)))
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
    state.select(Some(selected.min(items.len().saturating_sub(1))));
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

fn render_submit(frame: &mut Frame<'_>, app: &App, modal: &ModalState, area: Rect) {
    let active = modal.active_field() == DialogField::Confirm;
    let style = if active {
        Style::default()
            .fg(app.theme.focus)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.muted)
    };
    frame.render_widget(
        Paragraph::new("Enter: save/confirm · Tab: next field · Esc: cancel")
            .block(Block::default().borders(Borders::ALL).border_style(style))
            .style(style),
        area,
    );
}

fn render_interactive(frame: &mut Frame<'_>, app: &App, modal: &ModalState, area: Rect) {
    let active = modal.active_field() == DialogField::Interactive;
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

fn modal_title(modal: &Modal) -> &'static str {
    match modal {
        Modal::NewTask => " New task ",
        Modal::EditTask { .. } => " Edit task ",
        Modal::MoveTask { .. } => " Move task ",
        Modal::DeleteConfirm { .. } => " Delete task ",
        Modal::DelegateConfirm { .. } => " Delegate task ",
        Modal::AnswerQuestion { .. } => " Answer question ",
    }
}

fn one_line(text: &str) -> TextArea<'static> {
    TextArea::new(vec![sanitize_terminal_text(
        &text.replace(['\n', '\r'], " "),
    )])
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
