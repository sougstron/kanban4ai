use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui_textarea::{TextArea, WrapMode};

use crate::core::models::Task;
use crate::core::operations::QuestionRef;
use crate::core::update;
use crate::core::vcs::Availability;

use super::app::{App, HitAction, Hitbox, UiAction};
use super::card::{sanitize_paste_text, sanitize_terminal_text, truncate_display};

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
    AgentSettings,
    DesignerAgentSettings,
    ReviewerAgentSettings,
    Backend,
    Model,
    Effort,
    Agent,
    ChainTo,
    UseOrchestrator,
    UseDesigner,
    UseReviewer,
    TargetStatus,
    MessageKind,
    Question,
    Variant,
    Answer,
    Theme,
    TaskSort,
    HideKanbanMessages,
    EscapeToProjects,
    ProjectSort,
    UpdateCheckOnOpen,
    QueueEnabled,
    MaxRunningTotal,
    MaxRunningDesigner,
    MaxRunningReviewer,
    MaxRunningExecutor,
    MaxRunningPerBackend,
    MaxRunningPerBackendModel,
    AutoRestartEnabled,
    AutoRestartDelays,
    DesignerEnabled,
    DesignerBackend,
    DesignerModel,
    DesignerEffort,
    DesignerAgent,
    ReviewerEnabled,
    ReviewerBackend,
    ReviewerModel,
    ReviewerEffort,
    ReviewerAgent,
    ReviewerOnChanges,
    ReviewerMaxRounds,
    ExecutorMiddle1,
    ExecutorMiddle2,
    ExecutorMiddle3,
    ExecutorCheap1,
    ExecutorCheap2,
    ExecutorCheap3,
    ExecutorWeekThreshold,
    ExecutorFiveHourThreshold,
    IsolationStatus,
    Confirm,
    Cancel,
    PurgeData,
}

const TASK_FORM_FIELDS: [DialogField; 7] = [
    DialogField::Title,
    DialogField::Description,
    DialogField::AgentSettings,
    DialogField::ChainTo,
    DialogField::UseOrchestrator,
    DialogField::UseDesigner,
    DialogField::UseReviewer,
];

/// Which page of the project settings dialog is showing. One [`ModalState`]
/// holds every tab's field state, so switching tabs never loses an edit —
/// Save writes the whole dialog, not just the visible tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Common,
    Designer,
    Reviewer,
    Executor,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 4] = [
        SettingsTab::Common,
        SettingsTab::Designer,
        SettingsTab::Reviewer,
        SettingsTab::Executor,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::Common => "Common",
            SettingsTab::Designer => "Designer",
            SettingsTab::Reviewer => "Reviewer",
            SettingsTab::Executor => "Executor",
        }
    }

    /// Degrades with dialog width (see `render_settings_form`).
    fn short_label(self) -> &'static str {
        match self {
            SettingsTab::Common => "Com",
            SettingsTab::Designer => "Des",
            SettingsTab::Reviewer => "Rev",
            SettingsTab::Executor => "Exe",
        }
    }

    /// Arrow keys walk the tabs and wrap around.
    pub fn next(self) -> Self {
        let index = self.index();
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let index = self.index();
        let len = Self::ALL.len();
        Self::ALL[(index + len - 1) % len]
    }
}

/// One settings tab's field page, Save/Cancel inclusive: the buttons render
/// under every tab and save the whole dialog, not just the visible one.
const SETTINGS_PAGE_COMMON_FIELDS: [DialogField; 17] = [
    DialogField::Title,
    DialogField::AgentSettings,
    DialogField::Theme,
    DialogField::TaskSort,
    DialogField::HideKanbanMessages,
    DialogField::QueueEnabled,
    DialogField::MaxRunningTotal,
    DialogField::MaxRunningDesigner,
    DialogField::MaxRunningReviewer,
    DialogField::MaxRunningExecutor,
    DialogField::MaxRunningPerBackend,
    DialogField::MaxRunningPerBackendModel,
    DialogField::AutoRestartEnabled,
    DialogField::AutoRestartDelays,
    DialogField::IsolationStatus,
    DialogField::Confirm,
    DialogField::Cancel,
];

const SETTINGS_PAGE_DESIGNER_FIELDS: [DialogField; 4] = [
    DialogField::DesignerEnabled,
    DialogField::DesignerAgentSettings,
    DialogField::Confirm,
    DialogField::Cancel,
];

const SETTINGS_PAGE_REVIEWER_FIELDS: [DialogField; 6] = [
    DialogField::ReviewerEnabled,
    DialogField::ReviewerAgentSettings,
    DialogField::ReviewerOnChanges,
    DialogField::ReviewerMaxRounds,
    DialogField::Confirm,
    DialogField::Cancel,
];

const SETTINGS_PAGE_EXECUTOR_FIELDS: [DialogField; 10] = [
    DialogField::ExecutorMiddle1,
    DialogField::ExecutorMiddle2,
    DialogField::ExecutorMiddle3,
    DialogField::ExecutorCheap1,
    DialogField::ExecutorCheap2,
    DialogField::ExecutorCheap3,
    DialogField::ExecutorWeekThreshold,
    DialogField::ExecutorFiveHourThreshold,
    DialogField::Confirm,
    DialogField::Cancel,
];

/// The whole field page of one settings tab, buttons included.
pub(crate) fn settings_page_fields(tab: SettingsTab) -> &'static [DialogField] {
    match tab {
        SettingsTab::Common => &SETTINGS_PAGE_COMMON_FIELDS,
        SettingsTab::Designer => &SETTINGS_PAGE_DESIGNER_FIELDS,
        SettingsTab::Reviewer => &SETTINGS_PAGE_REVIEWER_FIELDS,
        SettingsTab::Executor => &SETTINGS_PAGE_EXECUTOR_FIELDS,
    }
}

/// The fields visible on one settings tab (Save/Cancel excluded — they sit
/// under every tab).
pub(crate) fn settings_fields(tab: SettingsTab) -> &'static [DialogField] {
    let page = settings_page_fields(tab);
    &page[..page.len() - 2]
}

/// The tab a settings field lives on — the inverse of [`settings_fields`].
/// A validation error focuses its own tab through this map.
pub(crate) fn tab_for_field(field: DialogField) -> Option<SettingsTab> {
    match field {
        DialogField::Title
        | DialogField::AgentSettings
        | DialogField::Theme
        | DialogField::TaskSort
        | DialogField::HideKanbanMessages
        | DialogField::QueueEnabled
        | DialogField::MaxRunningTotal
        | DialogField::MaxRunningDesigner
        | DialogField::MaxRunningReviewer
        | DialogField::MaxRunningExecutor
        | DialogField::MaxRunningPerBackend
        | DialogField::MaxRunningPerBackendModel
        | DialogField::AutoRestartEnabled
        | DialogField::AutoRestartDelays
        | DialogField::IsolationStatus => Some(SettingsTab::Common),
        DialogField::DesignerEnabled | DialogField::DesignerAgentSettings => {
            Some(SettingsTab::Designer)
        }
        DialogField::ReviewerEnabled
        | DialogField::ReviewerAgentSettings
        | DialogField::ReviewerOnChanges
        | DialogField::ReviewerMaxRounds => Some(SettingsTab::Reviewer),
        DialogField::ExecutorMiddle1
        | DialogField::ExecutorMiddle2
        | DialogField::ExecutorMiddle3
        | DialogField::ExecutorCheap1
        | DialogField::ExecutorCheap2
        | DialogField::ExecutorCheap3
        | DialogField::ExecutorWeekThreshold
        | DialogField::ExecutorFiveHourThreshold => Some(SettingsTab::Executor),
        _ => None,
    }
}

const PRIMARY_AGENT_FIELDS: [DialogField; 6] = [
    DialogField::Backend,
    DialogField::Model,
    DialogField::Effort,
    DialogField::Agent,
    DialogField::Confirm,
    DialogField::Cancel,
];

const DESIGNER_AGENT_FIELDS: [DialogField; 6] = [
    DialogField::DesignerBackend,
    DialogField::DesignerModel,
    DialogField::DesignerEffort,
    DialogField::DesignerAgent,
    DialogField::Confirm,
    DialogField::Cancel,
];

const REVIEWER_AGENT_FIELDS: [DialogField; 6] = [
    DialogField::ReviewerBackend,
    DialogField::ReviewerModel,
    DialogField::ReviewerEffort,
    DialogField::ReviewerAgent,
    DialogField::Confirm,
    DialogField::Cancel,
];

const GLOBAL_SETTINGS_FORM_FIELDS: [DialogField; 3] = [
    DialogField::EscapeToProjects,
    DialogField::ProjectSort,
    DialogField::UpdateCheckOnOpen,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalButton {
    Save,
    Cancel,
    Yes,
    No,
}

/// Which backend/model/effort/agent selector group a settings or task field
/// belongs to. The task form and the settings default-agent fields share
/// [`AgentSlot::Primary`]; designer and reviewer bots have their own catalogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSlot {
    Primary,
    Designer,
    Reviewer,
}

/// Compact one-line-per-entry map editors live in these textareas:
/// `backend: N` for per-backend caps and `backend/model: N` for the model
/// cap (`claude/opus: 1`, `opencode/openai/gpt-5.5: 2`). A bare model id is
/// never stored — save prefixes the selected default backend when the user
/// omits it, so the census key always matches.
#[derive(Clone)]
pub(crate) struct AgentPicker {
    backend: TextArea<'static>,
    model: TextArea<'static>,
    effort: TextArea<'static>,
    agent: TextArea<'static>,
    backend_options: Vec<SelectOption>,
    model_options: Vec<SelectOption>,
    effort_options: Vec<SelectOption>,
    agent_options: Vec<SelectOption>,
    pub(crate) backend_selected: usize,
    pub(crate) model_selected: usize,
    pub(crate) effort_selected: usize,
    pub(crate) agent_selected: usize,
    backend_filter: String,
    model_filter: String,
}

struct AgentPopupState {
    slot: AgentSlot,
    parent_field_index: usize,
    parent_form_scroll: usize,
    field_index: usize,
    form_scroll: usize,
    original: AgentPicker,
}

impl AgentPicker {
    fn new() -> Self {
        Self {
            backend: one_line(""),
            model: one_line(""),
            effort: one_line(""),
            agent: one_line(""),
            backend_options: Vec::new(),
            model_options: Vec::new(),
            effort_options: Vec::new(),
            agent_options: Vec::new(),
            backend_selected: 0,
            model_selected: 0,
            effort_selected: 0,
            agent_selected: 0,
            backend_filter: String::new(),
            model_filter: String::new(),
        }
    }
}

