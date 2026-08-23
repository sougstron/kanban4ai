use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::core::error::KanbanError;
use crate::core::timefmt;

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $value)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $value),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = KanbanError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($value => Ok($name::$variant),)+
                    other => Err(KanbanError::Invalid(format!(
                        concat!("invalid ", stringify!($name), ": {}"),
                        other
                    ))),
                }
            }
        }
    };
}

string_enum!(TaskStatus {
    Todo => "todo",
    InProgress => "in_progress",
    Review => "review",
    Done => "done",
    Archive => "archive",
});

// Sub-state of an In Progress task. The board columns (`TaskStatus`) do not
// change; this records where a delegated run stands inside In Progress.
// Absent (`None`) means "In Progress the old way" and is treated as
// `RunPhase::Execute` wherever a phase is required.
string_enum!(RunPhase {
    Queued => "queued",
    Design => "design",
    Execute => "execute",
    Review => "review",
});

// Which bot a session is running as. Derived from `RunPhase` (`design` →
// designer, `review` → reviewer, everything else including a missing
// phase → executor) and handed to the prompt builder and the move gate.
string_enum!(Role {
    Executor => "executor",
    Designer => "designer",
    Reviewer => "reviewer",
});

impl Role {
    pub fn from_phase(phase: Option<RunPhase>) -> Self {
        match phase {
            Some(RunPhase::Design) => Role::Designer,
            Some(RunPhase::Review) => Role::Reviewer,
            _ => Role::Executor,
        }
    }
}

string_enum!(SessionStatus {
    Active => "active",
    Closed => "closed",
    Crashed => "crashed",
});

string_enum!(MessageRole {
    Agent => "agent",
    Human => "human",
    System => "system",
});

string_enum!(MessageKind {
    System => "system",
    Task => "task",
    Question => "question",
    Suggestion => "suggestion",
    Context => "context",
    ReviewEdit => "review_edit",
    AgentStep => "agent_step",
});

string_enum!(MessageStatus {
    Open => "open",
    Answered => "answered",
    Resolved => "resolved",
    Rejected => "rejected",
});

fn default_message_role() -> MessageRole {
    MessageRole::System
}

fn default_message_kind() -> MessageKind {
    MessageKind::Question
}

fn default_message_status() -> MessageStatus {
    MessageStatus::Open
}

/// One thread entry. Field order mirrors the Python `Message.to_dict` so the
/// emitted YAML keeps the familiar key order; `answered_by_role` and
/// `resolved_at` are only written when set, exactly like the original.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    #[serde(default = "default_message_role")]
    pub role: MessageRole,
    #[serde(default = "default_message_kind")]
    pub kind: MessageKind,
    #[serde(default)]
    pub body: String,
    #[serde(default = "default_message_status")]
    pub status: MessageStatus,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub variants: Vec<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default = "timefmt::now", with = "timefmt::serde_naive")]
    pub created_at: NaiveDateTime,
    #[serde(default = "timefmt::now", with = "timefmt::serde_naive")]
    pub updated_at: NaiveDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by_role: Option<MessageRole>,
    #[serde(
        default,
        with = "timefmt::serde_naive_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub resolved_at: Option<NaiveDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl Message {
    pub fn new(
        id: impl Into<String>,
        role: MessageRole,
        kind: MessageKind,
        body: impl Into<String>,
    ) -> Self {
        let now = timefmt::now();
        Message {
            id: id.into(),
            role,
            kind,
            body: body.into(),
            status: MessageStatus::Open,
            parent_id: None,
            variants: Vec::new(),
            author: None,
            answer: None,
            created_at: now,
            updated_at: now,
            answered_by_role: None,
            resolved_at: None,
            origin: None,
        }
    }
}

/// Sidecar per-task conversation state (`.kanban/threads/TASK-NNN.yaml`).
/// `base_rev`/`base_messages` snapshot what this instance last loaded from disk
/// so a save can three-way-merge with concurrent writers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Thread {
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub rev: u64,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(skip)]
    pub base_rev: u64,
    #[serde(skip)]
    pub base_messages: HashMap<String, Message>,
}

impl Thread {
    pub fn new(task_id: impl Into<String>) -> Self {
        Thread {
            task_id: task_id.into(),
            ..Default::default()
        }
    }

    /// Record the current state as the load-time baseline used by merge-on-save.
    pub fn snapshot_base(&mut self) {
        self.base_rev = self.rev;
        self.base_messages = self
            .messages
            .iter()
            .map(|m| (m.id.clone(), m.clone()))
            .collect();
    }
}

