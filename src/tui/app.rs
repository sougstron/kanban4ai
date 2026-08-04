use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui_textarea::TextArea;
use serde_yaml_ng::{Mapping, Value};
use unicode_width::UnicodeWidthStr;

use crate::agent::{
    backend_has_catalog, cached_backend_catalog, recent_models, sort_opencode_models,
    warm_backend_catalog,
};
use crate::core::config::BoardConfig;
use crate::core::context::ContextManager;
use crate::core::error::{KanbanError, Result};
use crate::core::models::{
    Message, MessageKind, MessageStatus, Session, SessionStatus, Task, TaskStatus,
};
use crate::core::operations::{Operations, QuestionRef, TaskPatch};
use crate::core::provenance::{self, InputManifest};
use crate::core::session::{SessionManager, SessionState};
use crate::core::storage::NewTask;
use crate::core::telemetry::{self, SessionProgress};
use crate::core::thread::ThreadManager;
use crate::core::timefmt;

use super::card::{
    case_insensitive_match, sanitize_paste_text, sanitize_terminal_text, truncate_display,
};
use super::dialogs::{
    BulkAction, DialogField, Modal, ModalButton, ModalState, QuestionChoice, SelectOption,
};
use super::image;
use super::search::SearchState;
use super::theme::Theme;

const CTRL_C_EXIT_PROMPT: &str = "Press ctrl + C again to close";
const CTRL_C_EXIT_WINDOW: Duration = Duration::from_secs(3);
const COPY_NOTICE: &str = "Copied selected text to clipboard";
const COPY_NOTICE_WINDOW: Duration = Duration::from_secs(3);
const TASK_SORT_NUMBER: &str = "task_number";
const TASK_SORT_UPDATED_ASC: &str = "updated_at_asc";
const TASK_SORT_UPDATED_DESC: &str = "updated_at_desc";
const TASK_SORT_LEGACY_COMPLETION: &str = "completion_date";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Board,
    Detail,
    Sessions,
    Archive,
    LogView,
    /// Read-only pager over a static block of text (the assembled prompt or the
    /// gathered context of a task), opened from the detail view.
    TextView,
    Help,
}

#[derive(Debug, Clone)]
pub struct TuiSettings {
    pub project_name: String,
    pub card_height_lines: u16,
    pub max_tasks_per_column: usize,
    pub refresh_interval: Duration,
    pub theme_name: String,
    pub task_sort: String,
}

#[derive(Debug, Clone)]
pub struct BoardColumn {
    pub id: String,
    pub name: String,
    pub tasks: Vec<Task>,
}

/// Per-card decorations that need data beyond the task file: the first open
/// question for the preview line, and whether an interactive agent is blocked
/// waiting for an answer. Computed once per snapshot, not per frame.
#[derive(Debug, Clone, Default)]
pub struct CardExtra {
    pub question_preview: Option<String>,
    pub waiting: bool,
}

#[derive(Debug, Clone)]
pub struct BoardSnapshot {
    pub columns: Vec<BoardColumn>,
    pub extras: HashMap<String, CardExtra>,
    pub session_states: HashMap<String, SessionState>,
    pub session_deadlines: HashMap<String, chrono::NaiveDateTime>,
    pub session_wait_deadlines: HashMap<String, chrono::NaiveDateTime>,
    pub session_wait_notes: HashMap<String, String>,
    pub fingerprint: (u64, u128),
}

/// A clickable region registered by the renderers on every frame. Searched
/// front-to-back, so more specific regions (cards, later buttons) must be
/// pushed before the enclosing areas (columns) that contain them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hitbox {
    pub area: Rect,
    pub action: HitAction,
}

/// What a mouse press (or wheel, for column targeting) on a region maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitAction {
    FocusCard {
        column: usize,
        card: usize,
    },
    /// The question-preview line on a card: opens detail on its answer panel.
    OpenAnswer {
        column: usize,
        card: usize,
    },
    ColumnFocus(usize),
    Action(UiAction),
    ModalField(DialogField),
    ModalOption {
        field: DialogField,
        index: usize,
    },
    ModalButton(ModalButton),
    DetailAnswerOption {
        index: usize,
    },
    DetailThread,
    DetailEdits,
}

/// A user-level action, triggered equally by a hotkey, a button click, or a
/// status-bar hint. Key handlers and hitboxes both route through
/// [`App::dispatch`] so every trigger stays in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    Help,
    Search,
    OpenDetail,
    NewTask,
    EditTask,
    MoveTask,
    DeleteTask,
    Run,
    Revoke,
    AnswerQuestion,
    Recover,
    Approve,
    Attach,
    AddContext,
    Rerun,
    Revert,
    OpenArchive,
    OpenSessions,
    CycleTheme,
    OpenSettings,
    SaveReviewEdits,
    ArchiveAllDone,
    MarkReviewDone,
    FocusQuestions,
    ClearSearch,
    ViewLog,
    KillSession,
    OpenSessionTask,
    Restore,
    ToggleReject,
    ViewPrompt,
    ViewContext,
}

/// Which detail panel receives keyboard input. `Thread` is the neutral state
/// where action hotkeys work; the other two are text-entry panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailFocus {
    Thread,
    Answer,
    Edits,
}

#[derive(Clone)]
pub struct DetailState {
    pub task_id: String,
    pub task: Option<Task>,
    pub messages: Vec<Message>,
    /// Index into `messages` of the message `x` will toggle-reject.
    pub thread_selected: usize,
    pub scroll: u16,
    /// Upper scroll bound, set by the renderer from the thread content height.
    pub max_scroll: u16,
    pub review_edits: TextArea<'static>,
    pub focus: DetailFocus,
    pub answer_input: TextArea<'static>,
    pub question_index: usize,
    /// 0 = custom answer input, 1.. = the question's variants.
    pub variant_selected: usize,
    /// A prompt dump exists for this task (drives the "Prompt" view button).
    pub has_prompt: bool,
    /// Harvested input-provenance exists for this task (drives the "Inputs"
    /// view button and the `v` popup).
    pub has_provenance: bool,
    /// Input-provenance manifests harvested from each of this task's agent
    /// runs (what was actually consumed: files/URLs/commands/MCP). Rendered as
    /// a section above the thread, sourced from the manifests — never mixed
    /// into the conversation messages.
    pub provenance: Vec<InputManifest>,
}

impl DetailState {
    pub fn open_questions(&self) -> Vec<&Message> {
        self.messages
            .iter()
            .filter(|message| {
                message.kind == MessageKind::Question && message.status == MessageStatus::Open
            })
            .collect()
    }

    pub fn edits_editable(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(|task| task.status == TaskStatus::Review)
    }

    pub fn show_edits_panel(&self) -> bool {
        self.edits_editable() || !self.review_edits.lines().join("").trim().is_empty()
    }

    fn focus_available(&self, focus: DetailFocus) -> bool {
        match focus {
            DetailFocus::Thread => true,
            DetailFocus::Answer => !self.open_questions().is_empty(),
            DetailFocus::Edits => self.edits_editable(),
        }
    }
}

/// A terminal-taking action the event loop runs after suspending the TUI.
/// `Attach` re-enters a live tmux session; `Foreground` runs an arbitrary
/// command to completion (e.g. `claude --resume <id>` for a stopped agent).
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalAction {
    Attach(String),
    Foreground {
        command: String,
        args: Vec<String>,
        cwd: PathBuf,
        label: String,
    },
}