pub struct ModalState {
    pub settings_tab: SettingsTab,
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
    pub use_orchestrator: bool,
    pub use_designer: bool,
    pub use_reviewer: bool,
    pub escape_to_projects: bool,
    pub update_check_on_open: bool,
    /// Why the last deliberate "Check now" failed, shown on the Updates row.
    pub update_check_error: Option<String>,
    /// Package-manager upgrade command for a package-managed install. `Some`
    /// replaces the "Update now" button with the guidance text, `None` means
    /// self-update may replace the binary.
    pub update_upgrade_command: Option<String>,
    pub project_sort: TextArea<'static>,
    pub form_scroll: usize,
    pub error: Option<String>,
    /// Typed filters for the selectors that can hold long lists. Keyed by the
    /// field so the same plumbing covers any selector we later opt in.
    pub backend_filter: String,
    pub model_filter: String,
    pub chain_filter: String,
    /// Selector whose filter matched nothing when Enter was pressed. The
    /// section renders in the error colour until anything is picked or the
    /// filter changes.
    pub filter_error: Option<DialogField>,
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
    pub hide_kanban_messages: bool,
    pub project_sort_options: Vec<SelectOption>,
    pub project_sort_selected: usize,
    pub purge_data: bool,
    pub queue_enabled: bool,
    pub max_running_total: TextArea<'static>,
    pub max_running_designer: TextArea<'static>,
    pub max_running_reviewer: TextArea<'static>,
    pub max_running_executor: TextArea<'static>,
    pub max_running_per_backend: TextArea<'static>,
    pub max_running_per_backend_model: TextArea<'static>,
    pub auto_restart_enabled: bool,
    pub auto_restart_delays: TextArea<'static>,
    pub designer_enabled: bool,
    pub(crate) designer: AgentPicker,
    pub reviewer_enabled: bool,
    pub(crate) reviewer: AgentPicker,
    pub reviewer_on_changes: TextArea<'static>,
    pub reviewer_on_changes_options: Vec<SelectOption>,
    /// Executor-pool slot selectors share one backend/model option list;
    /// each slot keeps its own selection and filter text. Index order is
    /// priority: 0-2 middle ("smart"), 3-5 cheap.
    pub executor_slot_options: Vec<SelectOption>,
    pub executor_selected: [usize; 6],
    pub executor_filters: [String; 6],
    /// Remaining-percent floors a provider must clear before a pool
    /// candidate is considered usable.
    pub executor_week_threshold: TextArea<'static>,
    pub executor_five_hour_threshold: TextArea<'static>,
    pub reviewer_on_changes_selected: usize,
    pub reviewer_max_rounds: TextArea<'static>,
    /// Availability probe for the current project, taken once when the
    /// settings dialog opens (the probe runs git subprocesses).
    pub isolation_status: Option<Availability>,
    agent_popup: Option<AgentPopupState>,
}

impl ModalState {
    pub fn new(modal: Modal) -> Self {
        let wraps_description = matches!(&modal, Modal::NewTask { .. } | Modal::EditTask { .. });
        Self {
            modal,
            settings_tab: SettingsTab::Common,
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
            use_orchestrator: false,
            use_designer: false,
            use_reviewer: false,
            escape_to_projects: false,
            update_check_on_open: true,
            update_check_error: None,
            update_upgrade_command: None,
            project_sort: one_line("name"),
            form_scroll: 0,
            error: None,
            backend_filter: String::new(),
            model_filter: String::new(),
            chain_filter: String::new(),
            filter_error: None,
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
            hide_kanban_messages: false,
            project_sort_options: Vec::new(),
            project_sort_selected: 0,
            purge_data: false,
            queue_enabled: true,
            max_running_total: one_line("3"),
            max_running_designer: one_line("1"),
            max_running_reviewer: one_line("1"),
            max_running_executor: one_line("3"),
            executor_slot_options: Vec::new(),
            executor_selected: [0; 6],
            executor_filters: Default::default(),
            executor_week_threshold: one_line("5"),
            executor_five_hour_threshold: one_line("15"),
            max_running_per_backend: TextArea::default(),
            max_running_per_backend_model: TextArea::default(),
            auto_restart_enabled: true,
            auto_restart_delays: one_line("1, 30, 270"),
            designer_enabled: false,
            designer: AgentPicker::new(),
            reviewer_enabled: false,
            reviewer: AgentPicker::new(),
            reviewer_on_changes: one_line("in_progress"),
            reviewer_on_changes_options: Vec::new(),
            reviewer_on_changes_selected: 0,
            reviewer_max_rounds: one_line("3"),
            isolation_status: None,
            agent_popup: None,
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
        state.use_orchestrator = task.use_orchestrator;
        state.use_designer = task.use_designer;
        state.use_reviewer = task.use_reviewer;
        state
    }

    pub fn fields(&self) -> &'static [DialogField] {
        if let Some(popup) = &self.agent_popup {
            return agent_fields(popup.slot);
        }
        self.parent_fields()
    }