/// A task's YAML frontmatter plus its Markdown body (`description`). The body
/// is not part of the frontmatter, hence `skip_serializing`; key order mirrors
/// the Python `Task.to_dict`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing)]
    pub description: String,
    #[serde(default = "default_task_status")]
    pub status: TaskStatus,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default = "timefmt::now", with = "timefmt::serde_naive")]
    pub created_at: NaiveDateTime,
    #[serde(default = "timefmt::now", with = "timefmt::serde_naive")]
    pub updated_at: NaiveDateTime,
    /// Most recent time work reached Review or Done. Re-running a reviewed
    /// task preserves this value until the next completion replaces it.
    #[serde(
        default,
        with = "timefmt::serde_naive_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub completed_at: Option<NaiveDateTime>,
    #[serde(default)]
    pub has_questions: bool,
    #[serde(default)]
    pub context_file: Option<String>,
    #[serde(default)]
    pub context_size: u64,
    #[serde(default)]
    pub ai_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_effort: Option<String>,
    #[serde(default)]
    pub agent_backend: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub interactive: bool,
    #[serde(default)]
    pub chained_to: Option<String>,
    #[serde(default)]
    pub review_edits: String,
    /// Automatic relaunches consumed after clean agent exits that left the
    /// task In Progress without `done`/`ask`/`waiting`. Reset whenever a
    /// human (re)starts the task. Omitted from frontmatter while zero so
    /// legacy boards round-trip byte-identically.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub auto_resumes: u32,
    /// Set when an agent moved this task into Review and the user has not yet
    /// opened its detail. Drives the yellow "unseen review" notifier on the
    /// board card and the projects list. Cleared by any human action on the
    /// task (open detail, move, done, rerun, answer). Omitted while false so
    /// legacy boards round-trip byte-identically.
    #[serde(default, skip_serializing_if = "is_false")]
    pub review_unseen: bool,
    /// Where the current In Progress run stands (queued / design / execute /
    /// review). `None` is not serialized — legacy boards keep round-tripping
    /// byte-identically — and reads as `Execute` wherever a phase is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_phase: Option<RunPhase>,
    /// Crash auto-restarts consumed for this task. Separate from
    /// `auto_resumes`: a clean-exit resume and a crash restart are different
    /// failure modes and must not share a counter. Reset whenever a human
    /// (re)starts the task. Omitted while zero so legacy boards round-trip
    /// byte-identically.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub crash_restarts: u32,
    /// When the crash-restart pump should re-queue an In Progress task after
    /// a crash (`now + delay[crash_restarts]`). Omitted while unset.
    #[serde(
        default,
        with = "timefmt::serde_naive_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub restart_at: Option<NaiveDateTime>,
    /// Consecutive bot-review rounds consumed for this task. Capped by
    /// `orchestration.reviewer.max_rounds`; exhausting it falls through to
    /// human Review. Reset on a human restart. Omitted while zero.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub review_rounds: u32,
    /// A designer pass has already finished for this task and its plan is on
    /// the thread. Gates the design phase directly instead of inferring it
    /// from `review_rounds`, so a crash restart mid-execute resumes the
    /// executor rather than re-planning from scratch. Cleared only when a
    /// human sends the task back to To Do. Omitted while false so legacy
    /// boards round-trip byte-identically.
    #[serde(default, skip_serializing_if = "is_false")]
    pub designed: bool,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn default_task_status() -> TaskStatus {
    TaskStatus::Todo
}

impl Task {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        let now = timefmt::now();
        Task {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            status: TaskStatus::Todo,
            session: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
            has_questions: false,
            context_file: None,
            context_size: 0,
            ai_model: None,
            ai_effort: None,
            agent_backend: None,
            agent_name: None,
            interactive: false,
            chained_to: None,
            review_edits: String::new(),
            auto_resumes: 0,
            review_unseen: false,
            run_phase: None,
            crash_restarts: 0,
            restart_at: None,
            review_rounds: 0,
            designed: false,
        }
    }

    /// Clear the automatic-relaunch bookkeeping: the clean-exit resume budget,
    /// the crash-restart counter and its pending deadline. Used on its own
    /// where a human nudges a run that is still the *same* run (wake/revoke,
    /// re-run of a stranded session) — those must not also clear the reviewer
    /// bounce count, or a task woken mid-review would re-arm `max_rounds`
    /// from zero and the bounce loop the cap exists to stop could run forever.
    pub fn reset_auto_restart(&mut self) {
        self.auto_resumes = 0;
        self.crash_restarts = 0;
        self.restart_at = None;
    }

    /// Clear every automatic-relaunch counter after a human restarts the work
    /// itself (run, re-run from Review, recover, take, queue). That is a fresh
    /// attempt, so the bot-review bounce count starts over too.
    pub fn reset_human_restart(&mut self) {
        self.reset_auto_restart();
        self.review_rounds = 0;
    }
}

/// Agent session metadata (`.kanban/sessions/<id>.yaml`). `ended_at` is only
/// written once the session has ended, matching the Python serializer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "timefmt::now", with = "timefmt::serde_naive")]
    pub started_at: NaiveDateTime,
    #[serde(default = "default_session_status")]
    pub status: SessionStatus,
    #[serde(default = "timefmt::now", with = "timefmt::serde_naive")]
    pub last_seen: NaiveDateTime,
    #[serde(
        default,
        with = "timefmt::serde_naive_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub ended_at: Option<NaiveDateTime>,
    /// Deadline of a wait the agent declared via `kanban waiting`. While it
    /// is in the future the session counts as alive even without heartbeats;
    /// once it passes, the board relaunches the agent instead of crashing it.
    #[serde(
        default,
        with = "timefmt::serde_naive_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub wait_until: Option<NaiveDateTime>,
    /// What the agent said it is waiting for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_note: Option<String>,
    /// Set when the agent process exited while its declared wait was still
    /// pending, so the deadline relaunch does not need heartbeat heuristics
    /// to know the process is gone.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wait_exited: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_session_status() -> SessionStatus {
    SessionStatus::Active
}

impl Session {
    pub fn new(id: impl Into<String>, task_id: impl Into<String>) -> Self {
        let now = timefmt::now();
        Session {
            id: id.into(),
            task_id: task_id.into(),
            name: None,
            started_at: now,
            status: SessionStatus::Active,
            last_seen: now,
            ended_at: None,
            wait_until: None,
            wait_note: None,
            wait_exited: false,
        }
    }

    pub fn named(
        id: impl Into<String>,
        task_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        let mut session = Self::new(id, task_id);
        session.name = Some(name.into());
        session
    }
}