impl TerminalAction {
    /// Human-readable target for the post-run status line.
    pub fn label(&self) -> String {
        match self {
            TerminalAction::Attach(session_id) => session_id.clone(),
            TerminalAction::Foreground { label, .. } => label.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ActiveSession {
    pub session: Session,
    pub state: SessionState,
    pub task_label: String,
    pub token_display: String,
    /// Live telemetry for the Sessions list (todo progress + last activity),
    /// derived from the transcript like the card telemetry.
    pub progress: SessionProgress,
}

/// Pager over the tail of `.kanban/logs/<session>.log`. While `follow` is on
/// the view stays pinned to the freshest output as the tick reloads it.
#[derive(Clone)]
pub struct LogViewState {
    pub session_id: String,
    pub lines: Vec<String>,
    pub scroll: u16,
    /// Upper scroll bound, set by the renderer from the viewport height.
    pub max_scroll: u16,
    pub follow: bool,
}

/// Read-only pager over a static text block (a task's assembled prompt or its
/// gathered context). Unlike [`LogViewState`] the content is captured once when
/// the view opens and never follows.
#[derive(Clone)]
pub struct TextViewState {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: u16,
    /// Upper scroll bound, set by the renderer from the viewport height.
    pub max_scroll: u16,
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
    pub visible_card_capacities: Vec<usize>,
    pub search: SearchState,
    pub modal: Option<ModalState>,
    pub detail: Option<DetailState>,
    pub active_sessions: Vec<ActiveSession>,
    pub session_selected: usize,
    /// Live per-task agent telemetry (todo progress, tokens, last activity),
    /// keyed by task id and refreshed each tick for tasks with a running
    /// agent. Derived from the transcript; never persisted.
    pub session_progress: HashMap<String, SessionProgress>,
    pub archived_tasks: Vec<Task>,
    pub archive_selected: usize,
    pub should_quit: bool,
    pub status: String,
    pub hitboxes: Vec<Hitbox>,
    hovered: Option<HitAction>,
    pub dragging: Option<DragState>,
    rendered_screen: RenderedScreen,
    text_selection: Option<TextSelection>,
    pending_copy: Option<String>,
    copy_notice_deadline: Option<Instant>,
    status_before_copy: Option<String>,
    pub log_view: Option<LogViewState>,
    pub text_view: Option<TextViewState>,
    /// Screen the text pager (`q`) returns to; set by `open_text_view` from its
    /// caller (Detail for prompt/inputs, Sessions for the session-info panel).
    text_view_return: Screen,
    /// Where closing the detail screen returns to (Sessions `o`, Archive
    /// Enter); reset to Board once consumed.
    pub return_screen: Screen,
    pub help_scroll: u16,
    /// Upper help scroll bound, set by the renderer from the overlay height.
    pub help_max_scroll: u16,
    pending_terminal: Option<TerminalAction>,
    pending_fs_reload: bool,
    fs_change_generation: u64,
    recent_models: Vec<String>,
    ctrl_c_exit_deadline: Option<Instant>,
    /// Last time the tick scanned for expired declared waits to relaunch.
    last_wait_resume: Option<Instant>,
    /// Backends whose warmed model catalog has already been reflected into an
    /// open modal, so `tick` refreshes options at most once per backend.
    catalog_ready: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragState {
    pub task_id: String,
    pub from_column: usize,
    pub card: usize,
    pub target_column: Option<usize>,
    pub moved: bool,
}

#[derive(Debug, Clone, Default)]
struct RenderedScreen {
    area: Rect,
    cells: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextSelection {
    anchor: (u16, u16),
    head: (u16, u16),
    dragged: bool,
}

impl RenderedScreen {
    fn capture(&mut self, buffer: &Buffer) {
        self.area = buffer.area;
        self.cells.clear();
        self.cells
            .reserve(usize::from(self.area.width).saturating_mul(usize::from(self.area.height)));
        for y in self.area.y..self.area.y.saturating_add(self.area.height) {
            for x in self.area.x..self.area.x.saturating_add(self.area.width) {
                self.cells.push(
                    buffer
                        .cell((x, y))
                        .map(|cell| cell.symbol().to_string())
                        .unwrap_or_default(),
                );
            }
        }
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        contains(self.area, x, y)
    }

    fn clamp(&self, x: u16, y: u16) -> (u16, u16) {
        let right = self
            .area
            .x
            .saturating_add(self.area.width.saturating_sub(1));
        let bottom = self
            .area
            .y
            .saturating_add(self.area.height.saturating_sub(1));
        (x.clamp(self.area.x, right), y.clamp(self.area.y, bottom))
    }

    fn positions(&self, selection: TextSelection) -> Vec<(u16, u16)> {
        if self.area.is_empty() {
            return Vec::new();
        }
        let (start, end) = ordered_points(
            self.clamp(selection.anchor.0, selection.anchor.1),
            self.clamp(selection.head.0, selection.head.1),
        );
        let left = self.area.x;
        let right = self
            .area
            .x
            .saturating_add(self.area.width.saturating_sub(1));
        let mut positions = Vec::new();
        for y in start.1..=end.1 {
            let row_start = if y == start.1 { start.0 } else { left };
            let row_end = if y == end.1 { end.0 } else { right };
            positions.extend((row_start..=row_end).map(|x| (x, y)));
        }
        positions
    }

    fn selected_text(&self, selection: TextSelection) -> String {
        let mut lines = Vec::new();
        let mut current_y = None;
        let mut line = String::new();
        let mut skip_cells = 0usize;
        for (x, y) in self.positions(selection) {
            if current_y != Some(y) {
                if current_y.is_some() {
                    lines.push(line.trim_end().to_string());
                    line.clear();
                }
                current_y = Some(y);
                skip_cells = 0;
            }
            if skip_cells > 0 {
                skip_cells -= 1;
                continue;
            }
            let Some(symbol) = self.cell(x, y) else {
                continue;
            };
            line.push_str(symbol);
            skip_cells = UnicodeWidthStr::width(symbol).saturating_sub(1);
        }
        if current_y.is_some() {
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n").trim_matches('\n').to_string()
    }

    fn cell(&self, x: u16, y: u16) -> Option<&str> {
        if !self.contains(x, y) {
            return None;
        }
        let row = usize::from(y.saturating_sub(self.area.y));
        let column = usize::from(x.saturating_sub(self.area.x));
        let index = row
            .checked_mul(usize::from(self.area.width))?
            .checked_add(column)?;
        self.cells.get(index).map(String::as_str)
    }
}

fn ordered_points(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

impl App {
    pub fn new(project_path: &Path) -> Result<Self> {
        let ops = Operations::new(project_path);
        let settings = load_settings(&ops)?;
        let theme = Theme::named(&settings.theme_name);
        let board = BoardSnapshot::load(&ops)?;
        let archived_tasks = ops.list_archived_tasks(None)?;
        let recent_models = recent_models(project_path);
        let catalog_backends = ops
            .config
            .load()
            .ok()
            .map(|config| catalog_backend_commands(&config))
            .unwrap_or_default();
        let mut catalog_ready = HashSet::new();
        for (backend, command) in &catalog_backends {
            if cached_backend_catalog(backend, command).is_some() {
                catalog_ready.insert(backend.clone());
            } else {
                warm_backend_catalog(backend.clone(), command.clone());
            }
        }
        let column_offsets = vec![0; board.columns.len()];
        let visible_card_capacities = vec![1; board.columns.len()];
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
            visible_card_capacities,
            search: SearchState::default(),
            modal: None,
            detail: None,
            active_sessions: Vec::new(),
            session_selected: 0,
            session_progress: HashMap::new(),
            archived_tasks,
            archive_selected: 0,
            should_quit: false,
            status: "TUI ready".to_string(),
            hitboxes: Vec::new(),
            hovered: None,
            dragging: None,
            rendered_screen: RenderedScreen::default(),
            text_selection: None,
            pending_copy: None,
            copy_notice_deadline: None,
            status_before_copy: None,
            log_view: None,
            text_view: None,
            text_view_return: Screen::Detail,
            return_screen: Screen::Board,
            help_scroll: 0,
            help_max_scroll: 0,
            pending_terminal: None,
            pending_fs_reload: false,
            fs_change_generation: 0,
            recent_models,
            ctrl_c_exit_deadline: None,
            last_wait_resume: None,
            catalog_ready,
        })
    }

    /// Insert bracketed-paste text into whatever text field has focus.
    ///
    /// Without this the terminal replays the clipboard as key events: tabs hop
    /// between dialog fields, newlines press the focused button, and on the
    /// board every character fires its shortcut. Pastes that arrive with no
    /// text field focused are dropped instead of being executed.
    pub fn handle_paste(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if let Some(mut modal) = self.modal.take() {
            let inserted =
                !modal.discard_confirm && !is_confirmation_modal(&modal.modal) && modal.paste(text);
            self.modal = Some(modal);
            if !inserted {
                self.status = "Nothing pasted: focus a text field first".to_string();
            }
            return Ok(());
        }
        if self.search.active {
            self.search.query.insert_str(one_line_paste(text));
            return Ok(());
        }
        if self.screen == Screen::Detail
            && let Some(detail) = self.detail.as_mut()
        {
            match detail.focus {
                DetailFocus::Answer => {
                    detail.answer_input.insert_str(one_line_paste(text));
                    detail.variant_selected = 0;
                    return Ok(());
                }
                DetailFocus::Edits => {
                    detail.review_edits.insert_str(sanitize_paste_text(text));
                    return Ok(());
                }
                DetailFocus::Thread => {}
            }
        }
        self.status = "Nothing pasted: focus a text field first".to_string();
        Ok(())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if is_ctrl_c(key) {
            self.handle_ctrl_c_exit();
            return Ok(());
        }
        if self.handle_modal_key(key)? {
            return Ok(());
        }
        if self.search.active {
            return self.handle_search_key(key);
        }
        if matches!(
            normalize_command_key(key),
            KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) {
            return self.dispatch(UiAction::CycleTheme);
        }
        if self.screen == Screen::Detail && self.handle_detail_key(key)? {
            return Ok(());
        }
        let key = normalize_command_key(key);
        if self.screen == Screen::LogView {
            return self.handle_log_key(key);
        }
        if self.screen == Screen::TextView {
            return self.handle_text_view_key(key);
        }
        if self.screen == Screen::Sessions {
            return self.handle_sessions_key(key);
        }
        let action_screen = matches!(self.screen, Screen::Board | Screen::Detail);
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) if self.screen == Screen::Board && !self.search.text().is_empty() => {
                self.clear_search();
            }
            (KeyCode::Esc, _) if self.screen == Screen::Detail => self.close_detail()?,
            (KeyCode::Char('q'), _) => {
                if self.screen == Screen::Detail {
                    self.close_detail()?;
                } else if self.screen != Screen::Board {
                    self.screen = Screen::Board;
                    self.detail = None;
                } else {
                    self.status = "Press ctrl + C twice to close".to_string();
                }
            }
            (KeyCode::Esc, _) if matches!(self.screen, Screen::Archive | Screen::Help) => {
                self.screen = Screen::Board;
                self.detail = None;
            }
            (KeyCode::Char('?'), _) => self.dispatch(UiAction::Help)?,
            (KeyCode::Char('s'), KeyModifiers::CONTROL) if self.screen == Screen::Detail => {
                self.dispatch(UiAction::SaveReviewEdits)?
            }
            (KeyCode::Char('s'), _) if action_screen => self.dispatch(UiAction::OpenSettings)?,
            (KeyCode::Char('r'), KeyModifiers::CONTROL) if action_screen => {
                self.dispatch(UiAction::Rerun)?
            }
            (KeyCode::Char('/'), _) => self.dispatch(UiAction::Search)?,
            (KeyCode::Enter, _)
                if self.screen == Screen::Detail
                    && self
                        .current_task()
                        .is_some_and(|task| task.status == TaskStatus::Todo) =>
            {
                self.dispatch(UiAction::Run)?
            }
            (KeyCode::Enter, _) if self.screen != Screen::Detail => {
                self.dispatch(UiAction::OpenDetail)?
            }

            (KeyCode::Char('n'), _) if action_screen => self.dispatch(UiAction::NewTask)?,
            (KeyCode::Char('e'), _) if action_screen => self.dispatch(UiAction::EditTask)?,
            (KeyCode::Char('m'), _) if action_screen => self.dispatch(UiAction::MoveTask)?,
            (KeyCode::Char('r'), _) if action_screen => {
                let action = if self
                    .current_task()
                    .is_some_and(|task| task.status == TaskStatus::InProgress)
                {
                    UiAction::Revoke
                } else {
                    UiAction::Run
                };
                self.dispatch(action)?
            }
            (KeyCode::Char('w'), _) if action_screen => self.dispatch(UiAction::AnswerQuestion)?,
            (KeyCode::Char('y'), _) if action_screen => self.dispatch(UiAction::Approve)?,
            (KeyCode::Char('t'), _) if action_screen => self.dispatch(UiAction::Attach)?,
            (KeyCode::Char('c'), _) if action_screen => self.dispatch(UiAction::AddContext)?,
            (KeyCode::Char('u'), _) if action_screen => self.dispatch(UiAction::Recover)?,
            (KeyCode::Char('['), _) if self.screen == Screen::Detail => {
                self.move_thread_selection(-1)
            }
            (KeyCode::Char(']'), _) if self.screen == Screen::Detail => {
                self.move_thread_selection(1)
            }
            (KeyCode::Char('x'), _) if self.screen == Screen::Detail => {
                self.dispatch(UiAction::ToggleReject)?
            }
            (KeyCode::Char('p'), _) if self.screen == Screen::Detail => {
                self.dispatch(UiAction::ViewPrompt)?
            }
            (KeyCode::Char('v'), _) if self.screen == Screen::Detail => {
                self.dispatch(UiAction::ViewContext)?
            }
            (KeyCode::Char('u'), _) if self.screen == Screen::Archive => {
                self.dispatch(UiAction::Restore)?
            }
            (KeyCode::Char('d'), _) | (KeyCode::Delete, _) if action_screen => {
                self.dispatch(UiAction::DeleteTask)?;
            }
            (KeyCode::Char('A'), _) if self.screen == Screen::Board => {
                self.dispatch(UiAction::ArchiveAllDone)?
            }
            (KeyCode::Char('b'), _) | (KeyCode::Char('R'), _) if self.screen == Screen::Board => {
                self.dispatch(UiAction::MarkReviewDone)?
            }
            (KeyCode::Char('a'), _) => self.dispatch(UiAction::OpenArchive)?,
            (KeyCode::Char('l'), _) => self.dispatch(UiAction::OpenSessions)?,
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

    fn handle_ctrl_c_exit(&mut self) {
        let now = Instant::now();
        if self
            .ctrl_c_exit_deadline
            .is_some_and(|deadline| now <= deadline)
        {
            self.ctrl_c_exit_deadline = None;
            self.should_quit = true;
        } else {
            self.ctrl_c_exit_deadline = Some(now + CTRL_C_EXIT_WINDOW);
            self.status = CTRL_C_EXIT_PROMPT.to_string();
        }
    }

    /// Detail-screen key routing for the text-entry panels. Returns `true`
    /// when the key was consumed; `Thread` focus falls through to the main
    /// hotkey match. Receives the raw (non-normalized) key so Cyrillic input
    /// reaches the textareas untouched.
    fn handle_detail_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(detail) = self.detail.as_ref() else {
            return Ok(false);
        };
        if key.code == KeyCode::Tab {
            self.cycle_detail_focus();
            return Ok(true);
        }
        match detail.focus {
            DetailFocus::Thread => Ok(false),
            DetailFocus::Answer => {
                self.handle_answer_key(key)?;
                Ok(true)
            }
            DetailFocus::Edits => {
                let norm = normalize_command_key(key);
                if norm.modifiers == KeyModifiers::CONTROL {
                    match norm.code {
                        KeyCode::Char('s') => self.dispatch(UiAction::SaveReviewEdits)?,
                        KeyCode::Char('r') => self.dispatch(UiAction::Rerun)?,
                        _ => {}
                    }
                    return Ok(true);
                }
                if key.code == KeyCode::Esc {
                    self.set_detail_focus(DetailFocus::Thread);
                    return Ok(true);
                }
                if is_text_input_key(key) {
                    self.input_review_edits(key);
                }
                Ok(true)
            }
        }
    }

    fn cycle_detail_focus(&mut self) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        let order = [DetailFocus::Thread, DetailFocus::Answer, DetailFocus::Edits];
        let current = order
            .iter()
            .position(|focus| *focus == detail.focus)
            .unwrap_or(0);
        for step in 1..=order.len() {
            let candidate = order[(current + step) % order.len()];
            if detail.focus_available(candidate) {
                self.set_detail_focus(candidate);
                return;
            }
        }
    }

    fn set_detail_focus(&mut self, focus: DetailFocus) {
        if let Some(detail) = self.detail.as_mut() {
            detail.focus = if detail.focus_available(focus) {
                focus
            } else {
                DetailFocus::Thread
            };
            self.status = match self.detail.as_ref().map(|detail| detail.focus) {
                Some(DetailFocus::Answer) => "Answer panel focused".to_string(),
                Some(DetailFocus::Edits) => "Review editor focused".to_string(),
                _ => "Thread focused".to_string(),
            };
        }
    }

    fn handle_answer_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.set_detail_focus(DetailFocus::Thread),
            KeyCode::Enter => self.submit_detail_answer()?,
            KeyCode::Left => self.switch_detail_question(-1),
            KeyCode::Right => self.switch_detail_question(1),
            KeyCode::Up => self.move_detail_variant(-1),
            KeyCode::Down => self.move_detail_variant(1),
            _ => {
                if is_text_input_key(key)
                    && let Some(detail) = self.detail.as_mut()
                {
                    detail.answer_input.input(key);
                    // Typing switches the submission source back to the
                    // custom input.
                    detail.variant_selected = 0;
                }
            }
        }
        Ok(())
    }

    fn switch_detail_question(&mut self, delta: isize) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        let count = detail.open_questions().len();
        if count == 0 {
            return;
        }
        detail.question_index = if delta.is_negative() {
            detail.question_index.saturating_sub(delta.unsigned_abs())
        } else {
            detail
                .question_index
                .saturating_add(delta.unsigned_abs())
                .min(count - 1)
        };
        detail.variant_selected = 0;
        detail.answer_input = TextArea::default();
    }