    fn parent_fields(&self) -> &'static [DialogField] {
        match self.modal {
            Modal::NewTask { .. } | Modal::EditTask { .. } => &[
                DialogField::Title,
                DialogField::Description,
                DialogField::AgentSettings,
                DialogField::ChainTo,
                DialogField::UseOrchestrator,
                DialogField::UseDesigner,
                DialogField::UseReviewer,
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
            Modal::Settings => settings_page_fields(self.settings_tab),
            Modal::GlobalSettings => &[
                DialogField::EscapeToProjects,
                DialogField::ProjectSort,
                DialogField::UpdateCheckOnOpen,
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
        let fields = self.fields();
        let index = self
            .agent_popup
            .as_ref()
            .map(|popup| popup.field_index)
            .unwrap_or(self.field_index);
        fields[index.min(fields.len().saturating_sub(1))]
    }

    /// Single entry point for every focus change, so a filter can never
    /// outlive the visit that typed it. Leaving a selector drops its filter
    /// text and any error it was showing; coming back always starts from the
    /// full list instead of a narrowing the user has since forgotten about.
    fn set_field_index(&mut self, index: usize) {
        let current = self
            .agent_popup
            .as_ref()
            .map(|popup| popup.field_index)
            .unwrap_or(self.field_index);
        if index != current {
            let leaving = self.active_field();
            if let Some(filter) = self.field_filter_mut(leaving) {
                filter.clear();
            }
            if self.filter_error == Some(leaving) {
                self.filter_error = None;
            }
        }
        if let Some(popup) = self.agent_popup.as_mut() {
            popup.field_index = index;
        } else {
            self.field_index = index;
        }
        self.ensure_active_field_visible();
    }

    pub fn next_field(&mut self) {
        let len = self.fields().len();
        if len > 0 {
            let current = self
                .agent_popup
                .as_ref()
                .map(|popup| popup.field_index)
                .unwrap_or(self.field_index);
            self.set_field_index((current + 1) % len);
        }
    }

    pub fn prev_field(&mut self) {
        let len = self.fields().len();
        if len > 0 {
            let current = self
                .agent_popup
                .as_ref()
                .map(|popup| popup.field_index)
                .unwrap_or(self.field_index);
            let index = if current == 0 { len - 1 } else { current - 1 };
            self.set_field_index(index);
        }
    }

    pub fn submit_on_enter(&self) -> bool {
        self.active_field() == DialogField::Confirm
    }

    pub fn cancel_on_enter(&self) -> bool {
        self.active_field() == DialogField::Cancel
    }

    /// Multiline text fields consume Enter as a newline. Many terminals —
    /// and tmux without `extended-keys` — deliver Shift+Enter as a bare
    /// Enter, so the only way both Shift+Enter and Alt+Enter can break a
    /// line is for Enter itself to do so. Tab still walks to the next field.
    /// Project path/name reuse `Description` as a single line and are excluded.
    pub fn enter_inserts_newline(&self) -> bool {
        matches!(
            (&self.modal, self.active_field()),
            (
                Modal::NewTask { .. } | Modal::EditTask { .. } | Modal::AddMessage { .. },
                DialogField::Description,
            ) | (Modal::AnswerQuestion { .. }, DialogField::Answer)
                | (
                    Modal::Settings,
                    DialogField::MaxRunningPerBackend | DialogField::MaxRunningPerBackendModel
                )
        )
    }

    pub fn agent_slot(field: DialogField) -> Option<AgentSlot> {
        match field {
            DialogField::Backend
            | DialogField::Model
            | DialogField::Effort
            | DialogField::Agent => Some(AgentSlot::Primary),
            DialogField::DesignerBackend
            | DialogField::DesignerModel
            | DialogField::DesignerEffort
            | DialogField::DesignerAgent => Some(AgentSlot::Designer),
            DialogField::ReviewerBackend
            | DialogField::ReviewerModel
            | DialogField::ReviewerEffort
            | DialogField::ReviewerAgent => Some(AgentSlot::Reviewer),
            _ => None,
        }
    }

    pub fn launcher_slot(field: DialogField) -> Option<AgentSlot> {
        match field {
            DialogField::AgentSettings => Some(AgentSlot::Primary),
            DialogField::DesignerAgentSettings => Some(AgentSlot::Designer),
            DialogField::ReviewerAgentSettings => Some(AgentSlot::Reviewer),
            _ => None,
        }
    }

    pub fn agent_popup_slot(&self) -> Option<AgentSlot> {
        self.agent_popup.as_ref().map(|popup| popup.slot)
    }

    pub fn open_agent_settings(&mut self, slot: AgentSlot) {
        if self.agent_popup.is_some() {
            return;
        }
        let original = self.picker_snapshot(slot);
        self.agent_popup = Some(AgentPopupState {
            slot,
            parent_field_index: self.field_index,
            parent_form_scroll: self.form_scroll,
            field_index: 0,
            form_scroll: 0,
            original,
        });
        self.filter_error = None;
    }

    pub fn save_agent_settings(&mut self) {
        let field = self.active_field();
        if let Some(filter) = self.field_filter_mut(field) {
            filter.clear();
        }
        self.filter_error = None;
        self.close_agent_settings(false);
    }

    pub fn cancel_agent_settings(&mut self) {
        self.close_agent_settings(true);
    }

    fn close_agent_settings(&mut self, restore: bool) {
        let Some(popup) = self.agent_popup.take() else {
            return;
        };
        if restore {
            self.restore_picker(popup.slot, popup.original);
        }
        self.field_index = popup.parent_field_index;
        self.form_scroll = popup.parent_form_scroll;
        self.filter_error = None;
    }

    fn picker_snapshot(&self, slot: AgentSlot) -> AgentPicker {
        match slot {
            AgentSlot::Primary => AgentPicker {
                backend: self.backend.clone(),
                model: self.model.clone(),
                effort: self.effort.clone(),
                agent: self.agent.clone(),
                backend_options: self.backend_options.clone(),
                model_options: self.model_options.clone(),
                effort_options: self.effort_options.clone(),
                agent_options: self.agent_options.clone(),
                backend_selected: self.backend_selected,
                model_selected: self.model_selected,
                effort_selected: self.effort_selected,
                agent_selected: self.agent_selected,
                backend_filter: self.backend_filter.clone(),
                model_filter: self.model_filter.clone(),
            },
            AgentSlot::Designer => self.designer.clone(),
            AgentSlot::Reviewer => self.reviewer.clone(),
        }
    }

    fn restore_picker(&mut self, slot: AgentSlot, picker: AgentPicker) {
        match slot {
            AgentSlot::Primary => {
                self.backend = picker.backend;
                self.model = picker.model;
                self.effort = picker.effort;
                self.agent = picker.agent;
                self.backend_options = picker.backend_options;
                self.model_options = picker.model_options;
                self.effort_options = picker.effort_options;
                self.agent_options = picker.agent_options;
                self.backend_selected = picker.backend_selected;
                self.model_selected = picker.model_selected;
                self.effort_selected = picker.effort_selected;
                self.agent_selected = picker.agent_selected;
                self.backend_filter = picker.backend_filter;
                self.model_filter = picker.model_filter;
            }
            AgentSlot::Designer => self.designer = picker,
            AgentSlot::Reviewer => self.reviewer = picker,
        }
    }

    fn picker(&self, slot: AgentSlot) -> Option<&AgentPicker> {
        match slot {
            AgentSlot::Primary => None,
            AgentSlot::Designer => Some(&self.designer),
            AgentSlot::Reviewer => Some(&self.reviewer),
        }
    }

    fn picker_mut(&mut self, slot: AgentSlot) -> Option<&mut AgentPicker> {
        match slot {
            AgentSlot::Primary => None,
            AgentSlot::Designer => Some(&mut self.designer),
            AgentSlot::Reviewer => Some(&mut self.reviewer),
        }
    }

    pub fn designer_backend_selected(&self) -> usize {
        self.designer.backend_selected
    }

    pub fn designer_model_selected(&self) -> usize {
        self.designer.model_selected
    }

    pub fn reviewer_backend_selected(&self) -> usize {
        self.reviewer.backend_selected
    }

    pub fn reviewer_model_selected(&self) -> usize {
        self.reviewer.model_selected
    }

    pub fn backend_text_for(&self, slot: AgentSlot) -> Option<String> {
        match slot {
            AgentSlot::Primary => self.backend_text(),
            AgentSlot::Designer => non_empty(textarea_text(&self.designer.backend)),
            AgentSlot::Reviewer => non_empty(textarea_text(&self.reviewer.backend)),
        }
    }

    pub fn model_text_for(&self, slot: AgentSlot) -> Option<String> {
        match slot {
            AgentSlot::Primary => self.model_text(),
            AgentSlot::Designer => non_empty(textarea_text(&self.designer.model)),
            AgentSlot::Reviewer => non_empty(textarea_text(&self.reviewer.model)),
        }
    }

    pub fn effort_text_for(&self, slot: AgentSlot) -> Option<String> {
        match slot {
            AgentSlot::Primary => self.effort_text(),
            AgentSlot::Designer => non_empty(textarea_text(&self.designer.effort)),
            AgentSlot::Reviewer => non_empty(textarea_text(&self.reviewer.effort)),
        }
    }

    pub fn agent_text_for(&self, slot: AgentSlot) -> Option<String> {
        match slot {
            AgentSlot::Primary => self.agent_text(),
            AgentSlot::Designer => non_empty(textarea_text(&self.designer.agent)),
            AgentSlot::Reviewer => non_empty(textarea_text(&self.reviewer.agent)),
        }
    }

    pub fn set_backend_text_for(&mut self, slot: AgentSlot, value: &str) {
        match slot {
            AgentSlot::Primary => self.backend = one_line(value),
            AgentSlot::Designer => self.designer.backend = one_line(value),
            AgentSlot::Reviewer => self.reviewer.backend = one_line(value),
        }
    }

    pub fn set_model_text_for(&mut self, slot: AgentSlot, value: &str) {
        match slot {
            AgentSlot::Primary => self.model = one_line(value),
            AgentSlot::Designer => self.designer.model = one_line(value),
            AgentSlot::Reviewer => self.reviewer.model = one_line(value),
        }
    }

    pub fn set_effort_text_for(&mut self, slot: AgentSlot, value: &str) {
        match slot {
            AgentSlot::Primary => self.effort = one_line(value),
            AgentSlot::Designer => self.designer.effort = one_line(value),
            AgentSlot::Reviewer => self.reviewer.effort = one_line(value),
        }
    }

    pub fn set_agent_text_for(&mut self, slot: AgentSlot, value: &str) {
        match slot {
            AgentSlot::Primary => self.agent = one_line(value),
            AgentSlot::Designer => self.designer.agent = one_line(value),
            AgentSlot::Reviewer => self.reviewer.agent = one_line(value),
        }
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
        // A validation error may name a field on a hidden tab; surface its
        // tab first so the focus below can actually land on it.
        if matches!(self.modal, Modal::Settings)
            && let Some(tab) = tab_for_field(field)
        {
            self.set_settings_tab(tab);
        }
        if let Some(index) = self
            .fields()
            .iter()
            .position(|candidate| *candidate == field)
        {
            self.set_field_index(index);
        }
    }

    /// Show another settings tab. Field values are never touched — one
    /// [`ModalState`] holds every tab's state and Save writes them all.
    pub fn set_settings_tab(&mut self, tab: SettingsTab) {
        if !matches!(self.modal, Modal::Settings) || tab == self.settings_tab {
            return;
        }
        // Leaving a selector drops its filter, exactly as leaving the field
        // would; the filter must never outlive its visit.
        let leaving = self.active_field();
        if let Some(filter) = self.field_filter_mut(leaving) {
            filter.clear();
        }
        if self.filter_error == Some(leaving) {
            self.filter_error = None;
        }
        self.settings_tab = tab;
        self.field_index = 0;
        self.form_scroll = 0;
        self.ensure_active_field_visible();
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
        if let Some(kind) = selector_kind(field) {
            let len = self.selection_len_for(field, kind);
            if len > 0 {
                // Picking anything clears the "filter matched nothing" state,
                // even when the pick lands on the already-selected option.
                self.filter_error = None;
                let selected = index.min(len - 1);
                if self.selection_value_for(field, kind) == selected {
                    return;
                }
                *self.selection_mut_for(field, kind) = selected;
                self.apply_selection_for(field, kind);
                self.error = None;
            }
        }
    }

    /// Filter text typed into a selector, or `None` when the selector has no
    /// filter row. Effort, agent, theme and the sort selectors are deliberately
    /// left out: their lists are short and fixed, so a filter row would cost a
    /// line of the dialog without ever saving a keystroke.
    pub fn field_filter(&self, field: DialogField) -> Option<&str> {
        match field {
            DialogField::Backend => Some(self.backend_filter.as_str()),
            DialogField::Model => Some(self.model_filter.as_str()),
            DialogField::ChainTo => Some(self.chain_filter.as_str()),
            DialogField::DesignerBackend => Some(self.designer.backend_filter.as_str()),
            DialogField::DesignerModel => Some(self.designer.model_filter.as_str()),
            DialogField::ReviewerBackend => Some(self.reviewer.backend_filter.as_str()),
            DialogField::ReviewerModel => Some(self.reviewer.model_filter.as_str()),
            DialogField::ExecutorMiddle1
            | DialogField::ExecutorMiddle2
            | DialogField::ExecutorMiddle3
            | DialogField::ExecutorCheap1
            | DialogField::ExecutorCheap2
            | DialogField::ExecutorCheap3 => {
                Some(self.executor_filters[executor_slot_index(field)].as_str())
            }
            _ => None,
        }
    }

    fn field_filter_mut(&mut self, field: DialogField) -> Option<&mut String> {
        match field {
            DialogField::Backend => Some(&mut self.backend_filter),
            DialogField::Model => Some(&mut self.model_filter),
            DialogField::ChainTo => Some(&mut self.chain_filter),
            DialogField::DesignerBackend => Some(&mut self.designer.backend_filter),
            DialogField::DesignerModel => Some(&mut self.designer.model_filter),
            DialogField::ReviewerBackend => Some(&mut self.reviewer.backend_filter),
            DialogField::ReviewerModel => Some(&mut self.reviewer.model_filter),
            DialogField::ExecutorMiddle1
            | DialogField::ExecutorMiddle2
            | DialogField::ExecutorMiddle3
            | DialogField::ExecutorCheap1
            | DialogField::ExecutorCheap2
            | DialogField::ExecutorCheap3 => {
                Some(&mut self.executor_filters[executor_slot_index(field)])
            }
            _ => None,
        }
    }

    pub fn options_for(&self, field: DialogField) -> &[SelectOption] {
        match field {
            DialogField::Backend => &self.backend_options,
            DialogField::Model => &self.model_options,
            DialogField::Effort => &self.effort_options,
            DialogField::Agent => &self.agent_options,
            DialogField::ChainTo => &self.chain_options,
            DialogField::TargetStatus => &self.status_options,
            DialogField::Theme => &self.theme_options,
            DialogField::TaskSort => &self.task_sort_options,
            DialogField::ProjectSort => &self.project_sort_options,
            DialogField::DesignerBackend => &self.designer.backend_options,
            DialogField::DesignerModel => &self.designer.model_options,
            DialogField::DesignerEffort => &self.designer.effort_options,
            DialogField::DesignerAgent => &self.designer.agent_options,
            DialogField::ReviewerBackend => &self.reviewer.backend_options,
            DialogField::ReviewerModel => &self.reviewer.model_options,
            DialogField::ReviewerEffort => &self.reviewer.effort_options,
            DialogField::ReviewerAgent => &self.reviewer.agent_options,
            DialogField::ReviewerOnChanges => &self.reviewer_on_changes_options,
            DialogField::ExecutorMiddle1
            | DialogField::ExecutorMiddle2
            | DialogField::ExecutorMiddle3
            | DialogField::ExecutorCheap1
            | DialogField::ExecutorCheap2
            | DialogField::ExecutorCheap3 => &self.executor_slot_options,
            _ => &[],
        }
    }

    /// Option indices the selector currently shows. Identical to `0..len` for
    /// selectors without a filter row.
    pub fn visible_options(&self, field: DialogField) -> Vec<usize> {
        match self.field_filter(field) {
            Some(filter) => filtered_indices(self.options_for(field), filter),
            None => match selector_kind(field) {
                Some(kind) => (0..self.selection_len_for(field, kind)).collect(),
                None => Vec::new(),
            },
        }
    }

    /// Plain Enter inside a form field: commit whatever is focused and report
    /// whether focus should advance. A filtered selector with no matches keeps
    /// focus and paints the section red instead.
    pub fn enter_field(&mut self) -> bool {
        let field = self.active_field();
        let Some(kind) = selector_kind(field) else {
            return true;
        };
        if self.field_filter(field).is_some() {
            let visible = self.visible_options(field);
            let Some(first) = visible.first().copied() else {
                // An empty list is only an error when the filter is what
                // emptied it; a selector with nothing to offer at all just
                // hands focus on rather than trapping it.
                if self.options_for(field).is_empty() {
                    self.filter_error = None;
                    return true;
                }
                self.filter_error = Some(field);
                return false;
            };
            if !visible.contains(&self.selection_value_for(field, kind)) {
                *self.selection_mut_for(field, kind) = first;
                self.apply_selection_for(field, kind);
            }
        }
        self.filter_error = None;
        true
    }

    /// Re-point the selection after the filter text changed. The selection is
    /// only moved when it scrolled out of the filtered list, so narrowing down
    /// to a single match leaves that match selected and ready for Enter.
    fn sync_filtered_selection(&mut self, kind: SelectorKind, field: DialogField) {
        self.filter_error = None;
        let visible = self.visible_options(field);
        let Some(first) = visible.first().copied() else {
            return;
        };
        if visible.contains(&self.selection_value_for(field, kind)) {
            return;
        }
        *self.selection_mut_for(field, kind) = first;
        self.apply_selection_for(field, kind);
    }

    pub fn input(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let before = self.editable_signature();
        let field = self.active_field();
        match field {
            DialogField::Title => input_single_line(&mut self.title, key),
            DialogField::Description => {
                if !super::app::apply_word_edit(&mut self.description, key) {
                    super::app::input_multiline(&mut self.description, key);
                }
            }
            DialogField::Backend => self.input_select(key, SelectorKind::Backend),
            DialogField::Model => self.input_select(key, SelectorKind::Model),
            DialogField::Effort => self.input_select(key, SelectorKind::Effort),
            DialogField::Agent => self.input_select(key, SelectorKind::Agent),
            DialogField::ChainTo => self.input_select(key, SelectorKind::ChainTo),
            DialogField::AgentSettings
            | DialogField::DesignerAgentSettings
            | DialogField::ReviewerAgentSettings => {}
            DialogField::UseOrchestrator => toggle_on_space(&mut self.use_orchestrator, key),
            DialogField::UseDesigner => toggle_on_space(&mut self.use_designer, key),
            DialogField::UseReviewer => toggle_on_space(&mut self.use_reviewer, key),
            DialogField::EscapeToProjects => {
                if key.code == ratatui::crossterm::event::KeyCode::Char(' ') {
                    self.escape_to_projects = !self.escape_to_projects;
                }
            }
            DialogField::UpdateCheckOnOpen => {
                if key.code == ratatui::crossterm::event::KeyCode::Char(' ') {
                    self.update_check_on_open = !self.update_check_on_open;
                }
            }
            DialogField::ProjectSort => self.input_select(key, SelectorKind::ProjectSort),
            DialogField::PurgeData => {
                if key.code == ratatui::crossterm::event::KeyCode::Char(' ') {
                    self.purge_data = !self.purge_data;
                }
            }
            DialogField::TargetStatus => self.input_select(key, SelectorKind::TargetStatus),
            DialogField::MessageKind => self.input_select(key, SelectorKind::MessageKind),
            DialogField::Question => self.input_select(key, SelectorKind::Question),
            DialogField::Variant => self.input_select(key, SelectorKind::Variant),
            DialogField::Answer => super::app::input_multiline(&mut self.answer, key),
            DialogField::Theme => self.input_select(key, SelectorKind::Theme),
            DialogField::TaskSort => self.input_select(key, SelectorKind::TaskSort),
            DialogField::HideKanbanMessages => toggle_on_space(&mut self.hide_kanban_messages, key),
            DialogField::QueueEnabled => toggle_on_space(&mut self.queue_enabled, key),
            DialogField::MaxRunningTotal => input_single_line(&mut self.max_running_total, key),
            DialogField::MaxRunningDesigner => {
                input_single_line(&mut self.max_running_designer, key)
            }
            DialogField::MaxRunningReviewer => {
                input_single_line(&mut self.max_running_reviewer, key)
            }
            DialogField::MaxRunningExecutor => {
                input_single_line(&mut self.max_running_executor, key)
            }
            DialogField::MaxRunningPerBackend => {
                super::app::input_multiline(&mut self.max_running_per_backend, key);
            }
            DialogField::MaxRunningPerBackendModel => {
                self.maybe_prefix_backend_model_cap(&key);
                super::app::input_multiline(&mut self.max_running_per_backend_model, key);
            }
            DialogField::AutoRestartEnabled => toggle_on_space(&mut self.auto_restart_enabled, key),
            DialogField::AutoRestartDelays => input_single_line(&mut self.auto_restart_delays, key),
            DialogField::DesignerEnabled => toggle_on_space(&mut self.designer_enabled, key),
            DialogField::DesignerBackend => self.input_select(key, SelectorKind::Backend),
            DialogField::DesignerModel => self.input_select(key, SelectorKind::Model),
            DialogField::DesignerEffort => self.input_select(key, SelectorKind::Effort),
            DialogField::DesignerAgent => self.input_select(key, SelectorKind::Agent),
            DialogField::ReviewerEnabled => toggle_on_space(&mut self.reviewer_enabled, key),
            DialogField::ReviewerBackend => self.input_select(key, SelectorKind::Backend),
            DialogField::ReviewerModel => self.input_select(key, SelectorKind::Model),
            DialogField::ReviewerEffort => self.input_select(key, SelectorKind::Effort),
            DialogField::ReviewerAgent => self.input_select(key, SelectorKind::Agent),
            DialogField::ReviewerOnChanges => {
                self.input_select(key, SelectorKind::ReviewerOnChanges)
            }
            DialogField::ReviewerMaxRounds => input_single_line(&mut self.reviewer_max_rounds, key),
            DialogField::ExecutorMiddle1
            | DialogField::ExecutorMiddle2
            | DialogField::ExecutorMiddle3
            | DialogField::ExecutorCheap1
            | DialogField::ExecutorCheap2
            | DialogField::ExecutorCheap3 => {
                self.input_select(key, SelectorKind::ExecutorSlot(executor_slot_index(field)));
            }
            DialogField::ExecutorWeekThreshold => {
                input_single_line(&mut self.executor_week_threshold, key)
            }
            DialogField::ExecutorFiveHourThreshold => {
                input_single_line(&mut self.executor_five_hour_threshold, key)
            }
            DialogField::IsolationStatus => {}
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
            DialogField::MaxRunningTotal => {
                self.max_running_total.insert_str(text.replace('\n', " "));
            }
            DialogField::MaxRunningDesigner => {
                self.max_running_designer
                    .insert_str(text.replace('\n', " "));
            }
            DialogField::MaxRunningReviewer => {
                self.max_running_reviewer
                    .insert_str(text.replace('\n', " "));
            }
            DialogField::MaxRunningExecutor => {
                self.max_running_executor
                    .insert_str(text.replace('\n', " "));
            }
            DialogField::MaxRunningPerBackend => {
                self.max_running_per_backend.insert_str(&text);
            }
            DialogField::MaxRunningPerBackendModel => {
                self.prefix_backend_model_cap_if_empty();
                self.max_running_per_backend_model.insert_str(&text);
            }
            DialogField::AutoRestartDelays => {
                self.auto_restart_delays.insert_str(text.replace('\n', " "));
            }
            DialogField::ReviewerMaxRounds => {
                self.reviewer_max_rounds.insert_str(text.replace('\n', " "));
            }
            DialogField::ExecutorWeekThreshold => {
                self.executor_week_threshold
                    .insert_str(text.replace('\n', " "));
            }
            DialogField::ExecutorFiveHourThreshold => {
                self.executor_five_hour_threshold
                    .insert_str(text.replace('\n', " "));
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
            DialogField::AgentSettings
            | DialogField::DesignerAgentSettings
            | DialogField::ReviewerAgentSettings
            | DialogField::UseOrchestrator
            | DialogField::UseDesigner
            | DialogField::UseReviewer
            | DialogField::EscapeToProjects
            | DialogField::UpdateCheckOnOpen => &mut self.answer,
            DialogField::ProjectSort => &mut self.project_sort,
            DialogField::TargetStatus => &mut self.target_status,
            DialogField::MessageKind => &mut self.description,
            DialogField::Question | DialogField::Variant => &mut self.answer,
            DialogField::Answer => &mut self.answer,
            DialogField::Theme => &mut self.theme,
            DialogField::TaskSort => &mut self.task_sort,
            DialogField::HideKanbanMessages => &mut self.answer,
            DialogField::MaxRunningTotal => &mut self.max_running_total,
            DialogField::MaxRunningDesigner => &mut self.max_running_designer,
            DialogField::MaxRunningReviewer => &mut self.max_running_reviewer,
            DialogField::MaxRunningExecutor => &mut self.max_running_executor,
            DialogField::MaxRunningPerBackend => &mut self.max_running_per_backend,
            DialogField::MaxRunningPerBackendModel => &mut self.max_running_per_backend_model,
            DialogField::AutoRestartDelays => &mut self.auto_restart_delays,
            DialogField::DesignerBackend => &mut self.designer.backend,
            DialogField::DesignerModel => &mut self.designer.model,
            DialogField::DesignerEffort => &mut self.designer.effort,
            DialogField::DesignerAgent => &mut self.designer.agent,
            DialogField::ReviewerBackend => &mut self.reviewer.backend,
            DialogField::ReviewerModel => &mut self.reviewer.model,
            DialogField::ReviewerEffort => &mut self.reviewer.effort,
            DialogField::ReviewerAgent => &mut self.reviewer.agent,
            DialogField::ReviewerOnChanges => &mut self.reviewer_on_changes,
            DialogField::ReviewerMaxRounds => &mut self.reviewer_max_rounds,
            DialogField::ExecutorWeekThreshold => &mut self.executor_week_threshold,
            DialogField::ExecutorFiveHourThreshold => &mut self.executor_five_hour_threshold,
            DialogField::ExecutorMiddle1
            | DialogField::ExecutorMiddle2
            | DialogField::ExecutorMiddle3
            | DialogField::ExecutorCheap1
            | DialogField::ExecutorCheap2
            | DialogField::ExecutorCheap3
            | DialogField::Confirm
            | DialogField::Cancel
            | DialogField::PurgeData
            | DialogField::QueueEnabled
            | DialogField::AutoRestartEnabled
            | DialogField::DesignerEnabled
            | DialogField::ReviewerEnabled
            | DialogField::IsolationStatus => &mut self.answer,
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
        // The catalog can warm up while a filter is typed; keep the selection
        // inside whatever the filter now shows.
        self.sync_filtered_selection(SelectorKind::Model, DialogField::Model);
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
        self.sync_filtered_selection(SelectorKind::ChainTo, DialogField::ChainTo);
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

    pub fn set_backend_options_for(&mut self, slot: AgentSlot, options: Vec<SelectOption>) {
        match slot {
            AgentSlot::Primary => self.set_backend_options(options),
            AgentSlot::Designer | AgentSlot::Reviewer => {
                let current = self.backend_text_for(slot);
                let picker = self.picker_mut(slot).expect("role picker");
                picker.backend_options = options;
                picker.backend_selected =
                    select_matching(&picker.backend_options, current.as_deref());
                let field = match slot {
                    AgentSlot::Designer => DialogField::DesignerBackend,
                    AgentSlot::Reviewer => DialogField::ReviewerBackend,
                    AgentSlot::Primary => unreachable!(),
                };
                self.apply_selection_for(field, SelectorKind::Backend);
                self.sync_filtered_selection(SelectorKind::Backend, field);
            }
        }
    }

    pub fn set_model_options_for(&mut self, slot: AgentSlot, options: Vec<SelectOption>) {
        match slot {
            AgentSlot::Primary => self.set_model_options(options),
            AgentSlot::Designer | AgentSlot::Reviewer => {
                let current = self.model_text_for(slot);
                let picker = self.picker_mut(slot).expect("role picker");
                picker.model_options = options;
                picker.model_selected = select_matching(&picker.model_options, current.as_deref());
                let field = match slot {
                    AgentSlot::Designer => DialogField::DesignerModel,
                    AgentSlot::Reviewer => DialogField::ReviewerModel,
                    AgentSlot::Primary => unreachable!(),
                };
                self.apply_selection_for(field, SelectorKind::Model);
                self.sync_filtered_selection(SelectorKind::Model, field);
            }
        }
    }

    pub fn set_effort_options_for(&mut self, slot: AgentSlot, options: Vec<SelectOption>) {
        match slot {
            AgentSlot::Primary => self.set_effort_options(options),
            AgentSlot::Designer | AgentSlot::Reviewer => {
                let current = self.effort_text_for(slot);
                let picker = self.picker_mut(slot).expect("role picker");
                picker.effort_options = options;
                picker.effort_selected =
                    select_matching(&picker.effort_options, current.as_deref());
                let field = match slot {
                    AgentSlot::Designer => DialogField::DesignerEffort,
                    AgentSlot::Reviewer => DialogField::ReviewerEffort,
                    AgentSlot::Primary => unreachable!(),
                };
                self.apply_selection_for(field, SelectorKind::Effort);
            }
        }
    }

    pub fn set_agent_options_for(&mut self, slot: AgentSlot, options: Vec<SelectOption>) {
        match slot {
            AgentSlot::Primary => self.set_agent_options(options),
            AgentSlot::Designer | AgentSlot::Reviewer => {
                let current = self.agent_text_for(slot);
                let picker = self.picker_mut(slot).expect("role picker");
                picker.agent_options = options;
                picker.agent_selected = select_matching(&picker.agent_options, current.as_deref());
                let field = match slot {
                    AgentSlot::Designer => DialogField::DesignerAgent,
                    AgentSlot::Reviewer => DialogField::ReviewerAgent,
                    AgentSlot::Primary => unreachable!(),
                };
                self.apply_selection_for(field, SelectorKind::Agent);
            }
        }
    }

    pub fn set_reviewer_on_changes_options(&mut self, options: Vec<SelectOption>) {
        self.reviewer_on_changes_options = options;
        self.reviewer_on_changes_selected = select_matching(
            &self.reviewer_on_changes_options,
            non_empty(textarea_text(&self.reviewer_on_changes)).as_deref(),
        );
        self.apply_selection_for(
            DialogField::ReviewerOnChanges,
            SelectorKind::ReviewerOnChanges,
        );
    }

    pub fn reviewer_on_changes_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.reviewer_on_changes))
    }

    /// The `<backend>/<model>` a pool slot currently points at, or `None`
    /// for the leading `— none —` option.
    pub fn executor_slot_value(&self, index: usize) -> Option<String> {
        let selected = self.executor_selected.get(index).copied().unwrap_or(0);
        self.executor_slot_options
            .get(selected)
            .and_then(|option| option.value.clone())
    }

    pub fn set_executor_slot_options(&mut self, options: Vec<SelectOption>) {
        let len = options.len();
        self.executor_slot_options = options;
        for selected in &mut self.executor_selected {
            *selected = (*selected).min(len.saturating_sub(1));
        }
    }

    pub fn executor_week_threshold_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.executor_week_threshold))
    }

    pub fn executor_five_hour_threshold_text(&self) -> Option<String> {
        non_empty(textarea_text(&self.executor_five_hour_threshold))
    }

    fn maybe_prefix_backend_model_cap(&mut self, key: &ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let KeyCode::Char(_) = key.code else {
            return;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            return;
        }
        self.prefix_backend_model_cap_if_empty();
    }

    fn prefix_backend_model_cap_if_empty(&mut self) {
        if !textarea_text(&self.max_running_per_backend_model).is_empty() {
            return;
        }
        let Some(backend) = self.backend_text() else {
            return;
        };
        self.max_running_per_backend_model = TextArea::new(vec![format!("{backend}/")]);
        // Place the cursor after the prefix so the next keystroke appends the model id.
        self.max_running_per_backend_model
            .move_cursor(ratatui_textarea::CursorMove::End);
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
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let field = self.active_field();
        match key.code {
            KeyCode::Up | KeyCode::Left => self.move_selection(kind, -1),
            KeyCode::Down | KeyCode::Right => self.move_selection(kind, 1),
            KeyCode::Char(character)
                if self.field_filter(field).is_some()
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(filter) = self.field_filter_mut(field) {
                    filter.push(character);
                }
                self.sync_filtered_selection(kind, field);
            }
            KeyCode::Backspace if self.field_filter(field).is_some() => {
                if let Some(filter) = self.field_filter_mut(field) {
                    filter.pop();
                }
                self.sync_filtered_selection(kind, field);
            }
            KeyCode::Delete if self.field_filter(field).is_some() => {
                if let Some(filter) = self.field_filter_mut(field) {
                    filter.clear();
                }
                self.sync_filtered_selection(kind, field);
            }
            _ => {}
        }
    }

    /// Move within the options the selector currently shows, so a filtered
    /// list never steps onto a hidden entry.
    fn move_selection(&mut self, kind: SelectorKind, delta: isize) {
        let field = self.active_field();
        let visible = self.visible_options(field);
        if visible.is_empty() {
            return;
        }
        let selected = self.selection_value(kind);
        let position = visible.iter().position(|index| *index == selected);
        let next = match position {
            Some(position) => {
                let moved = if delta.is_negative() {
                    position.saturating_sub(delta.unsigned_abs())
                } else {
                    position
                        .saturating_add(delta as usize)
                        .min(visible.len() - 1)
                };
                visible[moved]
            }
            // The selection scrolled out of the filtered list; step back onto it.
            None => visible[0],
        };
        if next == selected {
            return;
        }
        *self.selection_mut(kind) = next;
        self.apply_selection(kind);
        self.filter_error = None;
    }

    fn selection_len_for(&self, field: DialogField, kind: SelectorKind) -> usize {
        if let Some(slot) = Self::agent_slot(field)
            && let Some(picker) = self.picker(slot)
        {
            return match kind {
                SelectorKind::Backend => picker.backend_options.len(),
                SelectorKind::Model => picker.model_options.len(),
                SelectorKind::Effort => picker.effort_options.len(),
                SelectorKind::Agent => picker.agent_options.len(),
                _ => 0,
            };
        }
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
            SelectorKind::ReviewerOnChanges => self.reviewer_on_changes_options.len(),
            SelectorKind::ExecutorSlot(_) => self.executor_slot_options.len(),
        }
    }

    fn selection_mut(&mut self, kind: SelectorKind) -> &mut usize {
        self.selection_mut_for(self.active_field(), kind)
    }

    fn selection_mut_for(&mut self, field: DialogField, kind: SelectorKind) -> &mut usize {
        if let Some(slot) = Self::agent_slot(field)
            && slot != AgentSlot::Primary
        {
            let picker = self.picker_mut(slot).expect("role picker");
            return match kind {
                SelectorKind::Backend => &mut picker.backend_selected,
                SelectorKind::Model => &mut picker.model_selected,
                SelectorKind::Effort => &mut picker.effort_selected,
                SelectorKind::Agent => &mut picker.agent_selected,
                _ => &mut picker.backend_selected,
            };
        }
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
            SelectorKind::ReviewerOnChanges => &mut self.reviewer_on_changes_selected,
            SelectorKind::ExecutorSlot(index) => &mut self.executor_selected[index],
        }
    }

    fn selection_value(&self, kind: SelectorKind) -> usize {
        self.selection_value_for(self.active_field(), kind)
    }

    fn selection_value_for(&self, field: DialogField, kind: SelectorKind) -> usize {
        if let Some(slot) = Self::agent_slot(field)
            && let Some(picker) = self.picker(slot)
        {
            return match kind {
                SelectorKind::Backend => picker.backend_selected,
                SelectorKind::Model => picker.model_selected,
                SelectorKind::Effort => picker.effort_selected,
                SelectorKind::Agent => picker.agent_selected,
                _ => 0,
            };
        }
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
            SelectorKind::ReviewerOnChanges => self.reviewer_on_changes_selected,
            SelectorKind::ExecutorSlot(index) => self.executor_selected[index],
        }
    }

    fn apply_selection(&mut self, kind: SelectorKind) {
        self.apply_selection_for(self.active_field(), kind);
    }

    fn apply_selection_for(&mut self, field: DialogField, kind: SelectorKind) {
        if let Some(slot) = Self::agent_slot(field)
            && slot != AgentSlot::Primary
        {
            let picker = self.picker_mut(slot).expect("role picker");
            match kind {
                SelectorKind::Backend => {
                    let text = selected_value(&picker.backend_options, picker.backend_selected);
                    picker.backend = one_line(text.as_deref().unwrap_or(""));
                }
                SelectorKind::Model => {
                    let text = selected_value(&picker.model_options, picker.model_selected);
                    picker.model = one_line(text.as_deref().unwrap_or(""));
                }
                SelectorKind::Effort => {
                    let text = selected_value(&picker.effort_options, picker.effort_selected);
                    picker.effort = one_line(text.as_deref().unwrap_or(""));
                }
                SelectorKind::Agent => {
                    let text = selected_value(&picker.agent_options, picker.agent_selected);
                    picker.agent = one_line(text.as_deref().unwrap_or(""));
                }
                _ => {}
            }
            return;
        }
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
            SelectorKind::ReviewerOnChanges => {
                let text = selected_value(
                    &self.reviewer_on_changes_options,
                    self.reviewer_on_changes_selected,
                );
                self.reviewer_on_changes = one_line(text.as_deref().unwrap_or("in_progress"));
            }
            // The slot selection is the value; nothing to sync.
            SelectorKind::ExecutorSlot(_) => {}
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
        if let Some(popup) = self.agent_popup.as_mut() {
            popup.form_scroll = popup.field_index.min(3);
            return;
        }
        let fields = match self.modal {
            Modal::NewTask { .. } | Modal::EditTask { .. } => Some(&TASK_FORM_FIELDS[..]),
            Modal::Settings => Some(settings_page_fields(self.settings_tab)),
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
            self.use_orchestrator.to_string(),
            self.use_designer.to_string(),
            self.use_reviewer.to_string(),
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
            self.hide_kanban_messages.to_string(),
            self.escape_to_projects.to_string(),
            self.update_check_on_open.to_string(),
            raw_textarea_text(&self.project_sort),
            self.project_sort_selected.to_string(),
            self.purge_data.to_string(),
            self.queue_enabled.to_string(),
            raw_textarea_text(&self.max_running_total),
            raw_textarea_text(&self.max_running_designer),
            raw_textarea_text(&self.max_running_reviewer),
            raw_textarea_text(&self.max_running_executor),
            raw_textarea_text(&self.max_running_per_backend),
            raw_textarea_text(&self.max_running_per_backend_model),
            self.auto_restart_enabled.to_string(),
            raw_textarea_text(&self.auto_restart_delays),
            self.designer_enabled.to_string(),
            raw_textarea_text(&self.designer.backend),
            raw_textarea_text(&self.designer.model),
            raw_textarea_text(&self.designer.effort),
            raw_textarea_text(&self.designer.agent),
            self.designer.backend_selected.to_string(),
            self.designer.model_selected.to_string(),
            self.designer.effort_selected.to_string(),
            self.designer.agent_selected.to_string(),
            self.reviewer_enabled.to_string(),
            raw_textarea_text(&self.reviewer.backend),
            raw_textarea_text(&self.reviewer.model),
            raw_textarea_text(&self.reviewer.effort),
            raw_textarea_text(&self.reviewer.agent),
            self.reviewer.backend_selected.to_string(),
            self.reviewer.model_selected.to_string(),
            self.reviewer.effort_selected.to_string(),
            self.reviewer.agent_selected.to_string(),
            raw_textarea_text(&self.reviewer_on_changes),
            self.reviewer_on_changes_selected.to_string(),
            raw_textarea_text(&self.reviewer_max_rounds),
            self.executor_selected
                .iter()
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join("\u{1f}"),
            self.executor_filters.join("\u{1f}"),
            raw_textarea_text(&self.executor_week_threshold),
            raw_textarea_text(&self.executor_five_hour_threshold),
        ]
        .join("\u{1f}")
    }
}

/// Which executor-pool slot (0-5) a settings field edits. 0-2 are the
/// middle "smart" pool, 3-5 the cheap working pool; index order is priority.
pub(crate) fn executor_slot_index(field: DialogField) -> usize {
    match field {
        DialogField::ExecutorMiddle1 => 0,
        DialogField::ExecutorMiddle2 => 1,
        DialogField::ExecutorMiddle3 => 2,
        DialogField::ExecutorCheap1 => 3,
        DialogField::ExecutorCheap2 => 4,
        DialogField::ExecutorCheap3 => 5,
        _ => 0,
    }
}

fn agent_fields(slot: AgentSlot) -> &'static [DialogField] {
    match slot {
        AgentSlot::Primary => &PRIMARY_AGENT_FIELDS,
        AgentSlot::Designer => &DESIGNER_AGENT_FIELDS,
        AgentSlot::Reviewer => &REVIEWER_AGENT_FIELDS,
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
    ReviewerOnChanges,
    ExecutorSlot(usize),
}

fn selector_kind(field: DialogField) -> Option<SelectorKind> {
    match field {
        DialogField::Backend | DialogField::DesignerBackend | DialogField::ReviewerBackend => {
            Some(SelectorKind::Backend)
        }
        DialogField::Model | DialogField::DesignerModel | DialogField::ReviewerModel => {
            Some(SelectorKind::Model)
        }
        DialogField::Effort | DialogField::DesignerEffort | DialogField::ReviewerEffort => {
            Some(SelectorKind::Effort)
        }
        DialogField::Agent | DialogField::DesignerAgent | DialogField::ReviewerAgent => {
            Some(SelectorKind::Agent)
        }
        DialogField::ChainTo => Some(SelectorKind::ChainTo),
        DialogField::ReviewerOnChanges => Some(SelectorKind::ReviewerOnChanges),
        DialogField::ExecutorMiddle1
        | DialogField::ExecutorMiddle2
        | DialogField::ExecutorMiddle3
        | DialogField::ExecutorCheap1
        | DialogField::ExecutorCheap2
        | DialogField::ExecutorCheap3 => {
            Some(SelectorKind::ExecutorSlot(executor_slot_index(field)))
        }
        DialogField::TargetStatus => Some(SelectorKind::TargetStatus),
        DialogField::MessageKind => Some(SelectorKind::MessageKind),
        DialogField::Question => Some(SelectorKind::Question),
        DialogField::Variant => Some(SelectorKind::Variant),
        DialogField::Theme => Some(SelectorKind::Theme),
        DialogField::TaskSort => Some(SelectorKind::TaskSort),
        DialogField::ProjectSort => Some(SelectorKind::ProjectSort),
        _ => None,
    }
}