    fn move_detail_variant(&mut self, delta: isize) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        let variant_count = detail
            .open_questions()
            .get(detail.question_index)
            .map(|question| question.variants.len())
            .unwrap_or(0);
        detail.variant_selected = if delta.is_negative() {
            detail.variant_selected.saturating_sub(delta.unsigned_abs())
        } else {
            detail
                .variant_selected
                .saturating_add(delta.unsigned_abs())
                .min(variant_count)
        };
    }

    fn submit_detail_answer(&mut self) -> Result<()> {
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        let task_id = detail.task_id.clone();
        let questions = detail.open_questions();
        let Some(question) = questions.get(detail.question_index).copied() else {
            self.status = "No open question selected".to_string();
            return Ok(());
        };
        let answer = if detail.variant_selected > 0 {
            question
                .variants
                .get(detail.variant_selected - 1)
                .cloned()
                .unwrap_or_default()
        } else {
            sanitize_terminal_text(&detail.answer_input.lines().join("\n"))
                .trim()
                .to_string()
        };
        if answer.is_empty() {
            self.status = "Answer cannot be empty".to_string();
            return Ok(());
        }
        let msg_id = question.id.clone();
        self.ops
            .answer_question(&task_id, QuestionRef::MsgId(msg_id), &answer)?;
        self.refresh_after_action()?;
        let remaining = self
            .detail
            .as_ref()
            .map(|detail| detail.open_questions().len())
            .unwrap_or(0);
        if remaining > 0 {
            self.set_detail_focus(DetailFocus::Answer);
            self.status = format!("Answered; {remaining} question(s) left on {task_id}");
        } else {
            self.set_detail_focus(DetailFocus::Thread);
            self.status = format!("Answered question on {task_id}");
        }
        Ok(())
    }

    fn handle_sessions_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = Screen::Board;
            }
            KeyCode::Char('/') => self.dispatch(UiAction::Search)?,
            KeyCode::Char('x') => self.dispatch(UiAction::KillSession)?,
            KeyCode::Char('v') => self.dispatch(UiAction::ViewLog)?,
            KeyCode::Char('o') => self.dispatch(UiAction::OpenSessionTask)?,
            KeyCode::Char('i') => self.open_session_info()?,
            KeyCode::Enter => self.open_focused_detail()?,
            KeyCode::Up => self.focus_up(),
            KeyCode::Down => self.focus_down(),
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.page_down(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            _ => {}
        }
        Ok(())
    }

    /// Log-view pager keys: scroll around the cached tail, `q`/`Esc` back to
    /// the sessions list. `End` re-enables follow mode.
    fn handle_log_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.log_view = None;
                self.screen = Screen::Sessions;
                self.refresh_active_sessions()?;
            }
            KeyCode::Up => self.scroll_log(-1),
            KeyCode::Down => self.scroll_log(1),
            KeyCode::PageUp => self.scroll_log(-10),
            KeyCode::PageDown => self.scroll_log(10),
            KeyCode::Home => {
                if let Some(log) = self.log_view.as_mut() {
                    log.scroll = 0;
                    log.follow = false;
                }
            }
            KeyCode::End => {
                if let Some(log) = self.log_view.as_mut() {
                    log.scroll = log.max_scroll;
                    log.follow = true;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn scroll_log(&mut self, delta: i32) {
        if let Some(log) = self.log_view.as_mut() {
            if delta < 0 {
                log.scroll = log.scroll.saturating_sub(delta.unsigned_abs() as u16);
                log.follow = false;
            } else {
                log.scroll = log.scroll.saturating_add(delta as u16).min(log.max_scroll);
                log.follow = log.scroll >= log.max_scroll;
            }
        }
    }

    /// Single entry point for user-level actions; see [`UiAction`].
    pub fn dispatch(&mut self, action: UiAction) -> Result<()> {
        match action {
            UiAction::Help => {
                if self.screen == Screen::Help {
                    self.screen = Screen::Board;
                } else {
                    self.help_scroll = 0;
                    self.screen = Screen::Help;
                }
            }
            UiAction::Search => self.search.active = true,
            UiAction::OpenDetail => self.open_focused_detail()?,
            UiAction::NewTask => self.open_new_dialog(),
            UiAction::EditTask => self.open_edit_dialog(),
            UiAction::MoveTask => self.open_move_dialog(),
            UiAction::DeleteTask => self.open_delete_dialog(),
            // The board is human-managed and agent-executed: running a task
            // is the primary action and never asks for confirmation.
            UiAction::Run => self.run_current_task()?,
            UiAction::Revoke => self.revoke_current_task()?,
            UiAction::AnswerQuestion => self.open_answer_dialog()?,
            UiAction::Recover => self.recover_current_task()?,
            UiAction::Approve => self.approve_current_task()?,
            UiAction::Attach => self.attach_current_task()?,
            UiAction::AddContext => self.open_add_message_dialog(),
            UiAction::Rerun => self.rerun_current_task()?,
            UiAction::Revert => self.open_revert_dialog(),
            UiAction::OpenArchive => self.open_archive()?,
            UiAction::OpenSessions => self.open_sessions()?,
            UiAction::CycleTheme => self.cycle_theme()?,
            UiAction::OpenSettings => self.open_settings_dialog(),
            UiAction::SaveReviewEdits => self.save_review_edits()?,
            UiAction::ArchiveAllDone => self.open_bulk_confirm(BulkAction::ArchiveAllDone),
            UiAction::MarkReviewDone => self.open_bulk_confirm(BulkAction::MarkReviewDone),
            UiAction::FocusQuestions => self.focus_first_question(),
            UiAction::ClearSearch => self.clear_search(),
            UiAction::ViewLog => self.open_log_view(),
            UiAction::KillSession => self.open_kill_confirm(),
            UiAction::OpenSessionTask => self.open_session_task_detail()?,
            UiAction::Restore => self.open_restore_confirm(),
            UiAction::ToggleReject => self.toggle_reject_selected_message()?,
            UiAction::ViewPrompt => self.open_prompt_view()?,
            UiAction::ViewContext => self.open_context_view()?,
        }
        Ok(())
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if self.handle_text_selection(mouse) {
            return Ok(());
        }
        if self.modal.is_some() {
            match mouse.kind {
                MouseEventKind::Moved => self.update_hover(mouse.column, mouse.row),
                MouseEventKind::Down(MouseButton::Left) => {
                    self.update_hover(mouse.column, mouse.row);
                    self.handle_modal_click(mouse.column, mouse.row)?;
                }
                _ => {}
            }
            // A modal owns all mouse input; board cards and drag targets must
            // never receive a click-through event.
            return Ok(());
        }
        if self.screen == Screen::Help {
            self.hovered = None;
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::Moved => self.update_hover(mouse.column, mouse.row),
            MouseEventKind::Down(MouseButton::Left) => {
                self.update_hover(mouse.column, mouse.row);
                self.mouse_down(mouse.column, mouse.row)?;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.update_hover(mouse.column, mouse.row);
                self.update_drag_target(mouse.column, mouse.row)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.update_hover(mouse.column, mouse.row);
                self.finish_drag(mouse.column, mouse.row)?;
            }
            MouseEventKind::ScrollUp => self.scroll_at(mouse.column, mouse.row, -1),
            MouseEventKind::ScrollDown => self.scroll_at(mouse.column, mouse.row, 1),
            _ => {}
        }
        Ok(())
    }

    fn handle_text_selection(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if self.can_start_text_selection(
                    mouse.column,
                    mouse.row,
                    mouse.modifiers.contains(KeyModifiers::SHIFT),
                ) =>
            {
                self.text_selection = Some(TextSelection {
                    anchor: (mouse.column, mouse.row),
                    head: (mouse.column, mouse.row),
                    dragged: false,
                });
                // Shift explicitly chooses text selection over an otherwise
                // interactive region such as a card or button.
                mouse.modifiers.contains(KeyModifiers::SHIFT)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.text_selection.is_some() => {
                let crossing_card_columns = !mouse.modifiers.contains(KeyModifiers::SHIFT)
                    && self.dragging.as_ref().is_some_and(|dragging| {
                        self.column_at(mouse.column, mouse.row)
                            .is_some_and(|column| column != dragging.from_column)
                    });
                if crossing_card_columns {
                    self.text_selection = None;
                    return false;
                }
                if let Some(selection) = self.text_selection.as_mut() {
                    selection.head = self.rendered_screen.clamp(mouse.column, mouse.row);
                    selection.dragged = selection.head != selection.anchor;
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.text_selection.is_some() => {
                let selection = self.text_selection.take().expect("selection checked above");
                if selection.dragged {
                    self.dragging = None;
                    let text = self.rendered_screen.selected_text(selection);
                    if !text.is_empty() {
                        self.pending_copy = Some(text);
                    }
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn can_start_text_selection(&self, x: u16, y: u16, shift: bool) -> bool {
        if !self.rendered_screen.contains(x, y) {
            return false;
        }
        match self.hit_at(x, y) {
            Some(HitAction::FocusCard { .. }) => true,
            Some(
                HitAction::OpenAnswer { .. }
                | HitAction::Action(_)
                | HitAction::ModalField(_)
                | HitAction::ModalOption { .. }
                | HitAction::ModalButton(_)
                | HitAction::DetailAnswerOption { .. }
                | HitAction::DetailEdits,
            ) => shift,
            Some(HitAction::ColumnFocus(_) | HitAction::DetailThread) | None => true,
        }
    }

    pub(crate) fn capture_and_highlight(&mut self, buffer: &mut Buffer) {
        self.rendered_screen.capture(buffer);
        let Some(selection) = self.text_selection.filter(|selection| selection.dragged) else {
            return;
        };
        for (x, y) in self.rendered_screen.positions(selection) {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
            }
        }
    }

    pub(crate) fn take_pending_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    pub(crate) fn finish_copy(&mut self, result: Result<()>) {
        match result {
            Ok(()) => {
                if self.copy_notice_deadline.is_none() || self.status != COPY_NOTICE {
                    self.status_before_copy = Some(std::mem::take(&mut self.status));
                }
                self.status = COPY_NOTICE.to_string();
                self.copy_notice_deadline = Some(Instant::now() + COPY_NOTICE_WINDOW);
            }
            Err(err) => {
                self.status = format!("Could not copy selected text: {err}");
                self.copy_notice_deadline = None;
                self.status_before_copy = None;
            }
        }
    }

    pub fn is_hovered(&self, action: HitAction) -> bool {
        self.hovered == Some(action)
    }

    fn update_hover(&mut self, x: u16, y: u16) {
        self.hovered = self.hit_at(x, y);
    }

    fn mouse_down(&mut self, x: u16, y: u16) -> Result<()> {
        if let Some(HitAction::FocusCard { column, card }) = self.hit_at(x, y)
            && let Some(task_id) = self
                .visible_tasks_for_column(column)
                .get(card)
                .map(|task| task.id.clone())
        {
            self.screen = Screen::Board;
            self.focused_column = column;
            self.focused_card = card;
            self.ensure_focused_visible();
            self.dragging = Some(DragState {
                task_id,
                from_column: column,
                card,
                target_column: None,
                moved: false,
            });
            return Ok(());
        }
        self.click_at(x, y)
    }

    fn update_drag_target(&mut self, x: u16, y: u16) {
        let target = self.column_at(x, y);
        if let Some(dragging) = self.dragging.as_mut() {
            dragging.target_column = target;
            if target.is_some_and(|target| target != dragging.from_column) {
                dragging.moved = true;
            }
        }
    }

    fn finish_drag(&mut self, x: u16, y: u16) -> Result<()> {
        let Some(dragging) = self.dragging.take() else {
            return Ok(());
        };
        let target = self.column_at(x, y);
        if dragging.moved
            && let Some(target) = target.filter(|target| *target != dragging.from_column)
        {
            let target_status = self
                .board
                .columns
                .get(target)
                .map(|column| column.id.clone());
            if let Some(target_status) = target_status {
                self.ops
                    .move_task(&dragging.task_id, &target_status, false)?;
                self.refresh_after_action()?;
                self.focused_column = target;
                self.focused_card = 0;
                self.clamp_focus();
                self.status = format!("Moved {} to {}", dragging.task_id, target_status);
            }
        } else if !dragging.moved {
            self.open_focused_detail()?;
        }
        Ok(())
    }

    /// Contextual status-bar text shown while a card is being dragged, naming
    /// the task and (once the cursor is over a different column) where a
    /// release would drop it. `None` when no drag is in flight.
    pub fn drag_hint(&self) -> Option<String> {
        let dragging = self.dragging.as_ref()?;
        match self
            .drop_target_column()
            .and_then(|target| self.board.columns.get(target))
        {
            Some(column) => Some(format!(
                "Moving {} → {} · release to move",
                dragging.task_id, column.name
            )),
            None => Some(format!(
                "Moving {} · drag onto another column to move it",
                dragging.task_id
            )),
        }
    }

    /// Column a release would drop the dragged card into — only when it differs
    /// from the source column, so the drop-target highlight never fires while
    /// hovering the card's own column.
    pub fn drop_target_column(&self) -> Option<usize> {
        let dragging = self.dragging.as_ref()?;
        dragging
            .target_column
            .filter(|target| *target != dragging.from_column)
    }

    /// True for the specific card currently held in a drag, so the renderer can
    /// mark the source as lifted.
    pub fn is_dragging_card(&self, column: usize, card: usize) -> bool {
        self.dragging
            .as_ref()
            .is_some_and(|dragging| dragging.from_column == column && dragging.card == card)
    }

    fn column_at(&self, x: u16, y: u16) -> Option<usize> {
        match self.hit_at(x, y) {
            Some(HitAction::FocusCard { column, .. })
            | Some(HitAction::OpenAnswer { column, .. })
            | Some(HitAction::ColumnFocus(column)) => Some(column),
            Some(
                HitAction::Action(_)
                | HitAction::ModalField(_)
                | HitAction::ModalOption { .. }
                | HitAction::ModalButton(_)
                | HitAction::DetailAnswerOption { .. }
                | HitAction::DetailEdits
                | HitAction::DetailThread,
            )
            | None => None,
        }
    }

    fn hit_at(&self, x: u16, y: u16) -> Option<HitAction> {
        self.hitboxes
            .iter()
            .find(|hitbox| contains(hitbox.area, x, y))
            .map(|hitbox| hitbox.action)
    }

    fn click_at(&mut self, x: u16, y: u16) -> Result<()> {
        match self.hit_at(x, y) {
            Some(HitAction::FocusCard { column, card }) => self.click_card(column, card),
            Some(HitAction::OpenAnswer { column, card }) => {
                self.screen = Screen::Board;
                self.focused_column = column;
                self.focused_card = card;
                self.ensure_focused_visible();
                self.open_focused_detail()?;
                self.set_detail_focus(DetailFocus::Answer);
                Ok(())
            }
            Some(HitAction::Action(action)) => self.dispatch(action),
            Some(HitAction::DetailAnswerOption { index }) => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.variant_selected = index;
                }
                self.set_detail_focus(DetailFocus::Answer);
                Ok(())
            }
            // Column areas only steer wheel targeting for now; clicking the
            // empty part of a column focuses it without opening any action.
            Some(HitAction::ColumnFocus(column)) => {
                self.focused_column = column;
                self.focused_card = 0;
                self.clamp_focus();
                Ok(())
            }
            Some(HitAction::DetailThread) => {
                self.set_detail_focus(DetailFocus::Thread);
                Ok(())
            }
            Some(HitAction::DetailEdits) => {
                self.set_detail_focus(DetailFocus::Edits);
                Ok(())
            }
            Some(
                HitAction::ModalField(_)
                | HitAction::ModalOption { .. }
                | HitAction::ModalButton(_),
            ) => Ok(()),
            None => Ok(()),
        }
    }

    fn handle_modal_click(&mut self, x: u16, y: u16) -> Result<()> {
        match self.hit_at(x, y) {
            Some(HitAction::ModalField(field)) => {
                if let Some(modal) = self.modal.as_mut() {
                    modal.focus_field(field);
                }
            }
            Some(HitAction::ModalOption { field, index }) => {
                let mut changed = false;
                if let Some(modal) = self.modal.as_mut() {
                    let previous = selector_index(modal, field);
                    modal.focus_field(field);
                    modal.select_option(field, index);
                    changed = selector_index(modal, field) != previous;
                }
                if changed
                    && matches!(field, DialogField::Backend | DialogField::Model)
                    && let Some(mut modal) = self.modal.take()
                {
                    if field == DialogField::Backend {
                        self.refresh_backend_options(&mut modal);
                    } else {
                        self.refresh_effort_options(&mut modal);
                    }
                    self.modal = Some(modal);
                }
            }
            Some(HitAction::ModalButton(button)) => self.activate_modal_button(button)?,
            _ => {}
        }
        Ok(())
    }

    fn click_card(&mut self, column: usize, card: usize) -> Result<()> {
        self.screen = Screen::Board;
        self.focused_column = column;
        self.focused_card = card;
        self.ensure_focused_visible();
        self.open_focused_detail()
    }

    fn scroll_at(&mut self, x: u16, y: u16, delta: isize) {
        if self.screen != Screen::Board {
            if self.screen == Screen::Detail && self.hit_at(x, y) == Some(HitAction::DetailThread) {
                self.scroll_detail(delta);
                return;
            }
            if delta < 0 {
                self.page_up();
            } else {
                self.page_down();
            }
            return;
        }
        let column = match self.hit_at(x, y) {
            Some(
                HitAction::FocusCard { column, .. }
                | HitAction::OpenAnswer { column, .. }
                | HitAction::ColumnFocus(column),
            ) => column,
            Some(
                HitAction::Action(_)
                | HitAction::ModalField(_)
                | HitAction::ModalOption { .. }
                | HitAction::ModalButton(_)
                | HitAction::DetailAnswerOption { .. }
                | HitAction::DetailEdits
                | HitAction::DetailThread,
            )
            | None => self.focused_column,
        };
        self.scroll_column(column, delta);
    }

    fn scroll_detail(&mut self, delta: isize) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        detail.scroll = if delta < 0 {
            detail.scroll.saturating_sub(delta.unsigned_abs() as u16)
        } else {
            detail
                .scroll
                .saturating_add(delta as u16)
                .min(detail.max_scroll)
        };
    }

    /// Scroll a column's viewport by `delta` cards. When the focused column
    /// scrolls, focus is dragged along so the render-time clamp
    /// (`ensure_focused_visible`) doesn't immediately undo the scroll.
    fn scroll_column(&mut self, column: usize, delta: isize) {
        let len = self.visible_tasks_for_column(column).len();
        let capacity = self
            .visible_card_capacities
            .get(column)
            .copied()
            .unwrap_or(1)
            .max(1);
        let Some(offset) = self.column_offsets.get_mut(column) else {
            return;
        };
        *offset = if delta < 0 {
            offset.saturating_sub(delta.unsigned_abs())
        } else {
            offset
                .saturating_add(delta.unsigned_abs())
                .min(len.saturating_sub(capacity))
        };
        if column == self.focused_column && len > 0 {
            let top = *offset;
            let bottom = offset.saturating_add(capacity - 1);
            self.focused_card = self.focused_card.clamp(top, bottom).min(len - 1);
        }
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
            self.refresh_archived_tasks()?;
            if self.screen == Screen::Sessions {
                self.refresh_active_sessions()?;
            }
            if let Some(detail) = self.detail.as_ref() {
                let task_id = detail.task_id.clone();
                let focus = detail.focus;
                let scroll = detail.scroll;
                self.load_detail(&task_id)?;
                if let Some(detail) = self.detail.as_mut() {
                    detail.scroll = scroll.min(detail.max_scroll);
                    if detail.focus_available(focus) {
                        detail.focus = focus;
                    }
                }
            }
            self.clamp_focus();
            self.status = "Board updated from disk".to_string();
        }
        Ok(())
    }

    pub fn tick(&mut self) -> Result<()> {
        self.reload_if_changed()?;
        self.refresh_modal_after_catalog_warm();
        let now = Instant::now();
        self.expire_ctrl_c_prompt_at(now);
        self.expire_copy_notice_at(now);
        self.expire_session_states_at(timefmt::now());
        self.resume_expired_waits_throttled();
        // Log writes bypass the fs watcher (it only covers board dirs), so
        // the pager tail refreshes on the tick.
        if self.screen == Screen::LogView {
            self.refresh_log_view();
        }
        self.refresh_session_progress();
        Ok(())
    }

    /// Refresh live agent telemetry for tasks with a running agent. Transcripts
    /// live under `.kanban/logs/` (unwatched, like the pager), so this reads on
    /// the tick. Only Live/Waiting tasks are read — a handful at most — and the
    /// map is rebuilt each pass so a finished agent's line disappears with it.
    fn refresh_session_progress(&mut self) {
        let mut progress = HashMap::new();
        for column in &self.board.columns {
            for task in &column.tasks {
                let Some(session_id) = task.session.as_deref() else {
                    continue;
                };
                if !matches!(
                    self.board.session_states.get(&task.id),
                    Some(SessionState::Live | SessionState::Waiting)
                ) {
                    continue;
                }
                let backend = task.agent_backend.as_deref().unwrap_or("claude");
                let found =
                    telemetry::read_session_progress(&self.project_path, session_id, backend);
                if found.has_data() {
                    progress.insert(task.id.clone(), found);
                }
            }
        }
        self.session_progress = progress;
    }

    fn refresh_modal_after_catalog_warm(&mut self) {
        let Ok(config) = self.ops.config.load() else {
            return;
        };
        let mut newly_ready = false;
        for (backend, command) in catalog_backend_commands(&config) {
            if self.catalog_ready.contains(&backend) {
                continue;
            }
            if cached_backend_catalog(&backend, &command).is_some() {
                self.catalog_ready.insert(backend);
                newly_ready = true;
            }
        }
        if !newly_ready {
            return;
        }
        let should_refresh = self.modal.as_ref().is_some_and(|modal| {
            matches!(
                modal.modal,
                Modal::NewTask { .. } | Modal::EditTask { .. } | Modal::Settings
            )
        });
        if should_refresh && let Some(mut modal) = self.modal.take() {
            self.refresh_backend_options_with_config(&mut modal, &config);
            self.modal = Some(modal);
        }
    }

    pub(crate) fn expire_ctrl_c_prompt_at(&mut self, now: Instant) {
        let Some(deadline) = self.ctrl_c_exit_deadline else {
            return;
        };
        if now <= deadline {
            return;
        }
        self.ctrl_c_exit_deadline = None;
        if self.status == CTRL_C_EXIT_PROMPT {
            self.status.clear();
        }
    }

    pub(crate) fn expire_copy_notice_at(&mut self, now: Instant) {
        let Some(deadline) = self.copy_notice_deadline else {
            return;
        };
        if now <= deadline {
            return;
        }
        self.copy_notice_deadline = None;
        let previous = self.status_before_copy.take().unwrap_or_default();
        if self.status == COPY_NOTICE {
            self.status = previous;
        }
    }

    pub(crate) fn expire_session_states_at(&mut self, now: chrono::NaiveDateTime) {
        let expired = self
            .board
            .session_deadlines
            .iter()
            .filter(|(_, deadline)| now > **deadline)
            .map(|(task_id, _)| task_id.clone())
            .collect::<Vec<_>>();
        for task_id in expired {
            if matches!(
                self.board.session_states.get(&task_id),
                Some(SessionState::Live)
            ) {
                self.board
                    .session_states
                    .insert(task_id.clone(), SessionState::Crashed);
                if let Some(extra) = self.board.extras.get_mut(&task_id) {
                    extra.waiting = false;
                }
            }
            self.board.session_deadlines.remove(&task_id);
        }
    }

    /// Relaunch agents whose declared wait deadline expired ("ping" them to
    /// report status). Throttled: the scan reads every session file, so it
    /// runs at most once per interval, not on every tick. Errors land in the
    /// status line instead of killing the TUI.
    fn resume_expired_waits_throttled(&mut self) {
        const WAIT_RESUME_INTERVAL: Duration = Duration::from_secs(10);
        if self
            .last_wait_resume
            .is_some_and(|last| last.elapsed() < WAIT_RESUME_INTERVAL)
        {
            return;
        }
        self.last_wait_resume = Some(Instant::now());
        match self.ops.resume_expired_waits() {
            Ok(resumed) if !resumed.is_empty() => {
                let tasks = resumed
                    .iter()
                    .map(|(task_id, _)| task_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.status = format!("Wait deadline passed — relaunched: {tasks}");
            }
            Ok(_) => {}
            Err(err) => self.status = format!("Wait resume failed: {err}"),
        }
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
                self.scroll_detail(-1);
            }
            Screen::Sessions => self.session_selected = self.session_selected.saturating_sub(1),
            Screen::Archive => self.archive_selected = self.archive_selected.saturating_sub(1),
            Screen::LogView => self.scroll_log(-1),
            Screen::TextView => self.scroll_text_view(-1),
            Screen::Help => self.help_scroll = self.help_scroll.saturating_sub(1),
        }
    }

    fn focus_down(&mut self) {
        match self.screen {
            Screen::Board => self.focus_next_card(),
            Screen::Detail => {
                self.scroll_detail(1);
            }
            Screen::Sessions => {
                self.session_selected =
                    next_index(self.session_selected, self.filtered_active_sessions().len());
            }
            Screen::Archive => {
                self.archive_selected =
                    next_index(self.archive_selected, self.filtered_archived_tasks().len());
            }
            Screen::LogView => self.scroll_log(1),
            Screen::TextView => self.scroll_text_view(1),
            Screen::Help => {
                self.help_scroll = self.help_scroll.saturating_add(1).min(self.help_max_scroll);
            }
        }
    }

    fn page_up(&mut self) {
        match self.screen {
            Screen::Board => {
                self.focused_card = self.focused_card.saturating_sub(5);
                self.ensure_focused_visible();
            }
            Screen::Detail => {
                self.scroll_detail(-5);
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
                self.scroll_detail(5);
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
            Screen::LogView => {
                if let Some(log) = self.log_view.as_mut() {
                    log.scroll = 0;
                    log.follow = false;
                }
            }
            Screen::TextView => {
                if let Some(view) = self.text_view.as_mut() {
                    view.scroll = 0;
                }
            }
            Screen::Help => self.help_scroll = 0,
        }
    }

    fn end(&mut self) {
        match self.screen {
            Screen::Board => self.focus_last_card(),
            Screen::Detail => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.scroll = detail.max_scroll;
                }
            }
            Screen::Sessions => {
                self.session_selected = self.filtered_active_sessions().len().saturating_sub(1);
            }
            Screen::Archive => {
                self.archive_selected = self.filtered_archived_tasks().len().saturating_sub(1);
            }
            Screen::LogView => {
                if let Some(log) = self.log_view.as_mut() {
                    log.scroll = log.max_scroll;
                    log.follow = true;
                }
            }
            Screen::TextView => {
                if let Some(view) = self.text_view.as_mut() {
                    view.scroll = view.max_scroll;
                }
            }
            Screen::Help => self.help_scroll = self.help_max_scroll,
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
        self.visible_card_capacities
            .resize(self.board.columns.len(), 1);
        self.clamp_all_column_offsets();
        self.ensure_focused_visible();
    }

    fn clamp_all_column_offsets(&mut self) {
        let limits = (0..self.board.columns.len())
            .map(|column| {
                let count = self.visible_tasks_for_column(column).len();
                let capacity = self
                    .visible_card_capacities
                    .get(column)
                    .copied()
                    .unwrap_or(1)
                    .max(1);
                count.saturating_sub(capacity)
            })
            .collect::<Vec<_>>();
        for (offset, limit) in self.column_offsets.iter_mut().zip(limits) {
            *offset = (*offset).min(limit);
        }
    }

    pub fn visible_tasks_for_column(&self, column_index: usize) -> Vec<&Task> {
        let Some(column) = self.board.columns.get(column_index) else {
            return Vec::new();
        };
        let query = self.search.text();
        column
            .tasks
            .iter()
            .filter(|task| {
                query.is_empty()
                    || case_insensitive_match(&task.id, &query)
                    || case_insensitive_match(&task.title, &query)
                    || case_insensitive_match(&task.description, &query)
            })
            .take(self.settings.max_tasks_per_column)
            .collect()
    }

    pub fn filtered_active_sessions(&self) -> Vec<&ActiveSession> {
        let query = self.search.text();
        self.active_sessions
            .iter()
            .filter(|active_session| {
                query.is_empty()
                    || case_insensitive_match(&active_session.session.id, &query)
                    || case_insensitive_match(&active_session.session.task_id, &query)
                    || case_insensitive_match(&active_session.task_label, &query)
            })
            .collect()
    }

    pub fn filtered_archived_tasks(&self) -> Vec<&Task> {
        let query = self.search.text();
        self.archived_tasks
            .iter()
            .filter(|task| {
                query.is_empty()
                    || case_insensitive_match(&task.id, &query)
                    || case_insensitive_match(&task.title, &query)
                    || case_insensitive_match(&task.description, &query)
            })
            .collect()
    }

    pub fn matching_task_count(&self, column_index: usize) -> usize {
        let Some(column) = self.board.columns.get(column_index) else {
            return 0;
        };
        let query = self.search.text();
        column
            .tasks
            .iter()
            .filter(|task| {
                query.is_empty()
                    || case_insensitive_match(&task.id, &query)
                    || case_insensitive_match(&task.title, &query)
                    || case_insensitive_match(&task.description, &query)
            })
            .count()
    }

    pub fn focused_task(&self) -> Option<&Task> {
        self.visible_tasks_for_column(self.focused_column)
            .get(self.focused_card)
            .copied()
    }

    fn focused_task_id(&self) -> Option<String> {
        self.focused_task().map(|task| task.id.clone())
    }

    /// The task an action applies to: the open detail's task, else the
    /// focused board card. Keeps every hotkey/button meaningful from both
    /// screens.
    pub(super) fn current_task(&self) -> Option<Task> {
        if self.screen == Screen::Detail {
            return self.detail.as_ref().and_then(|detail| detail.task.clone());
        }
        self.focused_task().cloned()
    }

    fn current_task_id(&self) -> Option<String> {
        if self.screen == Screen::Detail {
            return self.detail.as_ref().map(|detail| detail.task_id.clone());
        }
        self.focused_task_id()
    }

    /// Reload the board and, when the detail screen is open, its task —
    /// preserving scroll position and panel focus where still valid.
    fn refresh_after_action(&mut self) -> Result<()> {
        self.board = BoardSnapshot::load(&self.ops)?;
        self.refresh_archived_tasks()?;
        self.clamp_focus();
        if let Some(detail) = self.detail.as_ref() {
            let task_id = detail.task_id.clone();
            let focus = detail.focus;
            let scroll = detail.scroll;
            self.load_detail(&task_id)?;
            if let Some(detail) = self.detail.as_mut() {
                detail.scroll = scroll.min(detail.max_scroll);
                if detail.focus_available(focus) {
                    detail.focus = focus;
                }
            }
        }
        Ok(())
    }

    fn ensure_focused_visible(&mut self) {
        let capacity = self
            .visible_card_capacities
            .get(self.focused_column)
            .copied()
            .unwrap_or(1)
            .max(1);
        let task_count = self.visible_tasks_for_column(self.focused_column).len();
        let Some(offset) = self.column_offsets.get_mut(self.focused_column) else {
            return;
        };
        *offset = (*offset).min(task_count.saturating_sub(capacity));
        if self.focused_card < *offset {
            *offset = self.focused_card;
        }
        if self.focused_card >= offset.saturating_add(capacity) {
            *offset = self.focused_card + 1 - capacity;
        }
    }

    pub fn set_visible_card_capacity(&mut self, column_index: usize, capacity: usize) {
        if self.visible_card_capacities.len() <= column_index {
            self.visible_card_capacities.resize(column_index + 1, 1);
        }
        self.visible_card_capacities[column_index] = capacity.max(1);
        let task_count = self.visible_tasks_for_column(column_index).len();
        if let Some(offset) = self.column_offsets.get_mut(column_index) {
            *offset = (*offset).min(task_count.saturating_sub(capacity.max(1)));
        }
        if column_index == self.focused_column {
            self.ensure_focused_visible();
        }
    }

    fn open_focused_detail(&mut self) -> Result<()> {
        if self.screen == Screen::Sessions {
            let selected = self
                .filtered_active_sessions()
                .get(self.session_selected)
                .map(|active| (active.session.id.clone(), active.session.task_id.clone()));
            if let Some((session_id, task_id)) = selected {
                let backend = self
                    .ops
                    .get_task(&task_id)
                    .ok()
                    .flatten()
                    .and_then(|task| task.agent_backend)
                    .unwrap_or_else(|| "claude".to_string());
                self.open_session(&session_id, &backend)?;
            } else {
                self.status = "No active session selected".to_string();
            }
            return Ok(());
        }
        if self.screen == Screen::Archive {
            if let Some(task_id) = self
                .filtered_archived_tasks()
                .get(self.archive_selected)
                .map(|task| task.id.clone())
            {
                self.load_detail(&task_id)?;
                self.return_screen = Screen::Archive;
                self.screen = Screen::Detail;
            }
            return Ok(());
        }
        if let Some(task_id) = self.focused_task_id() {
            self.load_detail(&task_id)?;
            self.return_screen = Screen::Board;
            self.screen = Screen::Detail;
        }
        Ok(())
    }

    /// Leave the detail screen for wherever it was opened from, refreshing
    /// that list so the closed detail's changes are visible immediately.
    fn close_detail(&mut self) -> Result<()> {
        self.detail = None;
        let target = self.return_screen;
        self.return_screen = Screen::Board;
        self.screen = match target {
            Screen::Sessions => {
                self.refresh_active_sessions()?;
                Screen::Sessions
            }
            Screen::Archive => {
                self.refresh_archived_tasks()?;
                Screen::Archive
            }
            _ => Screen::Board,
        };
        Ok(())
    }

    fn load_detail(&mut self, task_id: &str) -> Result<()> {
        let preserved_review_edits = self.detail.as_ref().and_then(|detail| {
            if detail.task_id != task_id {
                return None;
            }
            let persisted = detail
                .task
                .as_ref()
                .map(|task| task.review_edits.as_str())
                .unwrap_or_default();
            (textarea_text(&detail.review_edits) != persisted).then(|| detail.review_edits.clone())
        });
        let preserved_answer = self.detail.as_ref().and_then(|detail| {
            if detail.task_id != task_id {
                return None;
            }
            let question_id = detail
                .open_questions()
                .get(detail.question_index)?
                .id
                .clone();
            Some((
                question_id,
                detail.answer_input.clone(),
                detail.variant_selected,
            ))
        });
        let preserved_selected_msg_id = self.detail.as_ref().and_then(|detail| {
            (detail.task_id == task_id)
                .then(|| detail.messages.get(detail.thread_selected))
                .flatten()
                .map(|message| message.id.clone())
        });
        let task = self.ops.get_task(task_id)?;
        let messages = ThreadManager::new(&self.project_path)?
            .load(task_id)?
            .messages;
        let thread_selected = preserved_selected_msg_id
            .and_then(|msg_id| messages.iter().position(|message| message.id == msg_id))
            .unwrap_or_else(|| messages.len().saturating_sub(1));
        let mut review_edits = TextArea::from(
            task.as_ref()
                .map(|task| lines_or_empty(&task.review_edits))
                .unwrap_or_else(|| vec![String::new()]),
        );
        if let Some(editor) = preserved_review_edits {
            review_edits = editor;
        }
        review_edits.set_cursor_line_style(ratatui::style::Style::default());
        let has_prompt = task
            .as_ref()
            .is_some_and(|task| self.ops.task_has_prompt(task));
        let provenance =
            provenance::collect_for_thread(&self.ops.storage.provenance_dir, &messages);
        let has_provenance = !provenance.is_empty();
        let mut detail = DetailState {
            task_id: task_id.to_string(),
            task,
            messages,
            thread_selected,
            scroll: 0,
            // The real bound is known only at render time; start unbounded so
            // a preserved scroll position survives until the next frame.
            max_scroll: u16::MAX,
            review_edits,
            focus: DetailFocus::Thread,
            answer_input: {
                let mut input = TextArea::default();
                input.set_cursor_line_style(ratatui::style::Style::default());
                input
            },
            question_index: 0,
            variant_selected: 0,
            has_prompt,
            has_provenance,
            provenance,
        };
        if let Some((question_id, answer_input, variant_selected)) = preserved_answer
            && let Some(question_index) = detail
                .open_questions()
                .iter()
                .position(|question| question.id == question_id)
        {
            let variant_count = detail.open_questions()[question_index].variants.len();
            detail.answer_input = answer_input;
            detail.question_index = question_index;
            detail.variant_selected = variant_selected.min(variant_count);
        }
        self.detail = Some(detail);
        Ok(())
    }

    fn move_thread_selection(&mut self, delta: i32) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        if detail.messages.is_empty() {
            return;
        }
        let len = detail.messages.len() as i32;
        let next = (detail.thread_selected as i32 + delta).rem_euclid(len);
        detail.thread_selected = next as usize;
    }

    /// Toggle `rejected` on the message selected in the thread panel (`[`/`]`
    /// to move the selection), quarantining or restoring it from the supply
    /// chain fed into future agent prompts.
    fn toggle_reject_selected_message(&mut self) -> Result<()> {
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        let Some(message) = detail.messages.get(detail.thread_selected) else {
            return Ok(());
        };
        let task_id = detail.task_id.clone();
        let msg_id = message.id.clone();
        let already_rejected = message.status == MessageStatus::Rejected;
        let result = if already_rejected {
            self.ops.unreject_message(&task_id, &msg_id)?
        } else {
            self.ops.reject_message(&task_id, &msg_id)?
        };
        self.status = match result {
            Some(_) if already_rejected => format!("{msg_id} restored"),
            Some(_) => format!("{msg_id} rejected"),
            None => format!("Message {msg_id} not found"),
        };
        self.load_detail(&task_id)?;
        Ok(())
    }

    fn open_new_dialog(&mut self) {
        let target_status = if self.screen == Screen::Detail {
            self.current_task()
                .map(|task| task.status.as_str().to_string())
        } else {
            self.board
                .columns
                .get(self.focused_column)
                .map(|column| column.id.clone())
        };
        self.open_new_dialog_with_target(target_status);
    }

    fn open_new_dialog_with_target(&mut self, target_status: Option<String>) {
        let mut modal = ModalState::new(Modal::NewTask { target_status });
        self.populate_task_form_options(&mut modal, None);
        modal.capture_initial_values();
        self.modal = Some(modal);
    }

    fn open_bulk_confirm(&mut self, action: BulkAction) {
        let status = match action {
            BulkAction::ArchiveAllDone => TaskStatus::Done,
            BulkAction::MarkReviewDone => TaskStatus::Review,
        };
        let task_ids = self
            .board
            .columns
            .iter()
            .find(|column| column.id == status.as_str())
            .map(|column| {
                column
                    .tasks
                    .iter()
                    .map(|task| task.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if task_ids.is_empty() {
            self.status = match action {
                BulkAction::ArchiveAllDone => "No Done tasks to archive".to_string(),
                BulkAction::MarkReviewDone => "No Review tasks to mark done".to_string(),
            };
            return;
        }
        self.modal = Some(ModalState::new(Modal::BulkConfirm { action, task_ids }));
    }

    fn focus_first_question(&mut self) {
        let first = (0..self.board.columns.len()).find_map(|column_index| {
            self.visible_tasks_for_column(column_index)
                .iter()
                .position(|task| task.has_questions)
                .map(|card| (column_index, card))
        });
        if let Some((column_index, card)) = first {
            let task_id = self.visible_tasks_for_column(column_index)[card].id.clone();
            self.screen = Screen::Board;
            self.focused_column = column_index;
            self.focused_card = card;
            self.ensure_focused_visible();
            self.status = format!("Focused {task_id}");
            return;
        }
        self.status = "No tasks have open questions".to_string();
    }

    fn open_edit_dialog(&mut self) {
        let Some(task) = self.current_task() else {
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
        modal.capture_initial_values();
        self.modal = Some(modal);
    }

    fn open_settings_dialog(&mut self) {
        let config = match self.ops.config.load_fresh() {
            Ok(config) => config,
            Err(err) => {
                self.status = format!("Could not load project settings: {err}");
                return;
            }
        };
        let mut modal = ModalState::new(Modal::Settings);
        modal.title = TextArea::new(vec![sanitize_terminal_text(&tui_string(
            &config.tui,
            "name",
            "Kanban",
        ))]);
        modal.backend = TextArea::new(vec![
            mapping_str(Some(&config.auto_launch), "default_agent")
                .unwrap_or_else(|| "opencode".to_string()),
        ]);
        modal.theme = TextArea::new(vec![
            Theme::normalize_name(&tui_string(&config.tui, "theme", "dark")).to_string(),
        ]);
        modal.task_sort = TextArea::new(vec![
            normalize_task_sort(&tui_string(&config.tui, "task_sort", TASK_SORT_NUMBER))
                .to_string(),
        ]);
        self.populate_settings_form_options(&mut modal);
        modal.capture_initial_values();
        self.modal = Some(modal);
    }

    fn open_move_dialog(&mut self) {
        if let Some(task) = self.current_task() {
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
            modal.capture_initial_values();
            self.modal = Some(modal);
        }
    }

    fn open_delete_dialog(&mut self) {
        if let Some(task_id) = self.current_task_id() {
            self.modal = Some(ModalState::new(Modal::DeleteConfirm { task_id }));
        }
    }

    fn open_answer_dialog(&mut self) -> Result<()> {
        let Some(task_id) = self.current_task_id() else {
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
        let mut modal = ModalState::new(Modal::AnswerQuestion {
            task_id,
            questions: choices,
        });
        modal.capture_initial_values();
        self.modal = Some(modal);
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
        self.refresh_backend_options_with_config(modal, &config);
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

    fn populate_settings_form_options(&self, modal: &mut ModalState) {
        let Ok(config) = self.ops.config.load() else {
            return;
        };
        let backend_options = config
            .agents
            .keys()
            .filter_map(Value::as_str)
            .map(|backend| SelectOption {
                label: backend.to_string(),
                value: Some(backend.to_string()),
            })
            .collect::<Vec<_>>();
        modal.set_backend_options(backend_options);
        modal.set_theme_options(vec![
            SelectOption {
                label: "Dark".to_string(),
                value: Some("dark".to_string()),
            },
            SelectOption {
                label: "Light".to_string(),
                value: Some("light".to_string()),
            },
        ]);
        modal.set_task_sort_options(vec![
            SelectOption {
                label: "Task number".to_string(),
                value: Some(TASK_SORT_NUMBER.to_string()),
            },
            SelectOption {
                label: "Updated (oldest first)".to_string(),
                value: Some(TASK_SORT_UPDATED_ASC.to_string()),
            },
            SelectOption {
                label: "Updated (newest first)".to_string(),
                value: Some(TASK_SORT_UPDATED_DESC.to_string()),
            },
        ]);
        self.refresh_backend_options_with_config(modal, &config);
    }

    fn recover_current_task(&mut self) -> Result<()> {
        // On an archived task `u` means restore (same key as in the Archive
        // list), which goes through its own confirmation.
        if self
            .current_task()
            .is_some_and(|task| task.status == TaskStatus::Archive)
        {
            self.open_restore_confirm();
            return Ok(());
        }
        let Some(task_id) = self.current_task_id() else {
            return Ok(());
        };
        let Some(_) = self.ops.recover_task(&task_id)? else {
            self.status = format!("Task {task_id} not found");
            return Ok(());
        };
        self.refresh_after_action()?;
        self.status = format!("Task {task_id} recovered to To Do");
        Ok(())
    }

    fn run_current_task(&mut self) -> Result<()> {
        let Some(task_id) = self.current_task_id() else {
            self.status = "No task selected".to_string();
            return Ok(());
        };
        let should_close_detail = self.screen == Screen::Detail;
        let started = match self.ops.start_task(&task_id) {
            Ok(Some(session_id)) => {
                self.status = format!("Started {task_id} → {session_id}");
                true
            }
            Ok(None) => {
                self.status = format!("Task {task_id} not found");
                false
            }
            Err(err) => {
                self.status = err.to_string();
                false
            }
        };
        self.refresh_after_action()?;
        if started && should_close_detail {
            self.close_detail()?;
        }
        Ok(())
    }

    fn revoke_current_task(&mut self) -> Result<()> {
        let Some(task) = self.current_task() else {
            self.status = "No task selected".to_string();
            return Ok(());
        };
        if task.status != TaskStatus::InProgress {
            self.status = format!("{} is not In Progress", task.id);
            return Ok(());
        }
        let task_id = task.id.clone();
        let expected_session = task.session.clone();
        let relaunched = self
            .ops
            .revoke_in_progress_task(&task_id, expected_session.as_deref())?
            .is_some();
        self.refresh_after_action()?;
        self.status = if relaunched {
            format!("Revoked and woke {task_id}")
        } else {
            format!("Revoke of {task_id} was not started")
        };
        Ok(())
    }

    fn approve_current_task(&mut self) -> Result<()> {
        let Some(task) = self.current_task() else {
            self.status = "No task selected".to_string();
            return Ok(());
        };
        if task.status != TaskStatus::Review {
            self.status = format!("{} is not in Review", task.id);
            return Ok(());
        }
        self.ops
            .move_task(&task.id, TaskStatus::Done.as_str(), false)?;
        self.refresh_after_action()?;
        self.status = format!("Approved {} → Done", task.id);
        Ok(())
    }

    fn attach_current_task(&mut self) -> Result<()> {
        let Some(task) = self.current_task() else {
            self.status = "No task selected".to_string();
            return Ok(());
        };
        let Some(session_id) = task.session.clone() else {
            self.status = format!("{} has no session", task.id);
            return Ok(());
        };
        let backend = task
            .agent_backend
            .clone()
            .unwrap_or_else(|| "claude".to_string());
        self.open_session(&session_id, &backend)
    }

    /// Decide how to "open" a session for the user and act on it. tmux-hosted
    /// live sessions are attached (interactive); a running background agent has
    /// no live terminal, so its log is followed instead; a stopped agent with a
    /// recorded backend session id is reopened with `<backend> --resume`.
    fn open_session(&mut self, session_id: &str, backend: &str) -> Result<()> {
        if crate::agent::session_exists(session_id) {
            self.pending_terminal = Some(TerminalAction::Attach(session_id.to_string()));
            self.status = format!("Attaching to {session_id}");
            return Ok(());
        }
        let heartbeat_timeout = self.ops.config.get_threshold("session_heartbeat_timeout")?;
        let state =
            SessionManager::new(&self.project_path).session_state(session_id, heartbeat_timeout);
        if matches!(state, Some(SessionState::Live | SessionState::Waiting)) {
            self.open_log_view_for(session_id.to_string());
            self.status =
                format!("Following {session_id} log (background agent, no terminal to attach)");
            return Ok(());
        }
        if let Some(action) = self.resume_action(session_id, backend)? {
            self.pending_terminal = Some(action);
            self.status = format!("Resuming conversation for {session_id}");
            return Ok(());
        }
        // Nothing live and no resumable conversation: fall back to the log.
        self.open_log_view_for(session_id.to_string());
        Ok(())
    }

    /// Build the `<backend> --resume <backend_session_id>` action for a stopped
    /// session, or `None` when the backend has no known resume flag or the
    /// backend session id was never captured. Only claude is supported today.
    fn resume_action(&self, session_id: &str, backend: &str) -> Result<Option<TerminalAction>> {
        if backend != "claude" {
            return Ok(None);
        }
        let Some(backend_session_id) =
            provenance::load_manifest(&self.ops.storage.provenance_dir, session_id)
                .and_then(|manifest| manifest.backend_session_id)
        else {
            return Ok(None);
        };
        let config = self.ops.config.load()?;
        let command = crate::agent::backend_config(&config, backend)?.command;
        Ok(Some(TerminalAction::Foreground {
            command,
            args: vec!["--resume".to_string(), backend_session_id],
            cwd: self.project_path.clone(),
            label: format!("resume {session_id}"),
        }))
    }

    fn rerun_current_task(&mut self) -> Result<()> {
        let Some(task) = self.current_task() else {
            self.status = "No task selected".to_string();
            return Ok(());
        };
        let relaunched = match task.status {
            TaskStatus::Review => {
                self.save_visible_review_edits_before_rerun(&task)?;
                self.ops.rerun_review_task(&task.id, None)?.is_some()
            }
            TaskStatus::InProgress => self.ops.rerun_in_progress_task(&task.id, None)?.is_some(),
            _ => {
                self.status = format!("{} can be re-run only from Review or In Progress", task.id);
                return Ok(());
            }
        };
        self.refresh_after_action()?;
        self.status = if relaunched {
            format!("Re-ran {}", task.id)
        } else {
            format!("Re-run of {} was not started", task.id)
        };
        Ok(())
    }

    fn save_visible_review_edits_before_rerun(&self, task: &Task) -> Result<()> {
        if self.screen != Screen::Detail {
            return Ok(());
        }
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        if detail.task_id != task.id || !detail.edits_editable() {
            return Ok(());
        }
        let text = detail.review_edits.lines().join("\n");
        if text != task.review_edits {
            self.ops.set_review_edits(&task.id, &text)?;
        }
        Ok(())
    }

    fn open_add_message_dialog(&mut self) {
        if let Some(task_id) = self.current_task_id() {
            let mut modal = ModalState::new(Modal::AddMessage { task_id });
            modal.capture_initial_values();
            self.modal = Some(modal);
        } else {
            self.status = "No task selected".to_string();
        }
    }

    fn open_revert_dialog(&mut self) {
        let Some(task_id) = self.current_task_id() else {
            self.status = "No task selected".to_string();
            return;
        };
        if !self.ops.task_has_backups(&task_id) {
            self.status = format!("{task_id} has no backups to revert from");
            return;
        }
        self.modal = Some(ModalState::new(Modal::RevertConfirm { task_id }));
    }

    fn open_archive(&mut self) -> Result<()> {
        self.refresh_archived_tasks()?;
        self.archive_selected = 0;
        self.screen = Screen::Archive;
        self.status = format!("Archive: {} tasks", self.filtered_archived_tasks().len());
        Ok(())
    }

    fn open_sessions(&mut self) -> Result<()> {
        self.refresh_active_sessions()?;
        self.session_selected = 0;
        self.screen = Screen::Sessions;
        Ok(())
    }

    fn selected_session_id(&self) -> Option<String> {
        self.filtered_active_sessions()
            .get(self.session_selected)
            .map(|active_session| active_session.session.id.clone())
    }

    fn open_kill_confirm(&mut self) {
        match self.selected_session_id() {
            Some(session_id) => {
                self.modal = Some(ModalState::new(Modal::KillSessionConfirm { session_id }));
            }
            None => self.status = "No session selected".to_string(),
        }
    }

    fn open_log_view(&mut self) {
        let Some(session_id) = self.selected_session_id() else {
            self.status = "No session selected".to_string();
            return;
        };
        self.open_log_view_for(session_id);
    }

    /// Open the follow-mode log pager for an explicit session id (used by the
    /// unified open action for background agents that have no tmux host).
    fn open_log_view_for(&mut self, session_id: String) {
        self.log_view = Some(LogViewState {
            lines: load_log_tail(&self.project_path, &session_id),
            session_id: session_id.clone(),
            scroll: 0,
            // The real bound is known only at render time; follow mode pins
            // the first frame to the bottom regardless.
            max_scroll: u16::MAX,
            follow: true,
        });
        self.screen = Screen::LogView;
        self.status = format!("Log of {session_id}");
    }

    fn refresh_log_view(&mut self) {
        if let Some(log) = self.log_view.as_mut() {
            log.lines = load_log_tail(&self.project_path, &log.session_id);
        }
    }

    /// Open the read-only text pager over the task's most recent assembled
    /// prompt dump. No-op with a status hint when the task has never launched.
    fn open_prompt_view(&mut self) -> Result<()> {
        let Some(task) = self.current_task() else {
            self.status = "No task selected".to_string();
            return Ok(());
        };
        let Some(prompt) = self.ops.task_prompt(&task) else {
            self.status = format!("No prompt recorded for {}", task.id);
            return Ok(());
        };
        self.open_text_view(format!("Prompt · {}", task.id), &prompt, Screen::Detail);
        Ok(())
    }

    /// Open the read-only text pager over this task's input-provenance — the
    /// files each agent run actually read and wrote, plus URLs and MCP calls.
    /// This is telemetry kept out of the thread; the popup is its only home.
    fn open_context_view(&mut self) -> Result<()> {
        let Some(task) = self.current_task() else {
            self.status = "No task selected".to_string();
            return Ok(());
        };
        let body = self
            .detail
            .as_ref()
            .map(|detail| provenance::render_manifests(&detail.provenance))
            .unwrap_or_default();
        if body.trim().is_empty() {
            self.status = format!("No inputs recorded for {}", task.id);
            return Ok(());
        }
        self.open_text_view(
            format!("Inputs (provenance) · {}", task.id),
            &body,
            Screen::Detail,
        );
        Ok(())
    }

    /// One-screen summary for the selected session (Sessions view `i`): elapsed
    /// time, live tokens/cost, todo progress, last activity, and the input
    /// provenance harvested so far. Read-only; reuses the text pager.
    fn open_session_info(&mut self) -> Result<()> {
        let Some(active) = self
            .filtered_active_sessions()
            .get(self.session_selected)
            .cloned()
        else {
            self.status = "No session selected".to_string();
            return Ok(());
        };
        let session = &active.session;
        let progress = &active.progress;
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("Session: {}", session.id));
        lines.push(format!("Task:    {}", active.task_label));
        lines.push(format!("State:   {:?}", active.state));
        let elapsed = (timefmt::now() - session.started_at).num_seconds().max(0);
        lines.push(format!(
            "Started: {}  (elapsed {})",
            timefmt::format(&session.started_at),
            format_elapsed(elapsed),
        ));
        match progress.tokens {
            Some(tokens) => lines.push(format!("Tokens:  {tokens}")),
            None => lines.push("Tokens:  unknown".to_string()),
        }
        if let Some(cost) = progress.cost_usd {
            lines.push(format!("Cost:    ${cost:.4}"));
        }
        if let Some((done, total)) = progress.todos() {
            lines.push(format!("Todos:   {done}/{total} completed"));
        }
        if let Some(activity) = progress.last_activity.as_deref() {
            lines.push(format!("Last:    {activity}"));
        }
        let manifest = provenance::load_manifest(&self.ops.storage.provenance_dir, &session.id);
        if let Some(manifest) = manifest {
            lines.push(String::new());
            lines.push("Inputs (provenance so far):".to_string());
            lines.push(provenance::render_manifests(std::slice::from_ref(
                &manifest,
            )));
        }
        self.open_text_view(
            format!("Session · {}", session.id),
            &lines.join("\n"),
            Screen::Sessions,
        );
        Ok(())
    }

    fn open_text_view(&mut self, title: String, body: &str, return_to: Screen) {
        self.status = title.clone();
        self.text_view_return = return_to;
        self.text_view = Some(TextViewState {
            title,
            lines: body.lines().map(str::to_string).collect(),
            scroll: 0,
            // The real bound is known only at render time.
            max_scroll: u16::MAX,
        });
        self.screen = Screen::TextView;
    }

    /// Text-pager keys: scroll around the captured block, `q`/`Esc` back to the
    /// detail view it was opened from.
    fn handle_text_view_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.text_view = None;
                self.screen = self.text_view_return;
                if self.screen == Screen::Sessions {
                    self.refresh_active_sessions()?;
                }
            }
            KeyCode::Up => self.scroll_text_view(-1),
            KeyCode::Down => self.scroll_text_view(1),
            KeyCode::PageUp => self.scroll_text_view(-10),
            KeyCode::PageDown => self.scroll_text_view(10),
            KeyCode::Home => {
                if let Some(view) = self.text_view.as_mut() {
                    view.scroll = 0;
                }
            }
            KeyCode::End => {
                if let Some(view) = self.text_view.as_mut() {
                    view.scroll = view.max_scroll;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn scroll_text_view(&mut self, delta: i32) {
        if let Some(view) = self.text_view.as_mut() {
            if delta < 0 {
                view.scroll = view.scroll.saturating_sub(delta.unsigned_abs() as u16);
            } else {
                view.scroll = view
                    .scroll
                    .saturating_add(delta as u16)
                    .min(view.max_scroll);
            }
        }
    }

    fn open_session_task_detail(&mut self) -> Result<()> {
        let Some(task_id) = self
            .filtered_active_sessions()
            .get(self.session_selected)
            .map(|active_session| active_session.session.task_id.clone())
        else {
            self.status = "No session selected".to_string();
            return Ok(());
        };
        if self.ops.get_task(&task_id)?.is_none() {
            self.status = format!("Task {task_id} not found");
            return Ok(());
        }
        self.load_detail(&task_id)?;
        self.return_screen = Screen::Sessions;
        self.screen = Screen::Detail;
        Ok(())
    }

    fn open_restore_confirm(&mut self) {
        let task_id = if self.screen == Screen::Archive {
            self.filtered_archived_tasks()
                .get(self.archive_selected)
                .map(|task| task.id.clone())
        } else {
            self.current_task()
                .filter(|task| task.status == TaskStatus::Archive)
                .map(|task| task.id)
        };
        match task_id {
            Some(task_id) => self.modal = Some(ModalState::new(Modal::RestoreConfirm { task_id })),
            None => self.status = "No archived task selected".to_string(),
        }
    }

    fn refresh_active_sessions(&mut self) -> Result<()> {
        let heartbeat_timeout = self.ops.config.get_threshold("session_heartbeat_timeout")?;
        self.active_sessions = SessionManager::new(&self.project_path)
            .list_sessions_with_state(heartbeat_timeout)
            .into_iter()
            .map(|(session, state)| {
                let task = self.ops.get_task(&session.task_id).ok().flatten();
                let task_label = task
                    .as_ref()
                    .map(|task| format!("{} {}", task.id, task.title))
                    .or_else(|| {
                        session
                            .name
                            .as_ref()
                            .map(|name| format!("{} {}", session.task_id, name))
                    })
                    .unwrap_or_else(|| session.task_id.clone());
                let backend = task
                    .as_ref()
                    .and_then(|task| task.agent_backend.as_deref())
                    .unwrap_or("claude");
                let progress =
                    telemetry::read_session_progress(&self.project_path, &session.id, backend);
                let token_display = progress
                    .tokens
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                ActiveSession {
                    session,
                    state,
                    task_label,
                    token_display,
                    progress,
                }
            })
            .collect();
        // Directory order is arbitrary; keep the list stable across refreshes
        // so the selection doesn't jump.
        self.active_sessions
            .sort_by(|a, b| a.session.id.cmp(&b.session.id));
        self.session_selected = self
            .session_selected
            .min(self.active_sessions.len().saturating_sub(1));
        Ok(())
    }

    pub fn refresh_archived_tasks(&mut self) -> Result<()> {
        self.archived_tasks = self.ops.list_archived_tasks(None)?;
        self.clamp_archive_selection();
        Ok(())
    }

    fn clamp_archive_selection(&mut self) {
        self.archive_selected = self
            .archive_selected
            .min(self.filtered_archived_tasks().len().saturating_sub(1));
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.clear_search();
                self.search.active = false;
            }
            KeyCode::Enter => {
                self.search.active = false;
                self.reset_search_selection();
            }
            _ => {
                input_single_line(&mut self.search.query, key);
                self.reset_search_selection();
            }
        }
        Ok(())
    }

    fn clear_search(&mut self) {
        self.search.query = TextArea::new(vec![String::new()]);
        self.reset_search_selection();
    }

    fn reset_search_selection(&mut self) {
        match self.screen {
            Screen::Board => {
                self.focused_card = 0;
                self.clamp_focus();
            }
            Screen::Sessions => self.session_selected = 0,
            Screen::Archive => self.archive_selected = 0,
            Screen::Detail | Screen::LogView | Screen::TextView | Screen::Help => {}
        }
    }

    fn input_review_edits(&mut self, key: KeyEvent) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        detail.review_edits.input(key);
    }

    /// Persist the review-edits buffer. Saving no longer re-runs the agent —
    /// re-running is its own action (`UiAction::Rerun`, Ctrl+R or the
    /// action-bar button), which folds the saved edits into the thread.
    fn save_review_edits(&mut self) -> Result<()> {
        let Some(detail) = self.detail.as_ref() else {
            return Ok(());
        };
        if !detail.edits_editable() {
            self.status =
                "Review edits can be changed only while the task is in Review".to_string();
            return Ok(());
        }
        let text = detail.review_edits.lines().join("\n");
        let task_id = detail.task_id.clone();
        self.ops.set_review_edits(&task_id, &text)?;
        self.refresh_after_action()?;
        self.status = format!("Saved review edits for {task_id}");
        Ok(())
    }

    pub fn take_terminal_action(&mut self) -> Option<TerminalAction> {
        self.pending_terminal.take()
    }

    pub fn finish_terminal_action(&mut self, action: &TerminalAction, ok: bool) {
        let target = action.label();
        self.status = match (action, ok) {
            (TerminalAction::Attach(_), true) => format!("Detached from {target}"),
            (TerminalAction::Attach(_), false) => format!("Could not attach to {target}"),
            (TerminalAction::Foreground { .. }, true) => format!("Closed {target}"),
            (TerminalAction::Foreground { .. }, false) => format!("Could not run {target}"),
        };
    }

    fn refresh_backend_options(&self, modal: &mut ModalState) {
        let Ok(config) = self.ops.config.load() else {
            return;
        };
        self.refresh_backend_options_with_config(modal, &config);
    }

    fn refresh_backend_options_with_config(&self, modal: &mut ModalState, config: &BoardConfig) {
        let backend = selected_backend(&config.auto_launch, modal);
        let backend_settings = config
            .agents
            .get(Value::String(backend.clone()))
            .and_then(Value::as_mapping);
        if matches!(modal.modal, Modal::Settings) {
            modal.model = TextArea::new(vec![
                mapping_str(backend_settings, "model").unwrap_or_default(),
            ]);
            modal.effort = TextArea::new(vec![
                mapping_str(backend_settings, "effort").unwrap_or_default(),
            ]);
            modal.agent = TextArea::new(vec![
                mapping_str(backend_settings, "agent").unwrap_or_default(),
            ]);
        }
        let empty_model = if matches!(modal.modal, Modal::Settings) {
            "No default model"
        } else {
            "Default model"
        };
        let models = optional_empty(empty_model)
            .into_iter()
            .chain(self.backend_model_options(&backend, backend_settings, &config.auto_launch))
            .collect::<Vec<_>>();
        let mut models = models;
        if matches!(modal.modal, Modal::Settings) {
            add_missing_option(&mut models, modal.model_text());
        }
        modal.set_model_options(models);
        let empty_agent = if matches!(modal.modal, Modal::Settings) {
            "No default agent"
        } else {
            "Default agent"
        };
        let agents = optional_empty(empty_agent)
            .into_iter()
            .chain(options_from_sequence(backend_settings, "agent_options").unwrap_or_default())
            .collect::<Vec<_>>();
        let mut agents = agents;
        if matches!(modal.modal, Modal::Settings) {
            add_missing_option(&mut agents, modal.agent_text());
        }
        modal.set_agent_options(agents);
        self.refresh_effort_options_with_config(modal, config);
    }

    /// Model choices for a backend. opencode models come from the live
    /// `opencode models` catalog ordered default-first, then recently used,
    /// then alphabetical; other backends (and an unavailable opencode CLI)
    /// use the configured `models` list as-is.
    fn backend_model_options(
        &self,
        backend: &str,
        backend_settings: Option<&Mapping>,
        auto_launch: &Mapping,
    ) -> Vec<SelectOption> {
        let configured = options_from_sequence(backend_settings, "models")
            .or_else(|| options_from_sequence(Some(auto_launch), "models"))
            .unwrap_or_default();
        if !backend_has_catalog(backend) {
            return configured;
        }
        let models = cached_backend_catalog(backend, &backend_command(backend, backend_settings))
            .map(|catalog| catalog.models.clone())
            .unwrap_or_else(|| {
                configured
                    .iter()
                    .filter_map(|option| option.value.clone())
                    .collect()
            });
        // Fall back to the global auto-launch model only for opencode, whose
        // global default names an opencode model; other catalog backends
        // (omp/pi) would otherwise surface opencode's default as a bogus entry.
        let default_model = mapping_str(backend_settings, "model").or_else(|| {
            (backend == "opencode")
                .then(|| mapping_str(Some(auto_launch), "model"))
                .flatten()
        });
        sort_opencode_models(&models, default_model.as_deref(), &self.recent_models)
            .into_iter()
            .map(|model| SelectOption {
                label: model.clone(),
                value: Some(model),
            })
            .collect()
    }

    /// Effort choices depend on the backend and, for opencode, on the model:
    /// claude lists its config `efforts`; opencode offers the variants the
    /// catalog reports for the selected (or default) model.
    fn refresh_effort_options(&self, modal: &mut ModalState) {
        let Ok(config) = self.ops.config.load() else {
            return;
        };
        self.refresh_effort_options_with_config(modal, &config);
    }

    fn refresh_effort_options_with_config(&self, modal: &mut ModalState, config: &BoardConfig) {
        let backend = selected_backend(&config.auto_launch, modal);
        let backend_settings = config
            .agents
            .get(Value::String(backend.clone()))
            .and_then(Value::as_mapping);
        let mut efforts = options_from_sequence(backend_settings, "efforts").unwrap_or_default();
        if backend_has_catalog(&backend)
            && let Some(catalog) =
                cached_backend_catalog(&backend, &backend_command(&backend, backend_settings))
        {
            let model = modal
                .model_text()
                .or_else(|| mapping_str(backend_settings, "model"))
                .or_else(|| {
                    (backend == "opencode")
                        .then(|| mapping_str(Some(&config.auto_launch), "model"))
                        .flatten()
                });
            efforts = model
                .map(|model| catalog.variants_for(&model))
                .unwrap_or_default()
                .iter()
                .map(|effort| SelectOption {
                    label: effort.clone(),
                    value: Some(effort.clone()),
                })
                .collect();
        }
        let empty_effort = if matches!(modal.modal, Modal::Settings) {
            "No default effort"
        } else {
            "Default effort"
        };
        let options = optional_empty(empty_effort)
            .into_iter()
            .chain(efforts)
            .collect::<Vec<_>>();
        let mut options = options;
        if matches!(modal.modal, Modal::Settings) {
            add_missing_option(&mut options, modal.effort_text());
        }
        modal.set_effort_options(options);
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(mut modal) = self.modal.take() else {
            return Ok(false);
        };
        let backend_selected = modal.backend_selected;
        let model_selected = modal.model_selected;
        let command = normalize_command_key(key);
        if modal.discard_confirm {
            match command.code {
                KeyCode::Char('y') => return self.discard_modal(modal),
                KeyCode::Char('n') | KeyCode::Esc => {
                    modal.discard_confirm = false;
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    modal.confirm_yes_selected = !modal.confirm_yes_selected;
                }
                KeyCode::Enter if modal.confirm_yes_selected => return self.discard_modal(modal),
                KeyCode::Enter => modal.discard_confirm = false,
                _ => {}
            }
            self.modal = Some(modal);
            return Ok(true);
        }
        if is_confirmation_modal(&modal.modal) {
            match command.code {
                KeyCode::Char('y') => return self.submit_modal(modal).map(|_| true),
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.status = "Dialog cancelled".to_string();
                    return Ok(true);
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    modal.confirm_yes_selected = !modal.confirm_yes_selected;
                }
                KeyCode::Enter if modal.confirm_yes_selected => {
                    return self.submit_modal(modal).map(|_| true);
                }
                KeyCode::Enter => {
                    self.status = "Dialog cancelled".to_string();
                    return Ok(true);
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc => return self.request_modal_close(modal),
                KeyCode::Tab => modal.next_field(),
                KeyCode::BackTab => modal.prev_field(),
                KeyCode::Left | KeyCode::Right
                    if matches!(
                        modal.active_field(),
                        DialogField::Confirm | DialogField::Cancel
                    ) =>
                {
                    if modal.active_field() == DialogField::Confirm {
                        modal.focus_field(DialogField::Cancel);
                    } else {
                        modal.focus_field(DialogField::Confirm);
                    }
                }
                KeyCode::Enter if modal.submit_on_enter() => {
                    return self.submit_modal(modal).map(|_| true);
                }
                KeyCode::Enter if modal.cancel_on_enter() => {
                    return self.request_modal_close(modal);
                }
                KeyCode::Char('s')
                    if key.modifiers == KeyModifiers::CONTROL && modal.submit_on_ctrl_s() =>
                {
                    return self.submit_modal(modal).map(|_| true);
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
        }
        if modal.active_field() == DialogField::Backend
            && modal.backend_selected != backend_selected
        {
            self.refresh_backend_options(&mut modal);
        } else if modal.active_field() == DialogField::Model
            && modal.model_selected != model_selected
        {
            self.refresh_effort_options(&mut modal);
        }
        self.modal = Some(modal);
        Ok(true)
    }

    fn activate_modal_button(&mut self, button: ModalButton) -> Result<()> {
        let Some(mut modal) = self.modal.take() else {
            return Ok(());
        };
        if modal.discard_confirm {
            return match button {
                ModalButton::Yes => self.discard_modal(modal).map(|_| ()),
                ModalButton::No | ModalButton::Cancel => {
                    modal.discard_confirm = false;
                    self.modal = Some(modal);
                    Ok(())
                }
                ModalButton::Save => {
                    self.modal = Some(modal);
                    Ok(())
                }
            };
        }
        match button {
            ModalButton::Yes => self.submit_modal(modal),
            ModalButton::No => {
                self.status = "Dialog cancelled".to_string();
                Ok(())
            }
            ModalButton::Save => self.submit_modal(modal),
            ModalButton::Cancel => self.request_modal_close(modal).map(|_| ()),
        }
    }

    fn request_modal_close(&mut self, mut modal: ModalState) -> Result<bool> {
        if modal.is_dirty() {
            modal.discard_confirm = true;
            modal.confirm_yes_selected = false;
            self.modal = Some(modal);
        } else {
            self.status = "Dialog cancelled".to_string();
        }
        Ok(true)
    }

    fn discard_modal(&mut self, _modal: ModalState) -> Result<bool> {
        self.status = "Dialog cancelled".to_string();
        Ok(true)
    }

    fn submit_modal(&mut self, mut modal: ModalState) -> Result<()> {
        match modal.modal.clone() {
            Modal::Settings => {
                let project_name = modal.title_text();
                let Some(backend) = modal.backend_text() else {
                    modal.focus_field(DialogField::Backend);
                    modal.error = Some("Default backend must be selected".to_string());
                    self.modal = Some(modal);
                    return Ok(());
                };
                if project_name.trim().is_empty() {
                    modal.focus_field(DialogField::Title);
                    modal.error = Some("Project name cannot be empty".to_string());
                    self.modal = Some(modal);
                    return Ok(());
                }
                let theme_name =
                    Theme::normalize_name(&modal.theme_text().unwrap_or_default()).to_string();
                let task_sort = normalize_task_sort(
                    &modal
                        .task_sort_text()
                        .unwrap_or_else(|| TASK_SORT_NUMBER.to_string()),
                )
                .to_string();
                let save_result = (|| -> Result<()> {
                    ensure_config_write_target_is_safe(&self.ops.config)?;
                    let _lock = self.ops.storage.lock()?;
                    ensure_config_write_target_is_safe(&self.ops.config)?;
                    let mut config = self.ops.config.load_fresh()?;
                    let Some(Value::Mapping(backend_config)) =
                        config.agents.get_mut(Value::String(backend.clone()))
                    else {
                        return Err(KanbanError::Invalid(
                            "Selected backend is not configured".to_string(),
                        ));
                    };
                    config.tui.insert(
                        Value::String("name".to_string()),
                        Value::String(project_name.clone()),
                    );
                    config.tui.insert(
                        Value::String("theme".to_string()),
                        Value::String(theme_name.clone()),
                    );
                    config.tui.insert(
                        Value::String("task_sort".to_string()),
                        Value::String(task_sort.clone()),
                    );
                    config.auto_launch.insert(
                        Value::String("default_agent".to_string()),
                        Value::String(backend.clone()),
                    );
                    for (key, value) in [
                        ("model", modal.model_text()),
                        ("effort", modal.effort_text()),
                        ("agent", modal.agent_text()),
                    ] {
                        backend_config.insert(
                            Value::String(key.to_string()),
                            value.map(Value::String).unwrap_or(Value::Null),
                        );
                    }
                    retain_source_legacy_auto_launch_keys(
                        &self.ops.config.config_file,
                        &mut config,
                    )?;
                    self.ops.config.save(&config)
                })();
                if let Err(err) = save_result {
                    let message = format!("Could not save project settings: {err}");
                    modal.error = Some(message.clone());
                    self.status = message;
                    self.modal = Some(modal);
                    return Ok(());
                }
                self.settings.project_name = project_name;
                self.settings.theme_name = theme_name.clone();
                self.settings.task_sort = task_sort;
                self.theme = Theme::named(&theme_name);
                self.refresh_after_action()?;
                self.status = "Project settings saved".to_string();
            }
            Modal::NewTask { target_status } => {
                let title = modal.title_text();
                if title.trim().is_empty() {
                    modal.focus_field(DialogField::Title);
                    modal.error = Some("Task title cannot be empty".to_string());
                    self.modal = Some(modal);
                    return Ok(());
                }
                let new_task = NewTask {
                    title,
                    description: modal.description_text(),
                    ai_model: modal.model_text(),
                    ai_effort: modal.effort_text(),
                    agent_backend: modal.backend_text(),
                    agent_name: modal.agent_text(),
                    interactive: modal.interactive,
                    chained_to: modal.chain_text(),
                };
                let target = target_status
                    .as_deref()
                    .unwrap_or(TaskStatus::Todo.as_str())
                    .parse::<TaskStatus>()?;
                let task = self.ops.create_task_in_status(new_task, target)?;
                if target == TaskStatus::Todo {
                    self.status = format!("Created {}", task.id);
                } else {
                    self.status = format!("Created {} in {}", task.id, target.as_str());
                }
                self.refresh_after_action()?;
            }
            Modal::EditTask { task_id } => {
                if modal.title_text().trim().is_empty() {
                    modal.focus_field(DialogField::Title);
                    modal.error = Some("Task title cannot be empty".to_string());
                    self.modal = Some(modal);
                    return Ok(());
                }
                let updated = self.ops.update_task(
                    &task_id,
                    TaskPatch {
                        title: Some(modal.title_text()),
                        description: Some(modal.description_text()),
                        ai_model: Some(modal.model_text()),
                        ai_effort: Some(modal.effort_text()),
                        agent_backend: Some(modal.backend_text()),
                        agent_name: Some(modal.agent_text()),
                        interactive: Some(modal.interactive),
                        chained_to: Some(modal.chain_text()),
                        ..Default::default()
                    },
                )?;
                self.refresh_after_action()?;
                self.status = if updated.is_some() {
                    format!("Updated {task_id}")
                } else {
                    format!("Task {task_id} not found")
                };
            }
            Modal::MoveTask { task_id } => {
                let target = modal.target_text();
                if target.trim().is_empty() {
                    modal.error = Some("Move target cannot be empty".to_string());
                    self.modal = Some(modal);
                    return Ok(());
                }
                self.ops.move_task(&task_id, &target, false)?;
                self.refresh_after_action()?;
                self.status = format!("Moved {task_id} to {target}");
            }
            Modal::DeleteConfirm { task_id } => {
                self.ops.abandon_task(&task_id)?;
                if self
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.task_id == task_id)
                {
                    self.close_detail()?;
                }
                self.board = BoardSnapshot::load(&self.ops)?;
                self.refresh_archived_tasks()?;
                self.clamp_focus();
                self.status = format!("Deleted {task_id}");
            }
            Modal::KillSessionConfirm { session_id } => {
                let stopped = self.ops.stop_session(&session_id)?;
                self.refresh_active_sessions()?;
                self.refresh_after_action()?;
                self.status = match stopped {
                    Some(task) => format!("Stopped {session_id} (task {})", task.id),
                    None => format!("Session {session_id} not found"),
                };
            }
            Modal::RestoreConfirm { task_id } => {
                let restored = self.ops.unarchive_task(&task_id)?;
                self.refresh_after_action()?;
                self.status = match restored {
                    Some(_) => format!("Restored {task_id} to To Do"),
                    None => format!("{task_id} is not archived"),
                };
            }
            Modal::RevertConfirm { task_id } => {
                let session_id = format!("ses-revert-{}", timefmt::now().format("%Y%m%d-%H%M%S"));
                self.status = if self.ops.launch_revert(&task_id, &session_id)? {
                    format!("Revert of {task_id} launched ({session_id})")
                } else {
                    format!("Failed to launch revert for {task_id}")
                };
                self.refresh_after_action()?;
            }
            Modal::BulkConfirm { action, task_ids } => {
                let (from, to) = match action {
                    BulkAction::ArchiveAllDone => (TaskStatus::Done, TaskStatus::Archive),
                    BulkAction::MarkReviewDone => (TaskStatus::Review, TaskStatus::Done),
                };
                let Some(moved) = self.ops.bulk_move_exact(from, to, &task_ids)? else {
                    self.refresh_after_action()?;
                    self.open_bulk_confirm(action);
                    if self.modal.is_some() {
                        self.status =
                            "Tasks changed while confirmation was open; confirm the updated set"
                                .to_string();
                    }
                    return Ok(());
                };
                self.refresh_after_action()?;
                self.status = match action {
                    BulkAction::ArchiveAllDone => format!("Archived {} task(s)", moved.len()),
                    BulkAction::MarkReviewDone => {
                        format!("Marked {} Review task(s) Done", moved.len())
                    }
                };
            }
            Modal::AddMessage { task_id } => {
                let body = modal.description_text();
                if body.trim().is_empty() {
                    modal.error = Some("Message text cannot be empty".to_string());
                    self.modal = Some(modal);
                    return Ok(());
                }
                if modal.kind_selected == 0 {
                    ContextManager::new(&self.project_path).append_context(
                        &task_id,
                        &body,
                        "user",
                        &self.ops.storage,
                    )?;
                    self.status = format!("Added context to {task_id}");
                } else {
                    self.ops
                        .suggest_improvement(&task_id, &body, "user", vec![])?;
                    self.status = format!("Added suggestion to {task_id}");
                }
                self.refresh_after_action()?;
            }
            Modal::AnswerQuestion { task_id, .. } => {
                let answer = modal.answer_text();
                if answer.trim().is_empty() {
                    modal.error = Some("Answer cannot be empty".to_string());
                    self.modal = Some(modal);
                    return Ok(());
                }
                let question_ref = modal.selected_question_ref().ok_or_else(|| {
                    crate::core::error::KanbanError::Invalid("No question selected".to_string())
                })?;
                self.ops.answer_question(&task_id, question_ref, &answer)?;
                self.refresh_after_action()?;
                self.status = format!("Answered question on {task_id}");
            }
        }
        Ok(())
    }

    fn cycle_theme(&mut self) -> Result<()> {
        let next = Theme::next_name(&self.settings.theme_name).to_string();
        let save_result = (|| -> Result<()> {
            ensure_config_write_target_is_safe(&self.ops.config)?;
            let _lock = self.ops.storage.lock()?;
            ensure_config_write_target_is_safe(&self.ops.config)?;
            let mut config = self.ops.config.load_fresh()?;
            config.tui.insert(
                Value::String("theme".to_string()),
                Value::String(next.clone()),
            );
            retain_source_legacy_auto_launch_keys(&self.ops.config.config_file, &mut config)?;
            self.ops.config.save(&config)
        })();
        if let Err(err) = save_result {
            self.status = format!("Could not save theme: {err}");
            return Ok(());
        }
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
        let task_sort =
            normalize_task_sort(&tui_string(&config.tui, "task_sort", TASK_SORT_NUMBER));
        let (sort_by, order) = match task_sort {
            TASK_SORT_UPDATED_ASC => ("updated", "asc"),
            TASK_SORT_UPDATED_DESC => ("updated", "desc"),
            _ => ("id", "asc"),
        };
        let tasks = ops.list_tasks(None, None, sort_by, order)?;
        let heartbeat_timeout = ops.config.get_threshold("session_heartbeat_timeout")?;
        let sessions_by_id = SessionManager::new(&ops.storage.project_path)
            .list_sessions_with_state(heartbeat_timeout)
            .into_iter()
            .map(|(session, state)| {
                let deadline = match state {
                    SessionState::Live => (session.status == SessionStatus::Active)
                        .then(|| session.last_seen + chrono::Duration::seconds(heartbeat_timeout)),
                    // A waiting card flips to crashed when the declared
                    // deadline passes; the resume relaunch then reloads it.
                    SessionState::Waiting => session.wait_until,
                    SessionState::Crashed => None,
                };
                (
                    session.id,
                    (session.task_id, state, deadline, session.wait_note),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut session_states = tasks
            .iter()
            .filter_map(|task| {
                let session_id = task.session.as_ref()?;
                let (session_task_id, state, _, _) = sessions_by_id.get(session_id)?;
                if session_task_id != &task.id {
                    return None;
                }
                Some((task.id.clone(), *state))
            })
            .collect::<HashMap<_, _>>();
        // An In Progress task whose session record is closed or gone is
        // stranded — nothing will move it. Surface it as crashed instead of
        // letting it look idle.
        for task in &tasks {
            if task.status == TaskStatus::InProgress
                && task.session.is_some()
                && !session_states.contains_key(&task.id)
                && !(task.has_questions && ops.first_open_question(&task.id)?.is_some())
            {
                session_states.insert(task.id.clone(), SessionState::Crashed);
            }
        }
        let session_deadlines = tasks
            .iter()
            .filter_map(|task| {
                let session_id = task.session.as_ref()?;
                let (session_task_id, _, deadline, _) = sessions_by_id.get(session_id)?;
                if session_task_id != &task.id {
                    return None;
                }
                Some((task.id.clone(), (*deadline)?))
            })
            .collect::<HashMap<_, _>>();
        let session_wait_notes = tasks
            .iter()
            .filter_map(|task| {
                let session_id = task.session.as_ref()?;
                let (session_task_id, state, _, note) = sessions_by_id.get(session_id)?;
                if session_task_id != &task.id || *state != SessionState::Waiting {
                    return None;
                }
                note.as_ref().map(|note| (task.id.clone(), note.clone()))
            })
            .collect::<HashMap<_, _>>();
        let session_wait_deadlines = tasks
            .iter()
            .filter_map(|task| {
                let session_id = task.session.as_ref()?;
                let (session_task_id, state, deadline, _) = sessions_by_id.get(session_id)?;
                if session_task_id != &task.id || *state != SessionState::Waiting {
                    return None;
                }
                Some((task.id.clone(), (*deadline)?))
            })
            .collect::<HashMap<_, _>>();
        let extras = Self::load_extras(ops, &tasks, &session_states)?;
        for task in tasks {
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
            extras,
            session_states,
            session_deadlines,
            session_wait_deadlines,
            session_wait_notes,
            fingerprint: ops.storage.tui_fingerprint(),
        })
    }

    /// Question previews and waiting-agent flags for questioned tasks only,
    /// so a snapshot rebuild stays cheap for ordinary boards.
    fn load_extras(
        ops: &Operations,
        tasks: &[Task],
        session_states: &HashMap<String, SessionState>,
    ) -> Result<HashMap<String, CardExtra>> {
        let mut extras = HashMap::new();
        let questioned = tasks.iter().filter(|task| task.has_questions);
        if questioned.clone().next().is_none() {
            return Ok(extras);
        }
        for task in questioned {
            let question_preview = ops
                .first_open_question(&task.id)?
                .map(|message| message.body.lines().next().unwrap_or_default().to_string());
            let waiting = task.interactive
                && question_preview.is_some()
                && session_states.get(&task.id) == Some(&SessionState::Live);
            extras.insert(
                task.id.clone(),
                CardExtra {
                    question_preview,
                    waiting,
                },
            );
        }
        Ok(extras)
    }
}

/// `1h 04m`, `12m 30s`, or `45s` from a non-negative second count.
fn format_elapsed(seconds: i64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

fn next_index(current: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        current.saturating_add(1).min(len - 1)
    }
}

pub(super) fn normalize_command_key(mut key: KeyEvent) -> KeyEvent {
    let KeyCode::Char(ch) = key.code else {
        return key;
    };
    let normalized = match ch {
        'й' | 'Й' => 'q',
        'ц' | 'Ц' => 'w',
        'у' | 'У' => 'e',
        'е' | 'Е' => 't',
        'к' => 'r',
        'К' => 'R',
        'н' | 'Н' => 'y',
        'и' | 'И' => 'b',
        'г' | 'Г' => 'u',
        'ф' => 'a',
        'Ф' => 'A',
        'ы' | 'Ы' => 's',
        'в' | 'В' => 'd',
        'л' | 'Л' => 'k',
        'д' | 'Д' => 'l',
        'с' | 'С' => 'c',
        'м' | 'М' => 'v',
        'т' | 'Т' => 'n',
        'ь' | 'Ь' => 'm',
        'ч' | 'Ч' => 'x',
        'щ' | 'Щ' => 'o',
        '.' => '/',
        ',' => '?',
        _ => ch,
    };
    key.code = KeyCode::Char(normalized);
    key
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    let key = normalize_command_key(key);
    matches!(
        (key.code, key.modifiers),
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
    )
}

fn lines_or_empty(text: &str) -> Vec<String> {
    let lines = text.lines().map(sanitize_terminal_text).collect::<Vec<_>>();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn textarea_text(textarea: &TextArea<'_>) -> String {
    textarea.lines().join("\n")
}

/// Bytes of `.kanban/logs/<session>.log` kept by the log-view pager. Enough
/// for a useful tail without holding a multi-megabyte agent log in memory.
const LOG_TAIL_BYTES: usize = 64 * 1024;

pub(super) fn load_log_tail(project_path: &Path, session_id: &str) -> Vec<String> {
    if crate::core::session::SessionManager::validate_session_id(session_id).is_err() {
        return vec!["(invalid session id)".to_string()];
    }
    let log_file = project_path
        .join(".kanban")
        .join("logs")
        .join(format!("{session_id}.log"));
    let Ok(bytes) = fs::read(&log_file) else {
        return vec!["(no log file for this session)".to_string()];
    };
    let skipped = bytes.len().saturating_sub(LOG_TAIL_BYTES);
    let text = String::from_utf8_lossy(&bytes[skipped..]);
    let mut lines = text.lines().map(sanitize_terminal_text).collect::<Vec<_>>();
    if skipped > 0 {
        if !lines.is_empty() {
            // The byte cut almost certainly split the first line.
            lines.remove(0);
        }
        lines.insert(0, format!("… older output omitted ({skipped} bytes)"));
    }
    if lines.is_empty() {
        lines.push("(log is empty)".to_string());
    }
    lines
}

fn input_single_line(textarea: &mut TextArea<'static>, key: KeyEvent) {
    if matches!(key.code, KeyCode::Enter) {
        return;
    }
    textarea.input(key);
}

/// Keys that belong to a focused textarea (typing and cursor movement),
/// excluding chorded shortcuts.
/// Flatten pasted text for fields that hold a single line (Enter submits them).
fn one_line_paste(text: &str) -> String {
    sanitize_paste_text(text).replace('\n', " ")
}

fn is_text_input_key(key: KeyEvent) -> bool {
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

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn is_confirmation_modal(modal: &Modal) -> bool {
    matches!(
        modal,
        Modal::DeleteConfirm { .. }
            | Modal::RevertConfirm { .. }
            | Modal::BulkConfirm { .. }
            | Modal::KillSessionConfirm { .. }
            | Modal::RestoreConfirm { .. }
    )
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

fn mapping_str(mapping: Option<&Mapping>, key: &str) -> Option<String> {
    mapping?
        .get(Value::String(key.to_string()))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn backend_command(backend: &str, backend_settings: Option<&Mapping>) -> String {
    mapping_str(backend_settings, "command").unwrap_or_else(|| backend.to_string())
}

/// (backend, command) pairs for every configured backend whose model/effort
/// catalog kanban can poll, so the TUI can warm and read each one.
fn catalog_backend_commands(config: &BoardConfig) -> Vec<(String, String)> {
    config
        .agents
        .iter()
        .filter_map(|(key, value)| {
            let backend = key.as_str()?;
            if !backend_has_catalog(backend) {
                return None;
            }
            let command = backend_command(backend, value.as_mapping());
            Some((backend.to_string(), command))
        })
        .collect()
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

fn selector_index(modal: &ModalState, field: DialogField) -> Option<usize> {
    match field {
        DialogField::Backend => Some(modal.backend_selected),
        DialogField::Model => Some(modal.model_selected),
        DialogField::Effort => Some(modal.effort_selected),
        DialogField::Agent => Some(modal.agent_selected),
        DialogField::Theme => Some(modal.theme_selected),
        DialogField::TaskSort => Some(modal.task_sort_selected),
        DialogField::ChainTo => Some(modal.chain_selected),
        DialogField::TargetStatus => Some(modal.status_selected),
        DialogField::MessageKind => Some(modal.kind_selected),
        DialogField::Question => Some(modal.question_selected),
        DialogField::Variant => modal.variant_selected,
        DialogField::Title
        | DialogField::Description
        | DialogField::Interactive
        | DialogField::Answer
        | DialogField::Confirm
        | DialogField::Cancel => None,
    }
}

fn add_missing_option(options: &mut Vec<SelectOption>, value: Option<String>) {
    let Some(value) = value else {
        return;
    };
    if options
        .iter()
        .all(|option| option.value.as_deref() != Some(value.as_str()))
    {
        options.push(SelectOption {
            label: value.clone(),
            value: Some(value),
        });
    }
}

fn ensure_config_write_target_is_safe(config: &crate::core::config::Config) -> Result<()> {
    for path in [&config.kanban_dir, &config.config_file] {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(KanbanError::Permission(format!(
                    "Refusing to save through symlinked {}",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("config path")
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// `Config::load` fills compatibility defaults for readers. Before a Settings
/// write, remove legacy auto-launch defaults that were absent in the source so
/// the UI never starts serializing keys it does not own.
fn retain_source_legacy_auto_launch_keys(
    config_file: &Path,
    config: &mut crate::core::config::BoardConfig,
) -> Result<()> {
    let raw = fs::read_to_string(config_file)?;
    let source: Value = serde_yaml_ng::from_str(&raw)?;
    let source_auto_launch = source
        .as_mapping()
        .and_then(|mapping| mapping.get("auto_launch"))
        .and_then(Value::as_mapping);
    for key in ["model", "models", "agent"] {
        if !source_auto_launch.is_some_and(|mapping| mapping.contains_key(key)) {
            config.auto_launch.remove(key);
        }
    }
    Ok(())
}

fn load_settings(ops: &Operations) -> Result<TuiSettings> {
    let config = ops.config.load()?;
    let card_height_lines = tui_int(&config.tui, "card_height_lines", 4).max(1) as u16;
    Ok(TuiSettings {
        project_name: tui_string(&config.tui, "name", "Kanban"),
        card_height_lines,
        max_tasks_per_column: tui_int(&config.tui, "max_tasks_per_column", 100).max(1) as usize,
        refresh_interval: Duration::from_secs(
            ops.config.get_threshold("tui_refresh_interval")?.max(1) as u64,
        ),
        theme_name: Theme::normalize_name(&tui_string(&config.tui, "theme", "dark")).to_string(),
        task_sort: normalize_task_sort(&tui_string(&config.tui, "task_sort", TASK_SORT_NUMBER))
            .to_string(),
    })
}

pub(super) fn normalize_task_sort(value: &str) -> &'static str {
    match value {
        TASK_SORT_UPDATED_ASC => TASK_SORT_UPDATED_ASC,
        TASK_SORT_UPDATED_DESC | TASK_SORT_LEGACY_COMPLETION => TASK_SORT_UPDATED_DESC,
        _ => TASK_SORT_NUMBER,
    }
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