/// Option indices whose label contains `filter`, case-insensitively. An empty
/// filter keeps every option, including the leading "Default …" entry.
pub(super) fn filtered_indices(options: &[SelectOption], filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    options
        .iter()
        .enumerate()
        .filter(|(_, option)| {
            needle.is_empty() || option.label.to_lowercase().contains(needle.as_str())
        })
        .map(|(index, _)| index)
        .collect()
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
    if modal.agent_popup.is_some() {
        hitboxes.clear();
        render_agent_popup(frame, app, modal, area, &mut hitboxes);
    }
    hitboxes
}

fn render_agent_popup(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    parent_area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    let Some(popup) = modal.agent_popup.as_ref() else {
        return;
    };
    let slot = popup.slot;
    let scroll = popup.form_scroll;
    let area = centered_percent(88, 90, parent_area);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" {} agent settings ", agent_slot_name(slot)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.focus))
        .style(Style::default().bg(app.theme.bg).fg(app.theme.fg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let button_height = 4;
    let content_height = inner.height.saturating_sub(button_height);
    let content = Rect {
        height: content_height,
        ..inner
    };
    let button_area = Rect {
        y: inner.y.saturating_add(content_height),
        height: button_height.min(inner.height.saturating_sub(content_height)),
        ..inner
    };
    let fields = &agent_fields(slot)[..4];
    let rows = selector_form_rows_from_scroll(modal, content.height, fields, scroll);
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

fn agent_slot_name(slot: AgentSlot) -> &'static str {
    match slot {
        AgentSlot::Primary => "Primary",
        AgentSlot::Designer => "Designer",
        AgentSlot::Reviewer => "Reviewer",
    }
}

fn centered_percent(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height) / 2),
            Constraint::Percentage(height),
            Constraint::Percentage((100 - height) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width) / 2),
            Constraint::Percentage(width),
            Constraint::Percentage((100 - width) / 2),
        ])
        .split(vertical[1])[1]
}

fn render_settings_form(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    let tab = modal.settings_tab;
    let tab_strip_height = 2u16;
    // The Executor tab opens with the resolved order the board would run
    // right now, so the priority slots read as live data, not labels.
    let status_line = match tab {
        SettingsTab::Executor => app.executor_pool_status_line(),
        _ => String::new(),
    };
    let strip_height = if status_line.is_empty() {
        tab_strip_height
    } else {
        tab_strip_height + 1
    };
    let form_area = if area.height > strip_height {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(strip_height), Constraint::Min(0)])
            .split(area);
        render_settings_tab_strip(frame, app, modal, rows[0], hitboxes);
        if !status_line.is_empty() {
            frame.render_widget(
                Paragraph::new(sanitize_terminal_text(&status_line))
                    .style(Style::default().fg(app.theme.muted)),
                Rect {
                    y: rows[0].y.saturating_add(tab_strip_height),
                    height: 1,
                    ..rows[0]
                },
            );
        }
        rows[1]
    } else {
        area
    };
    render_selector_form(frame, app, modal, form_area, hitboxes, settings_fields(tab));
}

/// The ` Common │ Designer │ Reviewer │ Executor ` header. Labels degrade to
/// short forms and then to the active label alone so a narrow terminal never
/// overflows. Each label registers a [`HitAction::ModalTab`] hitbox over
/// exactly its own cells.
fn render_settings_tab_strip(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let active = modal.settings_tab;
    let fit = |short: bool| -> Option<u16> {
        let width: usize = SettingsTab::ALL
            .iter()
            .map(|tab| {
                let label = if short {
                    tab.short_label()
                } else {
                    tab.label()
                };
                label.chars().count() + 2 // one leading and trailing space
            })
            .sum::<usize>()
            + SettingsTab::ALL.len()
            - 1; // dividers between labels
        u16::try_from(width)
            .ok()
            .filter(|width| *width <= area.width)
    };
    let (short, show_all) = match (fit(false), fit(true)) {
        (Some(_), _) => (false, true),
        (None, Some(_)) => (true, true),
        (None, None) => (true, false),
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut x = area.x;
    let mut widths: Vec<(SettingsTab, u16)> = Vec::new();
    for (index, tab) in SettingsTab::ALL.into_iter().enumerate() {
        let label = if short {
            tab.short_label()
        } else {
            tab.label()
        };
        if show_all && index > 0 {
            spans.push(Span::raw("│".to_string()));
            x = x.saturating_add(1);
        }
        if !show_all && tab != active {
            continue;
        }
        let highlighted = tab == active || app.is_hovered(HitAction::ModalTab(tab));
        let style = if highlighted {
            Style::default()
                .fg(app.theme.focus)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.theme.muted)
        };
        let cell = format!(" {label} ");
        let cell_width = cell.chars().count() as u16;
        widths.push((tab, cell_width));
        spans.push(Span::styled(cell, style));
        x = x.saturating_add(cell_width);
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect { height: 1, ..area },
    );
    let mut cell_x = area.x;
    for (tab, width) in widths {
        hitboxes.push(Hitbox {
            area: Rect {
                x: cell_x,
                y: area.y,
                width,
                height: 1,
            },
            action: HitAction::ModalTab(tab),
        });
        cell_x = cell_x.saturating_add(width + 1); // + divider
    }
    // The rule under the labels is what makes the strip read as tabs.
    frame.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(app.theme.border)),
        Rect {
            y: area.y.saturating_add(1),
            height: 1,
            ..area
        },
    );
}

fn render_global_settings_form(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &mut ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    // The Updates section (status + action buttons) sits above the standard
    // form; update state is machine-wide, so it lives only in this dialog.
    let updates_height = 4u16;
    let (updates_area, form_area) = if area.height > updates_height {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(updates_height), Constraint::Min(0)])
            .split(area);
        (rows[0], rows[1])
    } else {
        (Rect::default(), area)
    };
    render_updates_section(frame, app, modal, updates_area, hitboxes);
    render_selector_form(
        frame,
        app,
        modal,
        form_area,
        hitboxes,
        &GLOBAL_SETTINGS_FORM_FIELDS,
    );
}

fn render_updates_section(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let block = Block::default()
        .title(" Updates ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(update_status_line(modal)),
        Rect { height: 1, ..inner },
    );
    let action_area = Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let update_available = modal.update_check_error.is_none()
        && update::cached()
            .as_deref()
            .map(update::is_update_available)
            .unwrap_or(false);
    if let (Some(command), true) = (&modal.update_upgrade_command, update_available) {
        frame.render_widget(
            Paragraph::new(format!("update with: {command}"))
                .style(Style::default().fg(app.theme.muted)),
            action_area,
        );
        return;
    }
    let check_w = "[ Check now ]".len() as u16;
    let update_w = "[ Update now ]".len() as u16;
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(check_w),
            Constraint::Length(if update_available {
                update_w.saturating_add(2)
            } else {
                0
            }),
            Constraint::Min(0),
        ])
        .split(action_area);
    let check_active = app.is_hovered(HitAction::Action(UiAction::CheckUpdates));
    frame.render_widget(
        Paragraph::new("[ Check now ]").style(button_style(app, check_active)),
        columns[0],
    );
    hitboxes.push(Hitbox {
        area: columns[0],
        action: HitAction::Action(UiAction::CheckUpdates),
    });
    if update_available {
        let update_active = app.is_hovered(HitAction::Action(UiAction::ApplyUpdate));
        frame.render_widget(
            Paragraph::new("[ Update now ]").style(button_style(app, update_active)),
            columns[1],
        );
        hitboxes.push(Hitbox {
            area: columns[1],
            action: HitAction::Action(UiAction::ApplyUpdate),
        });
    }
}

/// The read-only status line on the Updates row, backed by
/// `update::cached()`: up to date, available (with release age), the failure
/// reason of a failed "Check now", or "never checked".
fn update_status_line(modal: &ModalState) -> String {
    if let Some(reason) = &modal.update_check_error {
        return format!("Check failed: {reason}");
    }
    match update::cached() {
        None => format!(
            "kanban4ai {} - no update checked yet",
            update::installed_version()
        ),
        Some(status) => {
            if update::is_update_available(&status) {
                format!(
                    "kanban4ai {} available{}",
                    status.latest_version,
                    released_suffix(&status)
                )
            } else {
                format!("kanban4ai {} - up to date", update::installed_version())
            }
        }
    }
}

fn released_suffix(status: &update::UpdateStatus) -> String {
    match status.published_at {
        Some(published_at) => {
            let age = chrono::Utc::now()
                .timestamp()
                .saturating_sub(published_at)
                .max(0);
            format!(" (released {} ago)", crate::core::limits::format_span(age))
        }
        None => String::new(),
    }
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
    selector_form_rows_from_scroll(modal, content_height, fields, modal.form_scroll)
}

fn selector_form_rows_from_scroll(
    modal: &ModalState,
    content_height: u16,
    fields: &[DialogField],
    scroll: usize,
) -> Vec<(DialogField, u16)> {
    let mut rows = Vec::new();
    let mut used: u16 = 0;
    for field in fields.iter().copied().skip(scroll.min(fields.len() - 1)) {
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
        let growth = (15 - *height).min(surplus);
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
        DialogField::Title
        | DialogField::AgentSettings
        | DialogField::DesignerAgentSettings
        | DialogField::ReviewerAgentSettings
        | DialogField::UseOrchestrator
        | DialogField::UseDesigner
        | DialogField::UseReviewer
        | DialogField::EscapeToProjects
        | DialogField::UpdateCheckOnOpen
        | DialogField::QueueEnabled
        | DialogField::HideKanbanMessages
        | DialogField::AutoRestartEnabled
        | DialogField::DesignerEnabled
        | DialogField::ReviewerEnabled
        | DialogField::MaxRunningTotal
        | DialogField::MaxRunningDesigner
        | DialogField::MaxRunningReviewer
        | DialogField::MaxRunningExecutor
        | DialogField::AutoRestartDelays
        | DialogField::ReviewerMaxRounds
        | DialogField::ExecutorWeekThreshold
        | DialogField::ExecutorFiveHourThreshold
        | DialogField::IsolationStatus => 3,
        DialogField::Description => 5,
        DialogField::MaxRunningPerBackend | DialogField::MaxRunningPerBackendModel => 5,
        // The chain selector always shows its filter and the "No chain"
        // entry, so two content rows already cover an empty board.
        DialogField::ChainTo => 4,
        // Filterable selectors spend a row on the filter input, so they need
        // one more line to still show two options.
        DialogField::Backend
        | DialogField::Model
        | DialogField::DesignerBackend
        | DialogField::DesignerModel
        | DialogField::ReviewerBackend
        | DialogField::ReviewerModel
        | DialogField::ExecutorMiddle1
        | DialogField::ExecutorMiddle2
        | DialogField::ExecutorMiddle3
        | DialogField::ExecutorCheap1
        | DialogField::ExecutorCheap2
        | DialogField::ExecutorCheap3 => 5,
        _ => 4,
    }
}

fn task_selector_max_height(modal: &ModalState, field: DialogField) -> u16 {
    if field == DialogField::Description {
        return 15;
    }
    if matches!(
        field,
        DialogField::MaxRunningPerBackend | DialogField::MaxRunningPerBackendModel
    ) {
        return 10;
    }
    if field == DialogField::ChainTo {
        // Filter + borders + one row per candidate, capped so a long chain
        // list never crowds out the description; the filter finds tasks by
        // number instead.
        return modal.chain_options.len().saturating_add(3).clamp(4, 8) as u16;
    }
    let chrome = if modal.field_filter(field).is_some() {
        3
    } else {
        2
    };
    let count = match field {
        DialogField::Backend => modal.backend_options.len(),
        DialogField::Model => modal.model_options.len(),
        DialogField::Effort => modal.effort_options.len(),
        DialogField::Agent => modal.agent_options.len(),
        DialogField::Theme => modal.theme_options.len(),
        DialogField::TaskSort => modal.task_sort_options.len(),
        DialogField::ProjectSort => modal.project_sort_options.len(),
        DialogField::ChainTo => modal.chain_options.len(),
        DialogField::DesignerBackend => modal.designer.backend_options.len(),
        DialogField::DesignerModel => modal.designer.model_options.len(),
        DialogField::DesignerEffort => modal.designer.effort_options.len(),
        DialogField::DesignerAgent => modal.designer.agent_options.len(),
        DialogField::ExecutorMiddle1
        | DialogField::ExecutorMiddle2
        | DialogField::ExecutorMiddle3
        | DialogField::ExecutorCheap1
        | DialogField::ExecutorCheap2
        | DialogField::ExecutorCheap3 => modal.executor_slot_options.len(),
        DialogField::ReviewerModel => modal.reviewer.model_options.len(),
        DialogField::ReviewerEffort => modal.reviewer.effort_options.len(),
        DialogField::ReviewerAgent => modal.reviewer.agent_options.len(),
        DialogField::ReviewerOnChanges => modal.reviewer_on_changes_options.len(),
        _ => return task_field_min_height(field),
    };
    task_field_min_height(field)
        .max((count.saturating_add(chrome as usize)).min(u16::MAX as usize) as u16)
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
        DialogField::AgentSettings => render_agent_launcher(
            frame,
            app,
            modal,
            AgentSlot::Primary,
            area,
            "Agent settings",
        ),
        DialogField::DesignerAgentSettings => render_agent_launcher(
            frame,
            app,
            modal,
            AgentSlot::Designer,
            area,
            "Designer agent settings",
        ),
        DialogField::ReviewerAgentSettings => render_agent_launcher(
            frame,
            app,
            modal,
            AgentSlot::Reviewer,
            area,
            "Reviewer agent settings",
        ),
        DialogField::Backend => render_select_filtered(
            frame,
            app,
            "Backend",
            &modal.backend_options,
            modal.backend_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
            Some(&modal.backend_filter),
            modal.filter_error == Some(field),
        ),
        DialogField::Model => render_select_filtered(
            frame,
            app,
            "Model",
            &modal.model_options,
            modal.model_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
            Some(&modal.model_filter),
            modal.filter_error == Some(field),
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
        DialogField::ChainTo => render_select_filtered(
            frame,
            app,
            "Chain to",
            &modal.chain_options,
            modal.chain_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
            Some(&modal.chain_filter),
            modal.filter_error == Some(field),
        ),
        DialogField::UseOrchestrator => render_checkbox(
            frame,
            app,
            area,
            "Orchestrator",
            "plan a subtask graph first",
            modal.use_orchestrator,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::UseDesigner => render_checkbox(
            frame,
            app,
            area,
            "Designer",
            "designer for this task",
            modal.use_designer,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::UseReviewer => render_checkbox(
            frame,
            app,
            area,
            "Reviewer",
            "reviewer for this task",
            modal.use_reviewer,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::EscapeToProjects => render_escape_to_projects(frame, app, modal, area),
        DialogField::UpdateCheckOnOpen => render_checkbox(
            frame,
            app,
            area,
            "Updates",
            "check for updates when kanban4ai opens",
            modal.update_check_on_open,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
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
        DialogField::HideKanbanMessages => render_checkbox(
            frame,
            app,
            area,
            "Thread",
            "hide messages by kanban",
            modal.hide_kanban_messages,
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
        DialogField::QueueEnabled => render_checkbox(
            frame,
            app,
            area,
            "Limits",
            "queue enabled (dispatcher starts queued tasks)",
            modal.queue_enabled,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::MaxRunningTotal => render_textarea(
            frame,
            app,
            &modal.max_running_total,
            area,
            "Limits · Max running total (0 = unlimited)",
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::MaxRunningDesigner => render_textarea(
            frame,
            app,
            &modal.max_running_designer,
            area,
            "Limits · Max designer tasks",
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::MaxRunningReviewer => render_textarea(
            frame,
            app,
            &modal.max_running_reviewer,
            area,
            "Limits · Max reviewer tasks",
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::MaxRunningExecutor => render_textarea(
            frame,
            app,
            &modal.max_running_executor,
            area,
            "Limits · Max executor tasks",
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::MaxRunningPerBackend => render_textarea(
            frame,
            app,
            &modal.max_running_per_backend,
            area,
            "Limits · Max tasks per backend (one `backend: N` line)",
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::MaxRunningPerBackendModel => {
            let hint = modal
                .backend_text()
                .map(|backend| {
                    format!("Limits · Max tasks per backend/model (`{backend}/model: N`)")
                })
                .unwrap_or_else(|| {
                    "Limits · Max tasks per backend/model (`claude/opus: N`)".to_string()
                });
            render_textarea(
                frame,
                app,
                &modal.max_running_per_backend_model,
                area,
                &hint,
                modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
            );
        }
        DialogField::AutoRestartEnabled => render_checkbox(
            frame,
            app,
            area,
            "Restarts",
            "auto-restart crashed sessions",
            modal.auto_restart_enabled,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::AutoRestartDelays => render_textarea(
            frame,
            app,
            &modal.auto_restart_delays,
            area,
            "Restarts · Delay minutes (e.g. 1, 30, 270)",
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::DesignerEnabled => render_checkbox(
            frame,
            app,
            area,
            "Designer",
            "run a designer bot before the executor",
            modal.designer_enabled,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::DesignerBackend => render_select_filtered(
            frame,
            app,
            "Designer · Backend",
            &modal.designer.backend_options,
            modal.designer.backend_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
            Some(&modal.designer.backend_filter),
            modal.filter_error == Some(field),
        ),
        DialogField::DesignerModel => render_select_filtered(
            frame,
            app,
            "Designer · Model",
            &modal.designer.model_options,
            modal.designer.model_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
            Some(&modal.designer.model_filter),
            modal.filter_error == Some(field),
        ),
        DialogField::DesignerEffort => render_select(
            frame,
            app,
            "Designer · Effort",
            &modal.designer.effort_options,
            modal.designer.effort_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::DesignerAgent => render_select(
            frame,
            app,
            "Designer · Agent",
            &modal.designer.agent_options,
            modal.designer.agent_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::ReviewerEnabled => render_checkbox(
            frame,
            app,
            area,
            "Reviewer",
            "run a reviewer bot before human Review",
            modal.reviewer_enabled,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::ReviewerBackend => render_select_filtered(
            frame,
            app,
            "Reviewer · Backend",
            &modal.reviewer.backend_options,
            modal.reviewer.backend_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
            Some(&modal.reviewer.backend_filter),
            modal.filter_error == Some(field),
        ),
        DialogField::ReviewerModel => render_select_filtered(
            frame,
            app,
            "Reviewer · Model",
            &modal.reviewer.model_options,
            modal.reviewer.model_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
            Some(&modal.reviewer.model_filter),
            modal.filter_error == Some(field),
        ),
        DialogField::ReviewerEffort => render_select(
            frame,
            app,
            "Reviewer · Effort",
            &modal.reviewer.effort_options,
            modal.reviewer.effort_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::ReviewerAgent => render_select(
            frame,
            app,
            "Reviewer · Agent",
            &modal.reviewer.agent_options,
            modal.reviewer.agent_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::ReviewerOnChanges => render_select(
            frame,
            app,
            "Reviewer · On changes requested",
            &modal.reviewer_on_changes_options,
            modal.reviewer_on_changes_selected,
            area,
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::ReviewerMaxRounds => render_textarea(
            frame,
            app,
            &modal.reviewer_max_rounds,
            area,
            "Reviewer · Max bounce rounds (0 = unlimited)",
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::IsolationStatus => render_isolation_status(frame, app, modal, area),
        DialogField::ExecutorMiddle1
        | DialogField::ExecutorMiddle2
        | DialogField::ExecutorMiddle3
        | DialogField::ExecutorCheap1
        | DialogField::ExecutorCheap2
        | DialogField::ExecutorCheap3 => {
            let slot = executor_slot_index(field);
            let pool = if slot < 3 { "Middle" } else { "Cheap" };
            let position = (slot % 3) + 1;
            let filter = &modal.executor_filters[slot];
            let selected = modal.executor_selected[slot];
            render_select_filtered(
                frame,
                app,
                &format!("Executor · {pool} {position}"),
                &modal.executor_slot_options,
                selected,
                area,
                modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
                Some(filter),
                modal.filter_error == Some(field),
            );
        }
        DialogField::ExecutorWeekThreshold => render_textarea(
            frame,
            app,
            &modal.executor_week_threshold,
            area,
            "Executor · Week quota floor % (out of quota below)",
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        DialogField::ExecutorFiveHourThreshold => render_textarea(
            frame,
            app,
            &modal.executor_five_hour_threshold,
            area,
            "Executor · 5h quota floor % (out of quota below)",
            modal.active_field() == field || app.is_hovered(HitAction::ModalField(field)),
        ),
        _ => {}
    }
}

fn render_agent_launcher(
    frame: &mut Frame<'_>,
    app: &App,
    modal: &ModalState,
    slot: AgentSlot,
    area: Rect,
    title: &str,
) {
    let field = match slot {
        AgentSlot::Primary => DialogField::AgentSettings,
        AgentSlot::Designer => DialogField::DesignerAgentSettings,
        AgentSlot::Reviewer => DialogField::ReviewerAgentSettings,
    };
    let active = modal.active_field() == field || app.is_hovered(HitAction::ModalField(field));
    let border = if active {
        app.theme.focus
    } else {
        app.theme.border
    };
    let values = [
        modal.backend_text_for(slot),
        modal.model_text_for(slot),
        modal.effort_text_for(slot),
        modal.agent_text_for(slot),
    ];
    let summary = values
        .into_iter()
        .flatten()
        .map(|value| sanitize_terminal_text(&value))
        .collect::<Vec<_>>()
        .join(" · ");
    let summary = if summary.is_empty() {
        "defaults".to_string()
    } else {
        summary
    };
    let suffix = "  › Enter to configure";
    let summary_width =
        usize::from(area.width.saturating_sub(2)).saturating_sub(suffix.chars().count());
    let label = format!("{}{}", truncate_display(&summary, summary_width), suffix);
    frame.render_widget(
        Paragraph::new(label).block(
            Block::default()
                .title(format!(" {title} "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        ),
        area,
    );
}

/// Read-only row: whether worktree isolation can run in this project. The
/// probe was taken once when the dialog opened (`modal.isolation_status`);
/// changes to git or the repository show after closing and reopening.
fn render_isolation_status(frame: &mut Frame<'_>, app: &App, modal: &ModalState, area: Rect) {
    let Some(availability) = &modal.isolation_status else {
        return;
    };
    let (value, color) = if availability.is_available() {
        (availability.to_string(), app.theme.ok)
    } else {
        (format!("unavailable — {availability}"), app.theme.err)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("Worktree isolation: "),
            Span::styled(value, Style::default().fg(color)),
        ]))
        .block(
            Block::default()
                .title(" Isolation ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.border)),
        ),
        area,
    );
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
        DialogField::DesignerBackend => (
            modal.designer.backend_options.len(),
            modal.designer.backend_selected,
        ),
        DialogField::DesignerModel => (
            modal.designer.model_options.len(),
            modal.designer.model_selected,
        ),
        DialogField::DesignerEffort => (
            modal.designer.effort_options.len(),
            modal.designer.effort_selected,
        ),
        DialogField::DesignerAgent => (
            modal.designer.agent_options.len(),
            modal.designer.agent_selected,
        ),
        DialogField::ReviewerBackend => (
            modal.reviewer.backend_options.len(),
            modal.reviewer.backend_selected,
        ),
        DialogField::ReviewerModel => (
            modal.reviewer.model_options.len(),
            modal.reviewer.model_selected,
        ),
        DialogField::ReviewerEffort => (
            modal.reviewer.effort_options.len(),
            modal.reviewer.effort_selected,
        ),
        DialogField::ReviewerAgent => (
            modal.reviewer.agent_options.len(),
            modal.reviewer.agent_selected,
        ),
        DialogField::ReviewerOnChanges => (
            modal.reviewer_on_changes_options.len(),
            modal.reviewer_on_changes_selected,
        ),
        DialogField::ExecutorMiddle1
        | DialogField::ExecutorMiddle2
        | DialogField::ExecutorMiddle3
        | DialogField::ExecutorCheap1
        | DialogField::ExecutorCheap2
        | DialogField::ExecutorCheap3 => (
            modal.executor_slot_options.len(),
            modal.executor_selected[executor_slot_index(field)],
        ),
        _ => (0, 0),
    };
    if modal.field_filter(field).is_some() {
        register_filtered_options(
            hitboxes,
            area,
            field,
            &modal.visible_options(field),
            selected,
        );
        return;
    }
    register_options(hitboxes, area, field, count, selected);
}

/// Hitboxes for a filtered selector. Rows are laid out below the filter line,
/// and each row carries the option's index in the unfiltered list so clicks
/// resolve to the same option `select_option` expects.
fn register_filtered_options(
    hitboxes: &mut Vec<Hitbox>,
    area: Rect,
    field: DialogField,
    visible: &[usize],
    selected: usize,
) {
    let rows = area.height.saturating_sub(3) as usize;
    let highlight = visible
        .iter()
        .position(|index| *index == selected)
        .unwrap_or(0);
    let start = list_viewport_start(highlight, visible.len(), rows);
    for (offset, option) in visible
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(position, option)| (position - start, *option))
    {
        hitboxes.insert(
            0,
            Hitbox {
                area: Rect {
                    x: area.x.saturating_add(1),
                    y: area.y.saturating_add(2 + offset as u16),
                    width: area.width.saturating_sub(2),
                    height: 1,
                },
                action: HitAction::ModalOption {
                    field,
                    index: option,
                },
            },
        );
    }
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
    render_select_filtered(
        frame, app, title, options, selected, area, active, None, false,
    );
}

/// Draw a selector, optionally with a filter row above the options. `filter`
/// is `Some` only for selectors that opt into filtering; `error` paints the
/// section in the error colour after Enter found nothing to select.
#[allow(clippy::too_many_arguments)]
fn render_select_filtered(
    frame: &mut Frame<'_>,
    app: &App,
    title: &str,
    options: &[SelectOption],
    selected: usize,
    area: Rect,
    active: bool,
    filter: Option<&str>,
    error: bool,
) {
    let field = select_field_from_title(title);
    let active = field
        .map(|field| active || select_field_hovered(app, field, options.len()))
        .unwrap_or(active);
    let visible = match filter {
        Some(filter) => filtered_indices(options, filter),
        None => (0..options.len()).collect::<Vec<_>>(),
    };
    let mut items = visible
        .iter()
        .map(|index| {
            let mut item = ListItem::new(sanitize_terminal_text(&options[*index].label));
            if let Some(field) = field {
                item = item.style(option_hover_style(
                    app,
                    HitAction::ModalOption {
                        field,
                        index: *index,
                    },
                ));
            }
            item
        })
        .collect::<Vec<_>>();
    let matched = !items.is_empty();
    if !matched {
        let placeholder = if filter.is_some() && !options.is_empty() {
            "no matches"
        } else {
            "-"
        };
        items.push(ListItem::new(placeholder).style(Style::default().fg(app.theme.muted)));
    }
    // A selection outside the filtered list highlights the first match, which
    // is exactly the option Enter would commit.
    let highlight = visible
        .iter()
        .position(|index| *index == selected)
        .unwrap_or(0);
    let border = select_border(app, active, error);
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    let Some(filter) = filter else {
        render_list_in(frame, app, items, highlight, area, Some(block), true);
        return;
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    frame.render_widget(filter_line(app, filter, error), Rect { height: 1, ..inner });
    render_list_in(
        frame,
        app,
        items,
        highlight,
        Rect {
            y: inner.y.saturating_add(1),
            height: inner.height.saturating_sub(1),
            ..inner
        },
        None,
        matched,
    );
}

fn select_border(app: &App, active: bool, error: bool) -> ratatui::style::Color {
    if error {
        app.theme.err
    } else if active {
        app.theme.focus
    } else {
        app.theme.border
    }
}

fn filter_line(app: &App, filter: &str, error: bool) -> Paragraph<'static> {
    let prefix = ratatui::text::Span::styled(
        "/ ",
        Style::default().fg(if error {
            app.theme.err
        } else {
            app.theme.muted
        }),
    );
    let body = if filter.is_empty() {
        ratatui::text::Span::styled(
            "type to filter",
            Style::default()
                .fg(app.theme.muted)
                .add_modifier(Modifier::ITALIC),
        )
    } else {
        ratatui::text::Span::styled(
            sanitize_terminal_text(filter),
            Style::default()
                .fg(if error { app.theme.err } else { app.theme.fg })
                .add_modifier(Modifier::BOLD),
        )
    };
    Paragraph::new(Line::from(vec![prefix, body]))
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
        "Designer · Backend" => Some(DialogField::DesignerBackend),
        "Designer · Model" => Some(DialogField::DesignerModel),
        "Designer · Effort" => Some(DialogField::DesignerEffort),
        "Designer · Agent" => Some(DialogField::DesignerAgent),
        "Reviewer · Backend" => Some(DialogField::ReviewerBackend),
        "Reviewer · Model" => Some(DialogField::ReviewerModel),
        "Reviewer · Effort" => Some(DialogField::ReviewerEffort),
        "Reviewer · Agent" => Some(DialogField::ReviewerAgent),
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
    let block = Block::default()
        .title(title.to_string())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    render_list_in(frame, app, items, selected, area, Some(block), true);
}

/// Render list items into `area`. `block` is `None` when the caller already
/// drew the surrounding border (filterable selectors reserve a row inside it),
/// and `highlight` is false when there is nothing selectable to point at.
fn render_list_in(
    frame: &mut Frame<'_>,
    app: &App,
    items: Vec<ListItem<'static>>,
    selected: usize,
    area: Rect,
    block: Option<Block<'static>>,
    highlight: bool,
) {
    let mut state = ListState::default();
    let selected = selected.min(items.len().saturating_sub(1));
    state.select(highlight.then_some(selected));
    let chrome = if block.is_some() { 2 } else { 0 };
    *state.offset_mut() = list_viewport_start(
        selected,
        items.len(),
        area.height.saturating_sub(chrome) as usize,
    );
    let mut list = List::new(items).highlight_symbol("› ").highlight_style(
        Style::default()
            .fg(app.theme.focus)
            .add_modifier(Modifier::BOLD),
    );
    if let Some(block) = block {
        list = list.block(block);
    }
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
        // Enter walks forward like Tab now, so the hint has to name it.
        let nav_hint = "use Tab, Enter or Shift + Tab to navigate";
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
fn toggle_on_space(value: &mut bool, key: ratatui::crossterm::event::KeyEvent) {
    if key.code == ratatui::crossterm::event::KeyCode::Char(' ') {
        *value = !*value;
    }
}

fn render_checkbox(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    title: &str,
    label: &str,
    checked: bool,
    active: bool,
) {
    let border = if active {
        app.theme.focus
    } else {
        app.theme.border
    };
    let mark = if checked { "☑" } else { "☐" };
    frame.render_widget(
        Paragraph::new(format!("{mark} {label} (Space toggles)")).block(
            Block::default()
                .title(format!(" {title} "))
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
        Paragraph::new(format!(
            "{mark} Esc from board opens projects (Space toggles)"
        ))
        .block(
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
            // Enter, Shift+Enter, and Alt+Enter all insert a newline. The
            // title names Enter because that is the key every terminal can
            // deliver; the modifiers are accepted too.
            .title(" Description (Ctrl+V image paste, Enter newline) ")
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
