//! Application-collected usage statistics (`.kanban/stats/events.jsonl`).
//!
//! Purely programmatic: the board itself appends one small JSON line at each
//! state transition it already drives (a session starting/ending, a declared
//! wait, a queue entry, a crash-restart backoff), tagged with whichever
//! backend/model/effort/agent the task was launched with. No agent ever writes
//! here — contrast `core::provenance`, which is genuinely agent-driven
//! (harvested from what the agent's own transcript says it did). Recording is
//! best-effort and infallible from the caller's perspective: a write failure
//! here must never break the state transition it is describing.
//!
//! Every hook records one **edge** of a phase (`Enter`/`Exit` of `Running` /
//! `Queued` / `Waiting` / `Retry`) rather than a pre-computed duration, so a
//! call site never needs to know when the *previous* edge happened — that
//! pairing is reconstructed once, lazily, by [`pair_records`] when a report is
//! rendered. An edge whose partner never arrives (the process died before
//! writing it, or the board was upgraded onto a fresh events file) is simply
//! dropped rather than guessed at.
//!
//! Tags (backend/model/effort/agent) are only carried on a `Running` `Enter`
//! and on a `Usage` record — the other phases have no backend/model breakdown
//! in the report (see the task description), so recording them there would
//! just be dead weight in the file.
//!
//! Storage is per-project (`.kanban/stats/`, alongside `sessions/` and
//! `logs/`), exactly like every other per-board record; the Stats window
//! aggregates across every registered project by reading each one's file in
//! turn ([`collect_store_report`]), the same pattern `kanban daemon` uses to
//! walk the store.
//!
//! Two caveats worth stating plainly, both acceptable for a for-fun feature
//! and not for anything load-bearing:
//! - Task ids are recycled (see `docs/data-model.md`), so an "all time" count
//!   keyed by task id can, in principle, conflate two different tasks that
//!   happened to reuse the same id months apart. Cross-project grouping keys
//!   every task id by its project to avoid the far more likely collision
//!   (the same `TASK-005` in two different projects).
//! - "Задача выполнена" (a completed task) is approximated as *a task id that
//!   has at least one recorded [`Usage`] entry* — i.e. at least one agent
//!   session for it closed with tokens accounted for — rather than "reached
//!   the Done column", because Done status is not stable history (tasks get
//!   reopened, archived, or deleted) while the event log is append-only.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::core::error::Result;
use crate::core::limit_share::LimitShare;
use crate::core::limits::format_span;
use crate::core::models::Task;
use crate::core::project::ProjectStore;
use crate::core::telemetry::{self, TokenBreakdown};
use crate::core::timefmt;
use crate::core::token_costs::{CostWeights, TokenCosts};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// An agent session is active and not in a declared wait — Design,
    /// Execute and Review sub-phases are all `Running`; the report does not
    /// split them.
    Running,
    /// `run_phase == Queued`: parked in the dispatcher queue.
    Queued,
    /// The agent declared a wait (`kanban waiting`) — awaiting its own
    /// response, a download, or anything else it named.
    Waiting,
    /// Crash-restart backoff: crashed, and a retry is scheduled.
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Edge {
    Enter,
    Exit,
}

/// Backend/model/effort/agent snapshot to stamp onto a `Running` span or a
/// [`Usage`] record. Captured at the moment it is recorded, not looked up
/// later — a task's fields describe only its *last* launch, so recording
/// late would mislabel a re-run under a different backend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

impl Tags {
    pub fn from_task(task: &Task) -> Self {
        Tags {
            backend: task.agent_backend.clone(),
            model: task.ai_model.clone(),
            effort: task.ai_effort.clone(),
            agent: task.agent_name.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Phase {
        #[serde(with = "timefmt::serde_naive")]
        ts: NaiveDateTime,
        task_id: String,
        phase: Phase,
        edge: Edge,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    Usage {
        #[serde(with = "timefmt::serde_naive")]
        ts: NaiveDateTime,
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        tokens: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        /// Whole prompt (fresh + cache read + cache write). Kept as the
        /// historical total so an older binary reading this file still sees
        /// the prompt size it always did. Absent when the record has no split.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<i64>,
        /// Fresh (uncached) prompt tokens. Absent on records written before
        /// the split was stored separately; those are recovered as
        /// `input - cache_read - cache_write`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_fresh: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write: Option<i64>,
    },
}

/// In-memory split of one usage record.
///
/// `input_fresh` present → that value is the fresh prompt. An older split
/// (input/output present, no `input_fresh`) recovers fresh as
/// `input - cache_read - cache_write`. A tokens-only record returns `None`
/// and is counted as mixed, never folded into the fresh or cache columns.
fn usage_breakdown(
    input: Option<i64>,
    input_fresh: Option<i64>,
    output: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
) -> Option<TokenBreakdown> {
    if let Some(fresh) = input_fresh {
        return Some(TokenBreakdown {
            input: fresh,
            output: output.unwrap_or(0),
            cache_read: cache_read.unwrap_or(0),
            cache_write: cache_write.unwrap_or(0),
        });
    }
    if input.is_some() || output.is_some() {
        let cache_read = cache_read.unwrap_or(0);
        let cache_write = cache_write.unwrap_or(0);
        return Some(TokenBreakdown {
            input: (input.unwrap_or(0) - cache_read - cache_write).max(0),
            output: output.unwrap_or(0),
            cache_read,
            cache_write,
        });
    }
    None
}

fn events_path(project_path: &Path) -> PathBuf {
    project_path
        .join(".kanban")
        .join("stats")
        .join("events.jsonl")
}

/// Append one record, best-effort: a write failure here (disk full, missing
/// permissions) must never surface as an error on the state transition that
/// triggered it. Callers are expected to already hold the board lock, same as
/// every other task/session mutation this rides alongside.
fn append(project_path: &Path, record: &Record) {
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    let path = events_path(project_path);
    let Some(parent) = path.parent() else { return };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

/// Record a phase starting. `tags` matters only for [`Phase::Running`] —
/// pass `&Tags::default()` for the others.
pub fn record_enter(project_path: &Path, task_id: &str, phase: Phase, tags: &Tags) {
    append(
        project_path,
        &Record::Phase {
            ts: timefmt::now(),
            task_id: task_id.to_string(),
            phase,
            edge: Edge::Enter,
            backend: tags.backend.clone(),
            model: tags.model.clone(),
            effort: tags.effort.clone(),
            agent: tags.agent.clone(),
        },
    );
}

/// Record a phase ending. No tags: the report reads them off the matching
/// `Enter`.
pub fn record_exit(project_path: &Path, task_id: &str, phase: Phase) {
    append(
        project_path,
        &Record::Phase {
            ts: timefmt::now(),
            task_id: task_id.to_string(),
            phase,
            edge: Edge::Exit,
            backend: None,
            model: None,
            effort: None,
            agent: None,
        },
    );
}

/// Record a session's final token tally. A `tokens <= 0` reading (nothing
/// parseable in the transcript) is skipped — an absent record is truer than a
/// zero that would otherwise look like a genuinely free run.
pub fn record_usage(
    project_path: &Path,
    task_id: &str,
    session_id: &str,
    tokens: i64,
    breakdown: Option<&TokenBreakdown>,
    tags: &Tags,
) {
    if tokens <= 0 {
        return;
    }
    append(
        project_path,
        &Record::Usage {
            ts: timefmt::now(),
            task_id: task_id.to_string(),
            session_id: Some(session_id.to_string()),
            tokens,
            backend: tags.backend.clone(),
            model: tags.model.clone(),
            effort: tags.effort.clone(),
            agent: tags.agent.clone(),
            input: breakdown.map(|b| b.prompt_total()),
            input_fresh: breakdown.map(|b| b.input),
            output: breakdown.map(|b| b.output),
            cache_read: breakdown.map(|b| b.cache_read),
            cache_write: breakdown.map(|b| b.cache_write),
        },
    );
}

/// How many bad lines one file keeps in [`SourceDiagnostics::samples`].
/// Rendering applies `stats_issue_samples` on top of this; the cap only stops
/// a huge corrupt log from retaining every bad line. `skipped_lines` stays exact.
const MAX_STORED_LINE_ISSUES: usize = 1000;
const DIAGNOSTIC_REASON_CHARS: usize = 160;

/// One skipped events line. `line` is 1-based in the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineIssue {
    pub line: usize,
    pub reason: String,
}

/// What [`read_records`] could and could not read from one events file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceDiagnostics {
    pub path: PathBuf,
    pub loaded: usize,
    pub skipped_lines: usize,
    pub samples: Vec<LineIssue>,
    pub read_error: Option<String>,
}

impl SourceDiagnostics {
    pub fn is_clean(&self) -> bool {
        self.read_error.is_none() && self.skipped_lines == 0
    }
}

fn load_records(project_path: &Path) -> Vec<Record> {
    read_records(&events_path(project_path)).0
}

/// Read one events file. A missing file is empty and clean (the project never
/// recorded a session). Any other I/O error yields no records and
/// [`SourceDiagnostics::read_error`]. Lines are split on `b'\n'` so one
/// non-UTF-8 byte does not drop the rest of the file. Whitespace-only lines
/// are ignored. A failing unterminated final line is marked as possibly still
/// being written.
fn read_records(path: &Path) -> (Vec<Record>, SourceDiagnostics) {
    let mut source = SourceDiagnostics {
        path: path.to_path_buf(),
        ..SourceDiagnostics::default()
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return (Vec::new(), source),
        Err(err) => {
            source.read_error = Some(err.to_string());
            return (Vec::new(), source);
        }
    };
    let mut records = Vec::new();
    if bytes.is_empty() {
        return (records, source);
    }
    let mut start = 0usize;
    let mut line_no = 1usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            consider_line(
                &bytes[start..index],
                line_no,
                false,
                &mut records,
                &mut source,
            );
            start = index + 1;
            line_no += 1;
        }
        index += 1;
    }
    if start < bytes.len() {
        consider_line(&bytes[start..], line_no, true, &mut records, &mut source);
    }
    (records, source)
}

fn consider_line(
    raw: &[u8],
    line_no: usize,
    unterminated: bool,
    records: &mut Vec<Record>,
    source: &mut SourceDiagnostics,
) {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let text = match std::str::from_utf8(raw) {
        Ok(text) => text,
        Err(_) => {
            note_issue(source, line_no, unterminated, || {
                "invalid UTF-8".to_string()
            });
            return;
        }
    };
    if text.trim().is_empty() {
        return;
    }
    match serde_json::from_str::<Record>(text.trim()) {
        Ok(record) => {
            records.push(record);
            source.loaded += 1;
        }
        Err(err) => note_issue(source, line_no, unterminated, || parse_reason(&err)),
    }
}

/// serde's text without its " at line 1 column N" suffix: every line is
/// parsed on its own, so its line is always 1 and would contradict the file
/// line the report shows. The column is kept.
fn parse_reason(err: &serde_json::Error) -> String {
    let text = err.to_string();
    let suffix = format!(" at line {} column {}", err.line(), err.column());
    match text.strip_suffix(&suffix) {
        Some(message) => format!("{message} (column {})", err.column()),
        None => text,
    }
}

/// Count one skipped line; `reason` is only built while samples are kept,
/// so a huge corrupt log costs no formatting past the cap.
fn note_issue(
    source: &mut SourceDiagnostics,
    line: usize,
    unterminated: bool,
    reason: impl FnOnce() -> String,
) {
    source.skipped_lines += 1;
    if source.samples.len() >= MAX_STORED_LINE_ISSUES {
        return;
    }
    let reason = reason();
    let reason = if unterminated {
        format!("incomplete final line (possibly still being written): {reason}")
    } else {
        reason
    };
    source.samples.push(LineIssue { line, reason });
}

/// One closed `[start, end)` phase span, tagged from its `Enter` edge.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub task_id: String,
    pub phase: Phase,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
}

impl Span {
    pub fn seconds(&self) -> i64 {
        (self.end - self.start).num_seconds().max(0)
    }
}

/// One session's final token tally.
#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub task_id: String,
    pub session_id: Option<String>,
    pub ts: NaiveDateTime,
    pub tokens: i64,
    pub breakdown: Option<TokenBreakdown>,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectStats {
    pub spans: Vec<Span>,
    pub usage: Vec<Usage>,
    pub source: SourceDiagnostics,
}

/// Load and reconstruct one project's stats from its events file.
pub fn load(project_path: &Path) -> ProjectStats {
    let (records, source) = read_records(&events_path(project_path));
    ProjectStats {
        source,
        ..pair_records(records)
    }
}

/// Pair `Enter`/`Exit` phase edges per `(task_id, phase)` in timestamp order
/// into closed spans, and split out `Usage` records untouched. An `Exit` with
/// no open `Enter` is dropped (stray/out-of-order data); a second `Enter`
/// before the matching `Exit` replaces the first (defensive — the recording
/// side never does this, but a hand-edited or concatenated file might).
fn pair_records(records: Vec<Record>) -> ProjectStats {
    #[allow(clippy::type_complexity)]
    let mut phase_events: Vec<(
        NaiveDateTime,
        String,
        Phase,
        Edge,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = Vec::new();
    let mut usage = Vec::new();
    for record in records {
        match record {
            Record::Phase {
                ts,
                task_id,
                phase,
                edge,
                backend,
                model,
                effort,
                agent,
            } => phase_events.push((ts, task_id, phase, edge, backend, model, effort, agent)),
            Record::Usage {
                ts,
                task_id,
                session_id,
                tokens,
                backend,
                model,
                effort,
                agent,
                input,
                input_fresh,
                output,
                cache_read,
                cache_write,
            } => usage.push(Usage {
                task_id,
                session_id,
                ts,
                tokens,
                breakdown: usage_breakdown(input, input_fresh, output, cache_read, cache_write),
                backend,
                model,
                effort,
                agent,
            }),
        }
    }
    phase_events.sort_by_key(|entry| entry.0);

    type OpenEnter = (
        NaiveDateTime,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let mut open: HashMap<(String, Phase), OpenEnter> = HashMap::new();
    let mut spans = Vec::new();
    for (ts, task_id, phase, edge, backend, model, effort, agent) in phase_events {
        let key = (task_id.clone(), phase);
        match edge {
            Edge::Enter => {
                open.insert(key, (ts, backend, model, effort, agent));
            }
            Edge::Exit => {
                if let Some((start, backend, model, effort, agent)) = open.remove(&key) {
                    spans.push(Span {
                        task_id,
                        phase,
                        start,
                        end: ts,
                        backend,
                        model,
                        effort,
                        agent,
                    });
                }
            }
        }
    }
    ProjectStats {
        spans,
        usage,
        ..ProjectStats::default()
    }
}

/// Per-task totals behind the TUI detail's Analytics panel: every agent
/// session the task ever ran (first run, review passes, reruns and crash
/// restarts alike), read from the same events file as the store report.
/// Closed sessions only — the caller adds the live session on top (its
/// running span via [`Self::running_since`], its tokens from live telemetry
/// unless [`Self::counted_sessions`] already has it).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskAnalytics {
    /// Agent sessions started (`Running` enters).
    pub runs: usize,
    /// Sum of closed `Running` spans.
    pub running_seconds: i64,
    /// Start of the `Running` span still open, if any.
    pub running_since: Option<NaiveDateTime>,
    /// Sum of closed declared-wait (`Waiting`) spans.
    pub waiting_seconds: i64,
    /// Token split summed over closed sessions.
    pub breakdown: TokenBreakdown,
    /// Tokens of closed sessions with no split on record and no transcript
    /// left to recover one from.
    pub unsplit_tokens: i64,
    /// Sessions whose usage is already in the totals above.
    pub counted_sessions: HashSet<String>,
    /// Share of each provider limit window the task consumed, per role
    /// (`core::limit_share`). Left empty here; the caller fills it in.
    pub limit_shares: Vec<LimitShare>,
}

impl TaskAnalytics {
    /// Token split of every run so far: the closed sessions plus the live
    /// session's transcript telemetry, unless that session's usage is already
    /// on record. Shared by the card and the detail panel so both agree.
    pub fn tokens_with_live(
        &self,
        live_session: Option<&str>,
        live: Option<&TokenBreakdown>,
    ) -> TokenBreakdown {
        let mut tokens = self.breakdown;
        let counted = live_session.is_some_and(|session| self.counted_sessions.contains(session));
        if !counted && let Some(live) = live {
            tokens.add(live);
        }
        tokens
    }
}

/// Size and mtime of the stats events file (`None` while it is missing), so
/// callers can cache [`task_analytics`] until a new record lands.
pub type EventsStamp = Option<(u64, std::time::SystemTime)>;

pub fn events_stamp(project_path: &Path) -> EventsStamp {
    let meta = fs::metadata(events_path(project_path)).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// Aggregate one task's analytics. Only events at or after `since` (the
/// task's `created_at`) count, so a recycled task id does not inherit the
/// history of the abandoned task that held it before.
pub fn task_analytics(project_path: &Path, task_id: &str, since: NaiveDateTime) -> TaskAnalytics {
    let mut pending = TaskAnalyticsBuilder::default();
    for record in load_records(project_path) {
        let (id, ts) = record_task_time(&record);
        if id == task_id && ts >= since {
            pending.record(project_path, record);
        }
    }
    pending.finish()
}

/// Aggregate all requested tasks with one read of the project's event log.
/// Cutoffs, session deduplication and legacy transcript recovery match
/// [`task_analytics`]. Tasks without matching events receive empty totals;
/// if an id is repeated, its last supplied creation time wins.
pub fn tasks_analytics(
    project_path: &Path,
    tasks: &[(String, NaiveDateTime)],
) -> HashMap<String, TaskAnalytics> {
    let mut pending: HashMap<_, _> = tasks
        .iter()
        .map(|(id, since)| (id.as_str(), (*since, TaskAnalyticsBuilder::default())))
        .collect();
    if !pending.is_empty() {
        for record in load_records(project_path) {
            let (id, ts) = record_task_time(&record);
            if let Some((since, builder)) = pending.get_mut(id)
                && ts >= *since
            {
                builder.record(project_path, record);
            }
        }
    }
    pending
        .into_iter()
        .map(|(id, (_, builder))| (id.to_owned(), builder.finish()))
        .collect()
}

fn record_task_time(record: &Record) -> (&str, NaiveDateTime) {
    match record {
        Record::Phase { task_id, ts, .. } | Record::Usage { task_id, ts, .. } => (task_id, *ts),
    }
}

#[derive(Default)]
struct TaskAnalyticsBuilder {
    analytics: TaskAnalytics,
    edges: Vec<(NaiveDateTime, Phase, Edge)>,
}

impl TaskAnalyticsBuilder {
    fn record(&mut self, project_path: &Path, record: Record) {
        let analytics = &mut self.analytics;
        match record {
            Record::Phase {
                ts, phase, edge, ..
            } => self.edges.push((ts, phase, edge)),
            Record::Usage {
                session_id,
                tokens,
                backend,
                input,
                input_fresh,
                output,
                cache_read,
                cache_write,
                ..
            } => {
                if let Some(session_id) = &session_id
                    && !analytics.counted_sessions.insert(session_id.clone())
                {
                    return;
                }
                let recorded = usage_breakdown(input, input_fresh, output, cache_read, cache_write);
                let breakdown = recorded.or_else(|| {
                    session_id.as_deref().and_then(|session_id| {
                        transcript_breakdown(
                            project_path,
                            session_id,
                            backend.as_deref().unwrap_or("claude"),
                        )
                    })
                });
                match breakdown {
                    Some(breakdown) => analytics.breakdown.add(&breakdown),
                    None => analytics.unsplit_tokens += tokens,
                }
            }
        }
    }

    fn finish(self) -> TaskAnalytics {
        let Self {
            mut analytics,
            mut edges,
        } = self;
        edges.sort_by_key(|edge| edge.0);
        let mut open_waiting = None;
        for (ts, phase, edge) in edges {
            match (phase, edge) {
                (Phase::Running, Edge::Enter) => {
                    analytics.runs += 1;
                    analytics.running_since = Some(ts);
                }
                (Phase::Running, Edge::Exit) => {
                    if let Some(start) = analytics.running_since.take() {
                        analytics.running_seconds += (ts - start).num_seconds().max(0);
                    }
                }
                (Phase::Waiting, Edge::Enter) => open_waiting = Some(ts),
                (Phase::Waiting, Edge::Exit) => {
                    if let Some(start) = open_waiting.take() {
                        analytics.waiting_seconds += (ts - start).num_seconds().max(0);
                    }
                }
                _ => {}
            }
        }
        analytics
    }
}

/// Recover the token split of a closed session recorded before splits were
/// stored, from its transcript if it is still on disk. A closed session's
/// transcript never changes, so the answer is memoized per process — the
/// detail view reloads on every board change.
fn transcript_breakdown(
    project_path: &Path,
    session_id: &str,
    backend: &str,
) -> Option<TokenBreakdown> {
    use std::sync::{Mutex, OnceLock};
    type Cache = Mutex<HashMap<(PathBuf, String), Option<TokenBreakdown>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let key = (project_path.to_path_buf(), session_id.to_string());
    let cache = CACHE.get_or_init(Default::default);
    if let Some(found) = cache.lock().ok().and_then(|map| map.get(&key).copied()) {
        return found;
    }
    let found = telemetry::read_session_progress(project_path, session_id, backend).breakdown;
    if let Ok(mut map) = cache.lock() {
        map.insert(key, found);
    }
    found
}

/// Reporting time window. "This month"/"This week" reset on the calendar
/// boundary (the 1st, and Monday) rather than a rolling 30/7 days, matching
/// the task description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    AllTime,
    Month,
    Week,
}

impl Window {
    const ALL: [Window; 3] = [Window::AllTime, Window::Month, Window::Week];

    fn label(self) -> &'static str {
        match self {
            Window::AllTime => "All time",
            Window::Month => "This month",
            Window::Week => "This week",
        }
    }

    /// Start of the window, or `None` for `AllTime` (no lower bound).
    fn start(self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        match self {
            Window::AllTime => None,
            Window::Month => NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
                .and_then(|date| date.and_hms_opt(0, 0, 0)),
            Window::Week => {
                let days_from_monday = now.weekday().num_days_from_monday() as i64;
                let today = NaiveDate::from_ymd_opt(now.year(), now.month(), now.day())?;
                (today - chrono::Duration::days(days_from_monday)).and_hms_opt(0, 0, 0)
            }
        }
    }

    fn includes(self, ts: NaiveDateTime, now: NaiveDateTime) -> bool {
        self.start(now).is_none_or(|start| ts >= start)
    }
}

/// Provider of a model id — the segment before its first slash
/// (`openai/gpt-5.5` → `openai`, `zai/glm-4.7` → `zai`). Derived at report
/// time, never stored: nothing new goes into the events file. A bare model id
/// (`opus`) has no provider and groups under `unknown` downstream.
fn model_provider(model: Option<&str>) -> Option<String> {
    model
        .and_then(|id| id.split_once('/'))
        .map(|(provider, _)| provider.to_string())
        .filter(|provider| !provider.is_empty())
}

/// Sum `value` grouped by `key` (missing key becomes `"unknown"`), sorted by
/// descending value then ascending key for a stable tie order.
fn group_sum(pairs: impl Iterator<Item = (Option<String>, i64)>) -> Vec<(String, i64)> {
    let mut totals: HashMap<String, i64> = HashMap::new();
    for (key, value) in pairs {
        *totals
            .entry(key.unwrap_or_else(|| "unknown".to_string()))
            .or_insert(0) += value;
    }
    let mut rows: Vec<(String, i64)> = totals.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

/// Distinct task count grouped by `key`.
fn group_task_count(pairs: impl Iterator<Item = (Option<String>, String)>) -> Vec<(String, i64)> {
    let mut sets: HashMap<String, HashSet<String>> = HashMap::new();
    for (key, task_id) in pairs {
        sets.entry(key.unwrap_or_else(|| "unknown".to_string()))
            .or_default()
            .insert(task_id);
    }
    let mut rows: Vec<(String, i64)> = sets
        .into_iter()
        .map(|(k, set)| (k, set.len() as i64))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

/// Average `value` per distinct task id, grouped by `key`.
fn group_average_f64(
    pairs: impl Iterator<Item = (Option<String>, String, f64)>,
) -> Vec<(String, f64)> {
    let mut sums: HashMap<String, f64> = HashMap::new();
    let mut tasks: HashMap<String, HashSet<String>> = HashMap::new();
    for (key, task_id, value) in pairs {
        let key = key.unwrap_or_else(|| "unknown".to_string());
        *sums.entry(key.clone()).or_insert(0.0) += value;
        tasks.entry(key).or_default().insert(task_id);
    }
    let mut rows: Vec<(String, f64)> = sums
        .into_iter()
        .map(|(key, sum)| {
            let count = tasks.get(&key).map_or(1, HashSet::len).max(1);
            (key, sum / count as f64)
        })
        .collect();
    rows.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    rows
}

/// Average `value` per distinct task id, grouped by `key`.
fn group_average_per_task(
    pairs: impl Iterator<Item = (Option<String>, String, i64)>,
) -> Vec<(String, f64)> {
    let mut sums: HashMap<String, i64> = HashMap::new();
    let mut tasks: HashMap<String, HashSet<String>> = HashMap::new();
    for (key, task_id, value) in pairs {
        let key = key.unwrap_or_else(|| "unknown".to_string());
        *sums.entry(key.clone()).or_insert(0) += value;
        tasks.entry(key).or_default().insert(task_id);
    }
    let mut rows: Vec<(String, f64)> = sums
        .into_iter()
        .map(|(key, sum)| {
            let count = tasks.get(&key).map_or(1, HashSet::len).max(1);
            (key, sum as f64 / count as f64)
        })
        .collect();
    rows.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    rows
}

/// Total elapsed time across `spans` counting overlapping spans only once —
/// two tasks running at the same moment do not double the clock. Interval
/// union by sweeping sorted, merged intervals.
fn union_seconds(spans: &[&Span]) -> i64 {
    let mut intervals: Vec<(NaiveDateTime, NaiveDateTime)> =
        spans.iter().map(|span| (span.start, span.end)).collect();
    intervals.sort_by_key(|interval| interval.0);
    let mut total = 0i64;
    let mut current: Option<(NaiveDateTime, NaiveDateTime)> = None;
    for (start, end) in intervals {
        current = match current {
            None => Some((start, end)),
            Some((cur_start, cur_end)) if start > cur_end => {
                total += (cur_end - cur_start).num_seconds().max(0);
                Some((start, end))
            }
            Some((cur_start, cur_end)) => Some((cur_start, cur_end.max(end))),
        };
    }
    if let Some((start, end)) = current {
        total += (end - start).num_seconds().max(0);
    }
    total
}

/// Cross-project distinct task key: a bare task id could collide between two
/// different projects (each numbers its own `TASK-NNN` from 1), so anything
/// aggregating task identity *across* projects qualifies it first.
fn project_task_key(project: &str, task_id: &str) -> String {
    format!("{project}\u{0}{task_id}")
}

fn format_tokens(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let grouped: String = grouped.chars().rev().collect();
    if n < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

fn format_avg_tokens(avg: f64) -> String {
    format_tokens(avg.round() as i64)
}

fn format_avg_span(avg: f64) -> String {
    format_span(avg.round() as i64)
}

fn push_top_table(out: &mut String, title: &str, rows: &[(String, String)]) {
    out.push_str(title);
    out.push('\n');
    if rows.is_empty() {
        out.push_str("  (no data)\n");
        return;
    }
    for (name, value) in rows {
        out.push_str(&format!("  {name:<28} {value}\n"));
    }
}

fn limited<T>(rows: &[T], limit: Option<usize>) -> &[T] {
    match limit {
        Some(n) => &rows[..rows.len().min(n)],
        None => rows,
    }
}

fn fmt_rows(
    rows: &[(String, i64)],
    fmt: impl Fn(i64) -> String,
    limit: Option<usize>,
) -> Vec<(String, String)> {
    limited(rows, limit)
        .iter()
        .map(|(k, v)| (k.clone(), fmt(*v)))
        .collect()
}

fn fmt_avg_rows(
    rows: &[(String, f64)],
    fmt: impl Fn(f64) -> String,
    limit: Option<usize>,
) -> Vec<(String, String)> {
    limited(rows, limit)
        .iter()
        .map(|(k, v)| (k.clone(), fmt(*v)))
        .collect()
}

/// Model breakdowns are capped at 10 entries; everything else (backends,
/// projects) is shown in full — both per the task description.
const MODEL_TOP_N: usize = 10;

/// Table row caps for one report: `None` renders the default report (full
/// tables, models at [`MODEL_TOP_N`]); `Some(n)` renders the compact report
/// with every breakdown capped at `n` rows (models additionally at
/// `MODEL_TOP_N`, whichever is smaller).
fn table_caps(cap: Option<usize>) -> (Option<usize>, Option<usize>) {
    match cap {
        Some(n) => (Some(n), Some(n.min(MODEL_TOP_N))),
        None => (None, Some(MODEL_TOP_N)),
    }
}

/// One usage record, or a sum of them, split the way the report prints it.
/// `tokens` is the historical total (what `Total tokens` has always summed).
/// `mixed` is the tokens-only part of that total. `eq` is fresh-input
/// equivalent; `usd` is set only when every contributing record had a price.
#[derive(Debug, Clone, Copy)]
struct TokenTotals {
    tokens: i64,
    fresh: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    mixed: i64,
    eq: f64,
    usd: UsdSum,
}

impl Default for TokenTotals {
    fn default() -> Self {
        Self {
            tokens: 0,
            fresh: 0,
            cache_read: 0,
            cache_write: 0,
            output: 0,
            mixed: 0,
            eq: 0.0,
            usd: UsdSum::Unset,
        }
    }
}

/// Dollar sum that becomes unavailable as soon as one record has no price.
#[derive(Debug, Clone, Copy, PartialEq)]
enum UsdSum {
    Unset,
    Total(f64),
    Missing,
}

impl TokenTotals {
    fn add(&mut self, other: &TokenTotals) {
        self.tokens += other.tokens;
        self.fresh += other.fresh;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.mixed += other.mixed;
        self.eq += other.eq;
        self.usd = merge_usd(self.usd, other.usd);
    }

    fn usd_total(&self) -> Option<f64> {
        match self.usd {
            UsdSum::Total(value) => Some(value),
            UsdSum::Unset | UsdSum::Missing => None,
        }
    }
}

fn merge_usd(left: UsdSum, right: UsdSum) -> UsdSum {
    match (left, right) {
        (UsdSum::Missing, _) | (_, UsdSum::Missing) => UsdSum::Missing,
        (UsdSum::Unset, other) | (other, UsdSum::Unset) => other,
        (UsdSum::Total(left), UsdSum::Total(right)) => UsdSum::Total(left + right),
    }
}

fn usage_totals(usage: &Usage, costs: &TokenCosts) -> TokenTotals {
    let weights = costs.weights_for(usage.backend.as_deref(), usage.model.as_deref());
    match &usage.breakdown {
        Some(breakdown) => {
            let eq = weights.equivalent(breakdown);
            TokenTotals {
                tokens: usage.tokens,
                fresh: breakdown.input,
                cache_read: breakdown.cache_read,
                cache_write: breakdown.cache_write,
                output: breakdown.output,
                mixed: 0,
                eq,
                usd: usd_part(weights, eq),
            }
        }
        None => TokenTotals {
            tokens: usage.tokens,
            mixed: usage.tokens,
            eq: usage.tokens as f64,
            usd: usd_part(weights, usage.tokens as f64),
            ..TokenTotals::default()
        },
    }
}

fn usd_part(weights: CostWeights, fresh_equivalent: f64) -> UsdSum {
    match weights.usd(fresh_equivalent) {
        Some(value) => UsdSum::Total(value),
        None => UsdSum::Missing,
    }
}

fn group_totals(
    pairs: impl Iterator<Item = (Option<String>, TokenTotals)>,
) -> Vec<(String, TokenTotals)> {
    let mut totals: HashMap<String, TokenTotals> = HashMap::new();
    for (key, value) in pairs {
        totals
            .entry(key.unwrap_or_else(|| "unknown".to_string()))
            .or_default()
            .add(&value);
    }
    let mut rows: Vec<(String, TokenTotals)> = totals.into_iter().collect();
    rows.sort_by(|a, b| b.1.tokens.cmp(&a.1.tokens).then_with(|| a.0.cmp(&b.0)));
    rows
}

fn tokens_window(
    entries: &[(String, ProjectStats)],
    window: Window,
    now: NaiveDateTime,
    cap: Option<usize>,
    costs: &TokenCosts,
) -> String {
    // Default report: full backend/provider/project tables, models capped at
    // MODEL_TOP_N. Compact report: every table capped at `cap`.
    let (other_cap, model_cap) = table_caps(cap);
    let mut total = TokenTotals::default();
    let mut by_backend: Vec<(Option<String>, TokenTotals)> = Vec::new();
    let mut by_provider: Vec<(Option<String>, TokenTotals)> = Vec::new();
    let mut by_model: Vec<(Option<String>, TokenTotals)> = Vec::new();
    let mut by_project: Vec<(String, TokenTotals)> = Vec::new();
    for (project, stats) in entries {
        let mut project_total = TokenTotals::default();
        for usage in &stats.usage {
            if !window.includes(usage.ts, now) {
                continue;
            }
            let row = usage_totals(usage, costs);
            total.add(&row);
            project_total.add(&row);
            by_backend.push((usage.backend.clone(), row));
            by_provider.push((model_provider(usage.model.as_deref()), row));
            by_model.push((usage.model.clone(), row));
        }
        if project_total.tokens > 0 {
            by_project.push((project.clone(), project_total));
        }
    }
    by_project.sort_by(|a, b| b.1.tokens.cmp(&a.1.tokens).then_with(|| a.0.cmp(&b.0)));

    let mut out = format!("-- {} --\n", window.label());
    out.push_str(&format!("Total tokens: {}\n", format_tokens(total.tokens)));
    push_split_line(&mut out, "Fresh input:", total.fresh);
    push_split_line(&mut out, "Cache read:", total.cache_read);
    push_split_line(&mut out, "Cache write:", total.cache_write);
    push_split_line(&mut out, "Output:", total.output);
    push_split_line(&mut out, "Mixed (no split):", total.mixed);
    out.push_str(&cost_line(&total));
    out.push('\n');
    push_token_table(
        &mut out,
        "Top backends:",
        &group_totals(by_backend.into_iter()),
        other_cap,
    );
    out.push('\n');
    push_token_table(
        &mut out,
        "Top providers:",
        &group_totals(by_provider.into_iter()),
        other_cap,
    );
    out.push('\n');
    push_token_table(
        &mut out,
        "Top models (max 10):",
        &group_totals(by_model.into_iter()),
        model_cap,
    );
    out.push('\n');
    push_token_table(&mut out, "Top projects:", &by_project, other_cap);
    out
}

fn push_split_line(out: &mut String, label: &str, value: i64) {
    out.push_str(&format!("  {label:<22}{:>16}\n", format_tokens(value)));
}

fn cost_line(total: &TokenTotals) -> String {
    let mut line = format!(
        "  {:<22}{} fresh-input tok",
        "Cost equivalent:",
        format_eq(total.eq)
    );
    if let Some(usd) = total.usd_total() {
        line.push_str(&format!("  [≈ {}]", format_usd(usd)));
    }
    if total.mixed > 0 {
        line.push_str(&format!(
            "  (incl. {} mixed tok at 1.0×)",
            format_tokens(total.mixed)
        ));
    }
    line.push('\n');
    line
}

fn format_eq(eq: f64) -> String {
    format!("~{}", format_tokens(eq.round() as i64))
}

fn format_usd(usd: f64) -> String {
    if usd.abs() >= 0.01 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.4}")
    }
}

/// Compact column figure: `842`, `12.4k`, `35.4M`, `1.79G`.
fn format_compact(n: i64) -> String {
    let negative = n < 0;
    let n = n.unsigned_abs();
    let body = match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        _ => format!("{:.2}G", n as f64 / 1_000_000_000.0),
    };
    if negative { format!("-{body}") } else { body }
}

fn push_token_table(
    out: &mut String,
    title: &str,
    rows: &[(String, TokenTotals)],
    limit: Option<usize>,
) {
    out.push_str(title);
    out.push('\n');
    let rows = limited(rows, limit);
    if rows.is_empty() {
        out.push_str("  (no data)\n");
        return;
    }
    push_token_row(
        out,
        "name",
        ["tokens", "fresh", "cache_r", "cache_w", "output", "eq"],
    );
    for (name, totals) in rows {
        push_token_row(
            out,
            name,
            [
                &format_compact(totals.tokens),
                &format_compact(totals.fresh),
                &format_compact(totals.cache_read),
                &format_compact(totals.cache_write),
                &format_compact(totals.output),
                &format_compact(totals.eq.round() as i64),
            ],
        );
    }
}

fn push_token_row(out: &mut String, name: &str, cols: [&str; 6]) {
    out.push_str(&format!(
        "  {:<28} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}\n",
        name, cols[0], cols[1], cols[2], cols[3], cols[4], cols[5]
    ));
}

fn time_window(
    entries: &[(String, ProjectStats)],
    window: Window,
    now: NaiveDateTime,
    cap: Option<usize>,
) -> String {
    let (other_cap, model_cap) = table_caps(cap);
    let mut running: Vec<Span> = Vec::new();
    let mut by_backend: Vec<(Option<String>, i64)> = Vec::new();
    let mut by_provider: Vec<(Option<String>, i64)> = Vec::new();
    let mut by_model: Vec<(Option<String>, i64)> = Vec::new();
    let mut by_project: Vec<(String, i64)> = Vec::new();
    for (project, stats) in entries {
        let mut project_total = 0i64;
        for span in &stats.spans {
            if span.phase != Phase::Running || !window.includes(span.end, now) {
                continue;
            }
            let seconds = span.seconds();
            project_total += seconds;
            by_backend.push((span.backend.clone(), seconds));
            by_provider.push((model_provider(span.model.as_deref()), seconds));
            by_model.push((span.model.clone(), seconds));
            running.push(span.clone());
        }
        if project_total > 0 {
            by_project.push((project.clone(), project_total));
        }
    }
    by_project.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let refs: Vec<&Span> = running.iter().collect();
    let total = union_seconds(&refs);

    let mut out = format!("-- {} --\n", window.label());
    out.push_str(&format!(
        "Total time worked: {} (wall-clock — concurrent tasks are not double-counted)\n\n",
        format_span(total)
    ));
    push_top_table(
        &mut out,
        "Top backends:",
        &fmt_rows(&group_sum(by_backend.into_iter()), format_span, other_cap),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top providers:",
        &fmt_rows(&group_sum(by_provider.into_iter()), format_span, other_cap),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models (max 10):",
        &fmt_rows(&group_sum(by_model.into_iter()), format_span, model_cap),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top projects:",
        &fmt_rows(&by_project, format_span, other_cap),
    );
    out
}

/// The "Задачи" section: task counts and per-task averages, plus the four
/// cumulative (parallel tasks summed, not deduplicated) phase totals. All
/// time only — the task description does not split this section by month/week.
fn tasks_section(
    entries: &[(String, ProjectStats)],
    cap: Option<usize>,
    costs: &TokenCosts,
) -> String {
    let (other_cap, model_cap) = table_caps(cap);
    let mut task_ids: HashSet<String> = HashSet::new();
    let mut by_backend_count: Vec<(Option<String>, String)> = Vec::new();
    let mut by_provider_count: Vec<(Option<String>, String)> = Vec::new();
    let mut by_model_count: Vec<(Option<String>, String)> = Vec::new();
    let mut by_backend_tokens: Vec<(Option<String>, String, i64)> = Vec::new();
    let mut by_provider_tokens: Vec<(Option<String>, String, i64)> = Vec::new();
    let mut by_model_tokens: Vec<(Option<String>, String, i64)> = Vec::new();
    let mut by_backend_eq: Vec<(Option<String>, String, f64)> = Vec::new();
    let mut by_provider_eq: Vec<(Option<String>, String, f64)> = Vec::new();
    let mut by_model_eq: Vec<(Option<String>, String, f64)> = Vec::new();
    let mut total_tokens = 0i64;
    let mut total_eq = 0.0f64;

    for (project, stats) in entries {
        for usage in &stats.usage {
            let key = project_task_key(project, &usage.task_id);
            task_ids.insert(key.clone());
            let provider = model_provider(usage.model.as_deref());
            let eq = usage_totals(usage, costs).eq;
            by_backend_count.push((usage.backend.clone(), key.clone()));
            by_provider_count.push((provider.clone(), key.clone()));
            by_model_count.push((usage.model.clone(), key.clone()));
            by_backend_tokens.push((usage.backend.clone(), key.clone(), usage.tokens));
            by_provider_tokens.push((provider.clone(), key.clone(), usage.tokens));
            by_model_tokens.push((usage.model.clone(), key.clone(), usage.tokens));
            by_backend_eq.push((usage.backend.clone(), key.clone(), eq));
            by_provider_eq.push((provider, key.clone(), eq));
            by_model_eq.push((usage.model.clone(), key, eq));
            total_tokens += usage.tokens;
            total_eq += eq;
        }
    }

    let mut by_backend_time: Vec<(Option<String>, String, i64)> = Vec::new();
    let mut by_provider_time: Vec<(Option<String>, String, i64)> = Vec::new();
    let mut by_model_time: Vec<(Option<String>, String, i64)> = Vec::new();
    let mut running_task_ids: HashSet<String> = HashSet::new();
    let mut cumulative_running = 0i64;
    let mut cumulative_waiting = 0i64;
    let mut cumulative_retry = 0i64;
    let mut cumulative_queued = 0i64;

    for (project, stats) in entries {
        for span in &stats.spans {
            let seconds = span.seconds();
            match span.phase {
                Phase::Running => {
                    cumulative_running += seconds;
                    let key = project_task_key(project, &span.task_id);
                    running_task_ids.insert(key.clone());
                    by_backend_time.push((span.backend.clone(), key.clone(), seconds));
                    by_provider_time.push((
                        model_provider(span.model.as_deref()),
                        key.clone(),
                        seconds,
                    ));
                    by_model_time.push((span.model.clone(), key, seconds));
                }
                Phase::Waiting => cumulative_waiting += seconds,
                Phase::Retry => cumulative_retry += seconds,
                Phase::Queued => cumulative_queued += seconds,
            }
        }
    }

    let task_count = task_ids.len().max(running_task_ids.len()) as i64;
    let avg_tokens = if task_ids.is_empty() {
        0.0
    } else {
        total_tokens as f64 / task_ids.len() as f64
    };
    let avg_eq = if task_ids.is_empty() {
        0.0
    } else {
        total_eq / task_ids.len() as f64
    };
    let avg_time = if running_task_ids.is_empty() {
        0.0
    } else {
        cumulative_running as f64 / running_task_ids.len() as f64
    };

    let mut out = String::from("=== TASKS (all time) ===\n\n");
    out.push_str(&format!("Total tasks completed: {task_count}\n\n"));
    push_top_table(
        &mut out,
        "Top backends by task count:",
        &fmt_rows(
            &group_task_count(by_backend_count.into_iter()),
            |n| n.to_string(),
            other_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top providers by task count:",
        &fmt_rows(
            &group_task_count(by_provider_count.into_iter()),
            |n| n.to_string(),
            other_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models by task count (max 10):",
        &fmt_rows(
            &group_task_count(by_model_count.into_iter()),
            |n| n.to_string(),
            model_cap,
        ),
    );
    out.push('\n');
    out.push_str(&format!(
        "Average tokens per task: {}\n",
        format_avg_tokens(avg_tokens)
    ));
    out.push_str(&format!(
        "Average cost-equivalent per task: {}\n",
        format_avg_tokens(avg_eq)
    ));
    out.push_str(&format!(
        "Average time per task: {}\n\n",
        format_avg_span(avg_time)
    ));
    push_top_table(
        &mut out,
        "Top backends by avg tokens/task:",
        &fmt_avg_rows(
            &group_average_per_task(by_backend_tokens.into_iter()),
            format_avg_tokens,
            other_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top providers by avg tokens/task:",
        &fmt_avg_rows(
            &group_average_per_task(by_provider_tokens.into_iter()),
            format_avg_tokens,
            other_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models by avg tokens/task (max 10):",
        &fmt_avg_rows(
            &group_average_per_task(by_model_tokens.into_iter()),
            format_avg_tokens,
            model_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top backends by avg cost-eq/task:",
        &fmt_avg_rows(
            &group_average_f64(by_backend_eq.into_iter()),
            format_avg_tokens,
            other_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top providers by avg cost-eq/task:",
        &fmt_avg_rows(
            &group_average_f64(by_provider_eq.into_iter()),
            format_avg_tokens,
            other_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models by avg cost-eq/task (max 10):",
        &fmt_avg_rows(
            &group_average_f64(by_model_eq.into_iter()),
            format_avg_tokens,
            model_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top backends by avg time/task:",
        &fmt_avg_rows(
            &group_average_per_task(by_backend_time.into_iter()),
            format_avg_span,
            other_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top providers by avg time/task:",
        &fmt_avg_rows(
            &group_average_per_task(by_provider_time.into_iter()),
            format_avg_span,
            other_cap,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models by avg time/task (max 10):",
        &fmt_avg_rows(
            &group_average_per_task(by_model_time.into_iter()),
            format_avg_span,
            model_cap,
        ),
    );
    out.push('\n');
    out.push_str(&format!(
        "Total running time (cumulative): {}\n",
        format_span(cumulative_running)
    ));
    out.push_str(&format!(
        "Total waiting/pause time (cumulative): {}\n",
        format_span(cumulative_waiting)
    ));
    out.push_str(&format!(
        "Total retry-wait time (cumulative): {}\n",
        format_span(cumulative_retry)
    ));
    out.push_str(&format!(
        "Total queue-wait time (cumulative): {}\n",
        format_span(cumulative_queued)
    ));
    out
}

fn count_label(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

fn truncate_reason(reason: &str) -> String {
    let mut out = String::new();
    for (index, ch) in reason.chars().enumerate() {
        if index == DIAGNOSTIC_REASON_CHARS {
            break;
        }
        out.push(ch);
    }
    out
}

/// Data-quality block, or an empty string when every project read cleanly.
/// Affected paths are always listed. Only per-line samples are capped at
/// `samples` (`0` keeps the counts and paths and shows no sample lines).
fn diagnostics_section(entries: &[(String, ProjectStats)], samples: usize) -> String {
    let affected: Vec<_> = entries
        .iter()
        .filter(|(_, stats)| !stats.source.is_clean())
        .collect();
    if affected.is_empty() {
        return String::new();
    }
    let malformed: usize = entries
        .iter()
        .map(|(_, stats)| stats.source.skipped_lines)
        .sum();
    let unreadable = entries
        .iter()
        .filter(|(_, stats)| stats.source.read_error.is_some())
        .count();
    let mut out = String::from("=== DATA QUALITY: REPORT INCOMPLETE ===\n\n");
    out.push_str(&format!(
        "Totals below include only readable records: {}, {} ({} of {} projects affected).\n\n",
        count_label(
            malformed,
            "malformed line skipped",
            "malformed lines skipped"
        ),
        count_label(
            unreadable,
            "events file unreadable",
            "events files unreadable"
        ),
        affected.len(),
        entries.len()
    ));
    for (name, stats) in affected {
        out.push_str(&format!("{name}: {}\n", stats.source.path.display()));
        if let Some(err) = &stats.source.read_error {
            out.push_str(&format!("  unreadable: {err}\n"));
        }
        if stats.source.read_error.is_none() {
            out.push_str(&format!(
                "  {}, {}\n",
                count_label(
                    stats.source.skipped_lines,
                    "malformed line skipped",
                    "malformed lines skipped"
                ),
                count_label(stats.source.loaded, "record loaded", "records loaded")
            ));
        }
        let shown = stats.source.samples.len().min(samples);
        for issue in stats.source.samples.iter().take(shown) {
            out.push_str(&format!(
                "  line {}: {}\n",
                issue.line,
                truncate_reason(&issue.reason)
            ));
        }
        let hidden = stats.source.skipped_lines.saturating_sub(shown);
        if samples > 0 && hidden > 0 {
            out.push_str(&format!(
                "  … {} not shown (stats_issue_samples = {samples})\n",
                count_label(hidden, "more malformed line", "more malformed lines")
            ));
        }
    }
    out.push('\n');
    out
}

/// One-line summary for stderr and the TUI status, when any source is dirty.
pub fn incomplete_summary(entries: &[(String, ProjectStats)]) -> Option<String> {
    if entries.iter().all(|(_, stats)| stats.source.is_clean()) {
        return None;
    }
    let malformed: usize = entries
        .iter()
        .map(|(_, stats)| stats.source.skipped_lines)
        .sum();
    let unreadable = entries
        .iter()
        .filter(|(_, stats)| stats.source.read_error.is_some())
        .count();
    Some(format!(
        "stats report incomplete: {}, {}",
        count_label(
            malformed,
            "malformed line skipped",
            "malformed lines skipped"
        ),
        count_label(
            unreadable,
            "events file unreadable",
            "events files unreadable"
        )
    ))
}

/// Render the full report (tokens, time, tasks) for a set of `(project name,
/// stats)` pairs, as plain text for the TUI text pager / `kanban stats`.
/// `diagnostic_samples` caps per-file sample lines in the data-quality section.
pub fn render_report(
    entries: &[(String, ProjectStats)],
    now: NaiveDateTime,
    diagnostic_samples: usize,
    costs: &TokenCosts,
) -> String {
    let mut out = diagnostics_section(entries, diagnostic_samples);
    out.push_str("=== TOKENS ===\n\n");
    for window in Window::ALL {
        out.push_str(&tokens_window(entries, window, now, None, costs));
        out.push('\n');
    }
    out.push_str("=== TIME ===\n\n");
    for window in Window::ALL {
        out.push_str(&time_window(entries, window, now, None));
        out.push('\n');
    }
    out.push_str(&tasks_section(entries, None, costs));
    out
}

/// Render the compact report for `kanban stats --compact`: the same sections
/// and totals, but every breakdown table is capped at `top_n` rows. The full
/// report stays available — the CLI writes it to a file next to this output.
/// The data-quality section uses the same text and sample cap as the full report.
pub fn render_compact_report(
    entries: &[(String, ProjectStats)],
    now: NaiveDateTime,
    top_n: usize,
    diagnostic_samples: usize,
    costs: &TokenCosts,
) -> String {
    let mut out = format!(
        "Compact report: at most {top_n} rows per breakdown (models max 10); omitted rows remain in the full report.\n\n"
    );
    out.push_str(&diagnostics_section(entries, diagnostic_samples));
    out.push_str("=== TOKENS (compact) ===\n\n");
    for window in Window::ALL {
        out.push_str(&tokens_window(entries, window, now, Some(top_n), costs));
        out.push('\n');
    }
    out.push_str("=== TIME (compact) ===\n\n");
    for window in Window::ALL {
        out.push_str(&time_window(entries, window, now, Some(top_n)));
        out.push('\n');
    }
    out.push_str(&tasks_section(entries, Some(top_n), costs));
    out
}

/// Load every registered project's stats — the shared entry collection for
/// [`render_report`] / [`render_compact_report`].
pub fn collect_store_entries() -> Result<Vec<(String, ProjectStats)>> {
    collect_store_entries_from(&ProjectStore::open()?)
}

/// Same collection as [`collect_store_entries`], from an already-open store.
/// The TUI Projects screen passes the store it is showing.
pub fn collect_store_entries_from(store: &ProjectStore) -> Result<Vec<(String, ProjectStats)>> {
    let entries: Vec<(String, ProjectStats)> = store
        .list()?
        .into_iter()
        .map(|project| {
            let stats = load(&project.data_root);
            (project.name, stats)
        })
        .collect();
    Ok(entries)
}

/// Load every registered project's stats and render the combined report.
pub fn collect_store_report(
    now: NaiveDateTime,
    diagnostic_samples: usize,
    costs: &TokenCosts,
) -> Result<String> {
    Ok(render_report(
        &collect_store_entries()?,
        now,
        diagnostic_samples,
        costs,
    ))
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    fn tags(backend: &str, model: &str) -> Tags {
        Tags {
            backend: Some(backend.to_string()),
            model: Some(model.to_string()),
            effort: None,
            agent: None,
        }
    }

    #[test]
    fn tasks_analytics_matches_individual_totals_and_transcript_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let since = at(2026, 2, 1, 0, 0);
        let tasks = vec![
            ("TASK-1".to_string(), since),
            ("TASK-2".to_string(), at(2026, 2, 1, 10, 0)),
            ("TASK-empty".to_string(), since),
        ];
        let lines = [
            // Recycled-id history must not leak into this task.
            r#"{"kind":"usage","ts":"2026-01-01T10:00:00","task_id":"TASK-1","session_id":"old","tokens":999}"#,
            // Deliberately out-of-order edges still pair by timestamp.
            r#"{"kind":"phase","ts":"2026-02-01T10:10:00","task_id":"TASK-1","phase":"running","edge":"exit"}"#,
            r#"{"kind":"phase","ts":"2026-02-01T10:00:00","task_id":"TASK-2","phase":"running","edge":"enter"}"#,
            r#"{"kind":"phase","ts":"2026-02-01T10:00:00","task_id":"TASK-1","phase":"running","edge":"enter"}"#,
            r#"{"kind":"phase","ts":"2026-02-01T10:15:00","task_id":"TASK-1","phase":"waiting","edge":"exit"}"#,
            r#"{"kind":"phase","ts":"2026-02-01T10:10:00","task_id":"TASK-1","phase":"waiting","edge":"enter"}"#,
            r#"{"kind":"usage","ts":"2026-02-01T10:10:00","task_id":"TASK-1","session_id":"recorded","tokens":15,"input":10,"output":5}"#,
            // Duplicate usage is ignored even when its values differ.
            r#"{"kind":"usage","ts":"2026-02-01T10:11:00","task_id":"TASK-1","session_id":"recorded","tokens":999,"input":999,"output":999}"#,
            r#"{"kind":"usage","ts":"2026-02-01T10:12:00","task_id":"TASK-1","session_id":"legacy","tokens":115}"#,
            r#"{"kind":"usage","ts":"2026-02-01T10:13:00","task_id":"TASK-1","session_id":"missing","tokens":300}"#,
            // Session deduplication is per task, not per batch.
            r#"{"kind":"usage","ts":"2026-02-01T10:10:00","task_id":"TASK-2","session_id":"recorded","tokens":22,"input":20,"output":2}"#,
            r#"{"kind":"usage","ts":"2026-02-01T09:59:00","task_id":"TASK-2","session_id":"early","tokens":800}"#,
            r#"{"kind":"usage","ts":"2026-02-01T10:11:00","task_id":"TASK-2","tokens":7}"#,
            r#"{"kind":"usage","ts":"2026-02-01T10:12:00","task_id":"TASK-2","tokens":8}"#,
            r#"{"kind":"usage","ts":"2026-02-01T10:00:00","task_id":"unrequested","tokens":999}"#,
        ];
        let path = events_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, lines.join("\n")).unwrap();
        let logs = dir.path().join(".kanban/logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("legacy.transcript.jsonl"),
            concat!(
                r#"{"type":"result","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":90,"cache_creation_input_tokens":10}}"#,
                "\n",
            ),
        )
        .unwrap();

        let batch = tasks_analytics(dir.path(), &tasks);
        assert_eq!(batch.len(), tasks.len());
        for (id, since) in &tasks {
            assert_eq!(batch[id], task_analytics(dir.path(), id, *since));
        }
        let first = &batch["TASK-1"];
        assert_eq!(first.runs, 1);
        assert_eq!(first.running_seconds, 600);
        assert_eq!(first.waiting_seconds, 300);
        assert_eq!(first.running_since, None);
        assert_eq!(first.unsplit_tokens, 300);
        assert_eq!(first.counted_sessions.len(), 3);
        assert_eq!(
            first.breakdown,
            TokenBreakdown {
                input: 20,
                output: 10,
                cache_read: 90,
                cache_write: 10,
            }
        );
        let second = &batch["TASK-2"];
        assert_eq!(second.runs, 1);
        assert_eq!(second.running_since, Some(at(2026, 2, 1, 10, 0)));
        assert_eq!(second.unsplit_tokens, 15);
        assert_eq!(second.breakdown.input, 20);
        assert_eq!(batch["TASK-empty"], TaskAnalytics::default());
        assert!(tasks_analytics(dir.path(), &[]).is_empty());
    }

    #[test]
    fn task_analytics_sums_every_run_and_skips_a_recycled_ids_past() {
        let dir = tempfile::tempdir().unwrap();
        let ts = |s: &str| timefmt::parse(s).unwrap();
        let tag = Tags::default();
        let lines = [
            // An abandoned task that held TASK-7 before it was recycled.
            r#"{"kind":"phase","ts":"2026-01-01T09:00:00","task_id":"TASK-7","phase":"running","edge":"enter"}"#,
            r#"{"kind":"phase","ts":"2026-01-01T09:30:00","task_id":"TASK-7","phase":"running","edge":"exit"}"#,
            r#"{"kind":"usage","ts":"2026-01-01T09:30:00","task_id":"TASK-7","session_id":"ses-old","tokens":99,"input":99,"output":1}"#,
            // First run, then a declared wait.
            r#"{"kind":"phase","ts":"2026-02-01T10:00:00","task_id":"TASK-7","phase":"running","edge":"enter"}"#,
            r#"{"kind":"phase","ts":"2026-02-01T10:10:00","task_id":"TASK-7","phase":"running","edge":"exit"}"#,
            r#"{"kind":"phase","ts":"2026-02-01T10:10:00","task_id":"TASK-7","phase":"waiting","edge":"enter"}"#,
            r#"{"kind":"phase","ts":"2026-02-01T10:15:00","task_id":"TASK-7","phase":"waiting","edge":"exit"}"#,
            r#"{"kind":"usage","ts":"2026-02-01T10:15:00","task_id":"TASK-7","session_id":"ses-a","tokens":500,"input":1000,"output":100,"cache_read":800,"cache_write":100}"#,
            // Review pass with a legacy record carrying no split.
            r#"{"kind":"phase","ts":"2026-02-01T11:00:00","task_id":"TASK-7","phase":"running","edge":"enter"}"#,
            r#"{"kind":"phase","ts":"2026-02-01T11:05:00","task_id":"TASK-7","phase":"running","edge":"exit"}"#,
            r#"{"kind":"usage","ts":"2026-02-01T11:05:00","task_id":"TASK-7","session_id":"ses-b","tokens":300}"#,
            // Another task, and a still-running restart.
            r#"{"kind":"phase","ts":"2026-02-01T11:00:00","task_id":"TASK-8","phase":"running","edge":"enter"}"#,
            r#"{"kind":"phase","ts":"2026-02-01T12:00:00","task_id":"TASK-7","phase":"running","edge":"enter"}"#,
        ];
        let path = events_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        // A fresh record goes through the recorder with its split.
        record_usage(
            dir.path(),
            "TASK-7",
            "ses-c",
            10,
            Some(&TokenBreakdown {
                input: 200,
                output: 20,
                cache_read: 200,
                cache_write: 0,
            }),
            &tag,
        );

        let analytics = task_analytics(dir.path(), "TASK-7", ts("2026-01-15T00:00:00"));
        assert_eq!(analytics.runs, 3);
        assert_eq!(analytics.running_seconds, 15 * 60);
        assert_eq!(analytics.waiting_seconds, 5 * 60);
        assert_eq!(analytics.running_since, Some(ts("2026-02-01T12:00:00")));
        assert_eq!(
            analytics.breakdown,
            TokenBreakdown {
                input: 300,
                output: 120,
                cache_read: 1000,
                cache_write: 100,
            }
        );
        assert_eq!(analytics.unsplit_tokens, 300);
        assert!(analytics.counted_sessions.contains("ses-c"));
        assert!(!analytics.counted_sessions.contains("ses-old"));
    }

    #[test]
    fn record_and_load_round_trips_through_the_events_file() {
        let dir = tempfile::tempdir().unwrap();
        record_enter(
            dir.path(),
            "TASK-001",
            Phase::Running,
            &tags("claude", "anthropic/opus"),
        );
        record_exit(dir.path(), "TASK-001", Phase::Running);
        record_usage(
            dir.path(),
            "TASK-001",
            "ses-1",
            1500,
            None,
            &tags("claude", "anthropic/opus"),
        );
        // A zero/negative reading is dropped rather than recorded as a free run.
        record_usage(
            dir.path(),
            "TASK-001",
            "ses-1",
            0,
            None,
            &tags("claude", "anthropic/opus"),
        );

        let stats = load(dir.path());
        assert_eq!(stats.spans.len(), 1);
        assert_eq!(stats.spans[0].phase, Phase::Running);
        assert_eq!(stats.spans[0].backend.as_deref(), Some("claude"));
        assert_eq!(stats.usage.len(), 1);
        assert_eq!(stats.usage[0].tokens, 1500);
    }

    #[test]
    fn missing_events_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let stats = load(dir.path());
        assert!(stats.spans.is_empty() && stats.usage.is_empty());
    }

    #[test]
    fn pairing_ignores_stray_exit_and_dangling_enter() {
        let records = vec![
            Record::Phase {
                ts: at(2026, 6, 1, 10, 0),
                task_id: "TASK-1".to_string(),
                phase: Phase::Queued,
                edge: Edge::Exit,
                backend: None,
                model: None,
                effort: None,
                agent: None,
            },
            Record::Phase {
                ts: at(2026, 6, 1, 11, 0),
                task_id: "TASK-1".to_string(),
                phase: Phase::Running,
                edge: Edge::Enter,
                backend: Some("claude".to_string()),
                model: None,
                effort: None,
                agent: None,
            },
        ];
        let stats = pair_records(records);
        // The stray Exit has no open Enter (dropped) and the dangling Enter has
        // no matching Exit yet (an in-progress run is not counted).
        assert!(stats.spans.is_empty());
    }

    #[test]
    fn pairing_matches_enter_exit_per_task_and_phase() {
        let records = vec![
            Record::Phase {
                ts: at(2026, 6, 1, 10, 0),
                task_id: "TASK-1".to_string(),
                phase: Phase::Running,
                edge: Edge::Enter,
                backend: Some("claude".to_string()),
                model: Some("anthropic/opus".to_string()),
                effort: None,
                agent: None,
            },
            Record::Phase {
                ts: at(2026, 6, 1, 10, 30),
                task_id: "TASK-1".to_string(),
                phase: Phase::Running,
                edge: Edge::Exit,
                backend: None,
                model: None,
                effort: None,
                agent: None,
            },
        ];
        let stats = pair_records(records);
        assert_eq!(stats.spans.len(), 1);
        assert_eq!(stats.spans[0].seconds(), 1800);
        assert_eq!(stats.spans[0].backend.as_deref(), Some("claude"));
        assert_eq!(stats.spans[0].model.as_deref(), Some("anthropic/opus"));
    }

    #[test]
    fn union_seconds_dedupes_overlapping_spans() {
        let span = |s: NaiveDateTime, e: NaiveDateTime| Span {
            task_id: "T".to_string(),
            phase: Phase::Running,
            start: s,
            end: e,
            backend: None,
            model: None,
            effort: None,
            agent: None,
        };
        let a = span(at(2026, 6, 1, 10, 0), at(2026, 6, 1, 11, 0)); // 1h
        let b = span(at(2026, 6, 1, 10, 30), at(2026, 6, 1, 11, 30)); // overlaps a by 30m
        let c = span(at(2026, 6, 1, 12, 0), at(2026, 6, 1, 12, 30)); // disjoint, 30m
        assert_eq!(union_seconds(&[&a, &b, &c]), 3600 + 1800 + 1800);
        // Cumulative (plain sum) would instead be 3600+3600+1800 — the two must differ.
        let cumulative: i64 = [&a, &b, &c].iter().map(|s| s.seconds()).sum();
        assert_ne!(cumulative, union_seconds(&[&a, &b, &c]));
    }

    #[test]
    fn window_boundaries_reset_on_the_first_and_on_monday() {
        // Wednesday 2026-06-17.
        let now = at(2026, 6, 17, 15, 0);
        assert_eq!(Window::Month.start(now), Some(at(2026, 6, 1, 0, 0)));
        // Monday of that week is 2026-06-15.
        assert_eq!(Window::Week.start(now), Some(at(2026, 6, 15, 0, 0)));
        assert!(Window::AllTime.includes(at(2020, 1, 1, 0, 0), now));
        assert!(!Window::Month.includes(at(2026, 5, 31, 23, 59), now));
        assert!(Window::Month.includes(at(2026, 6, 1, 0, 0), now));
    }

    #[test]
    fn group_sum_orders_descending_with_stable_ties() {
        let rows = group_sum(
            vec![
                (Some("b".to_string()), 10),
                (Some("a".to_string()), 10),
                (Some("c".to_string()), 20),
                (None, 5),
            ]
            .into_iter(),
        );
        assert_eq!(
            rows,
            vec![
                ("c".to_string(), 20),
                ("a".to_string(), 10),
                ("b".to_string(), 10),
                ("unknown".to_string(), 5),
            ]
        );
    }

    #[test]
    fn group_average_per_task_divides_by_distinct_task_count() {
        let rows = group_average_per_task(
            vec![
                (Some("claude".to_string()), "T1".to_string(), 100),
                (Some("claude".to_string()), "T1".to_string(), 50), // second session of T1
                (Some("claude".to_string()), "T2".to_string(), 300),
            ]
            .into_iter(),
        );
        // (100+50+300) tokens over 2 distinct tasks = 225 avg.
        assert_eq!(rows, vec![("claude".to_string(), 225.0)]);
    }

    #[test]
    fn model_provider_is_the_first_slash_segment() {
        assert_eq!(
            model_provider(Some("openai/gpt-5.5")).as_deref(),
            Some("openai")
        );
        assert_eq!(model_provider(Some("zai/glm-4.7")).as_deref(), Some("zai"));
        // Model ids may contain their own slashes; only the first segment is
        // the provider.
        assert_eq!(
            model_provider(Some("openrouter/deepseek/deepseek-r1")).as_deref(),
            Some("openrouter")
        );
        // A bare model id (or empty prefix) has no provider.
        assert_eq!(model_provider(Some("opus")), None);
        assert_eq!(model_provider(Some("/weird")), None);
        assert_eq!(model_provider(None), None);
    }

    #[test]
    fn cross_project_task_ids_do_not_collide() {
        // Two different projects both use TASK-1; a bare-id HashSet would
        // undercount this as a single task.
        let mut stats_a = ProjectStats::default();
        stats_a.usage.push(Usage {
            task_id: "TASK-1".to_string(),
            session_id: None,
            breakdown: None,
            ts: at(2026, 6, 1, 10, 0),
            tokens: 100,
            backend: Some("claude".to_string()),
            model: None,
            effort: None,
            agent: None,
        });
        let mut stats_b = ProjectStats::default();
        stats_b.usage.push(Usage {
            task_id: "TASK-1".to_string(),
            session_id: None,
            breakdown: None,
            ts: at(2026, 6, 1, 10, 0),
            tokens: 200,
            backend: Some("claude".to_string()),
            model: None,
            effort: None,
            agent: None,
        });
        let entries = vec![
            ("proj-a".to_string(), stats_a),
            ("proj-b".to_string(), stats_b),
        ];
        let report = tasks_section(&entries, None, &TokenCosts::builtin());
        assert!(report.contains("Total tasks completed: 2"));
    }

    #[test]
    fn format_tokens_groups_thousands() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1,000");
        assert_eq!(format_tokens(1_234_567), "1,234,567");
        assert_eq!(format_tokens(-1234), "-1,234");
    }

    #[test]
    fn render_report_smoke_test_over_synthetic_data() {
        let dir = tempfile::tempdir().unwrap();
        record_enter(
            dir.path(),
            "TASK-1",
            Phase::Running,
            &tags("claude", "anthropic/opus"),
        );
        record_exit(dir.path(), "TASK-1", Phase::Running);
        record_usage(
            dir.path(),
            "TASK-1",
            "ses-1",
            42,
            None,
            &tags("claude", "anthropic/opus"),
        );
        record_enter(dir.path(), "TASK-1", Phase::Queued, &Tags::default());
        record_exit(dir.path(), "TASK-1", Phase::Queued);
        record_enter(dir.path(), "TASK-1", Phase::Waiting, &Tags::default());
        record_exit(dir.path(), "TASK-1", Phase::Waiting);
        record_enter(dir.path(), "TASK-1", Phase::Retry, &Tags::default());
        record_exit(dir.path(), "TASK-1", Phase::Retry);

        let stats = load(dir.path());
        let entries = vec![("demo".to_string(), stats)];
        let report = render_report(&entries, timefmt::now(), 3, &TokenCosts::builtin());
        assert!(report.contains("=== TOKENS ==="));
        assert!(report.contains("=== TIME ==="));
        assert!(report.contains("=== TASKS (all time) ==="));
        assert!(report.contains("Total tokens: 42"));
        assert!(report.contains("Total tasks completed: 1"));
        // Provider tables in every section, grouped under the derived
        // provider of `anthropic/opus`.
        assert!(report.contains("Top providers:\n  anthropic"));
        assert!(report.contains("Top providers by task count:\n  anthropic"));
        assert!(report.contains("Top providers by avg tokens/task:\n  anthropic"));
        assert!(report.contains("Top providers by avg time/task:\n  anthropic"));
        assert!(!report.contains("DATA QUALITY"));
    }

    fn write_events(dir: &Path, bytes: &[u8]) -> PathBuf {
        let path = events_path(dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn usage_line(task: &str, tokens: i64) -> String {
        format!(
            r#"{{"kind":"usage","ts":"2026-06-01T10:00:00","task_id":"{task}","tokens":{tokens},"backend":"claude","model":"anthropic/opus"}}"#
        )
    }

    fn phase_lines() -> String {
        format!(
            "{}\n{}",
            r#"{"kind":"phase","ts":"2026-06-01T10:00:00","task_id":"TASK-1","phase":"running","edge":"enter","backend":"claude"}"#,
            r#"{"kind":"phase","ts":"2026-06-01T10:10:00","task_id":"TASK-1","phase":"running","edge":"exit"}"#
        )
    }

    #[test]
    fn malformed_line_keeps_valid_records_and_reports_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = usage_line("TASK-1", 40);
        body.push('\n');
        body.push_str("NOT-JSON secret-token\n");
        body.push_str(&phase_lines());
        body.push('\n');
        write_events(dir.path(), body.as_bytes());

        let clean_dir = tempfile::tempdir().unwrap();
        let mut clean = usage_line("TASK-1", 40);
        clean.push('\n');
        clean.push_str(&phase_lines());
        clean.push('\n');
        write_events(clean_dir.path(), clean.as_bytes());

        let dirty = load(dir.path());
        let clean_stats = load(clean_dir.path());
        assert_eq!(dirty.usage, clean_stats.usage);
        assert_eq!(dirty.spans, clean_stats.spans);
        assert_eq!(dirty.usage[0].tokens, 40);
        assert_eq!(dirty.spans.len(), 1);
        assert_eq!(dirty.source.loaded, 3);
        assert_eq!(dirty.source.skipped_lines, 1);
        assert_eq!(dirty.source.samples.len(), 1);
        assert_eq!(dirty.source.samples[0].line, 2);
        assert!(!dirty.source.samples[0].reason.contains("secret-token"));
        assert!(dirty.source.samples[0].reason.contains("expected value"));
        assert!(
            !dirty.source.samples[0].reason.contains("at line"),
            "serde's per-line `line 1` must not contradict the file line: {}",
            dirty.source.samples[0].reason
        );
        assert!(dirty.source.samples[0].reason.ends_with("(column 1)"));
    }

    #[test]
    fn non_utf8_line_does_not_drop_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = usage_line("TASK-1", 7).into_bytes();
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.push(b'\n');
        bytes.extend(usage_line("TASK-2", 9).into_bytes());
        bytes.push(b'\n');
        write_events(dir.path(), &bytes);

        let stats = load(dir.path());
        assert_eq!(stats.source.loaded, 2);
        assert_eq!(stats.source.skipped_lines, 1);
        assert_eq!(stats.usage.len(), 2);
        assert_eq!(stats.usage.iter().map(|u| u.tokens).sum::<i64>(), 16);
        assert_eq!(stats.source.samples[0].line, 2);
        assert_eq!(stats.source.samples[0].reason, "invalid UTF-8");
    }

    #[test]
    fn directory_events_file_is_a_read_error_and_other_projects_stay_intact() {
        let good = tempfile::tempdir().unwrap();
        write_events(
            good.path(),
            format!("{}\n", usage_line("TASK-1", 50)).as_bytes(),
        );
        let bad = tempfile::tempdir().unwrap();
        let bad_path = events_path(bad.path());
        fs::create_dir_all(&bad_path).unwrap();

        let good_stats = load(good.path());
        let bad_stats = load(bad.path());
        assert!(bad_stats.source.read_error.is_some());
        assert!(bad_stats.usage.is_empty());
        assert!(bad_stats.spans.is_empty());
        assert_eq!(good_stats.usage[0].tokens, 50);

        let entries = vec![
            ("good".to_string(), good_stats),
            ("bad".to_string(), bad_stats),
        ];
        let report = render_report(&entries, at(2026, 6, 15, 12, 0), 3, &TokenCosts::builtin());
        assert!(report.contains("Total tokens: 50"));
        assert!(report.contains("unreadable"));
        assert!(report.contains(&events_path(bad.path()).display().to_string()));
        assert!(entries[0].1.source.is_clean());
        assert!(entries[1].1.source.read_error.is_some());
    }

    #[test]
    fn stored_samples_are_capped_but_the_skip_count_stays_exact() {
        let dir = tempfile::tempdir().unwrap();
        let bad = MAX_STORED_LINE_ISSUES + 5;
        let mut body = "{bad\n".repeat(bad);
        body.push_str(&usage_line("TASK-1", 4));
        body.push('\n');
        write_events(dir.path(), body.as_bytes());
        let stats = load(dir.path());
        assert_eq!(stats.source.skipped_lines, bad);
        assert_eq!(stats.source.samples.len(), MAX_STORED_LINE_ISSUES);
        assert_eq!(stats.source.loaded, 1);
        assert_eq!(stats.usage[0].tokens, 4);
        let entries = vec![("demo".to_string(), stats)];
        let report = render_report(&entries, at(2026, 6, 15, 12, 0), 2, &TokenCosts::builtin());
        assert!(report.contains(&format!("… {} more malformed lines not shown", bad - 2)));
    }

    #[test]
    fn missing_file_and_blank_lines_are_clean() {
        let missing = tempfile::tempdir().unwrap();
        let (records, source) = read_records(&events_path(missing.path()));
        assert!(records.is_empty());
        assert!(source.is_clean());
        assert!(source.read_error.is_none());

        let blanks = tempfile::tempdir().unwrap();
        write_events(blanks.path(), b"\n\n  \n\r\n");
        let stats = load(blanks.path());
        assert!(stats.source.is_clean());
        assert_eq!(stats.source.loaded, 0);
        assert!(stats.usage.is_empty());
    }

    #[test]
    fn unterminated_final_garbage_line_is_marked_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = usage_line("TASK-1", 3).into_bytes();
        bytes.push(b'\n');
        bytes.extend_from_slice(br#"{"kind":"usage""#);
        write_events(dir.path(), &bytes);
        let stats = load(dir.path());
        assert_eq!(stats.source.loaded, 1);
        assert_eq!(stats.source.skipped_lines, 1);
        assert_eq!(stats.usage[0].tokens, 3);
        assert!(
            stats.source.samples[0]
                .reason
                .starts_with("incomplete final line (possibly still being written):")
        );
        assert_eq!(stats.source.samples[0].line, 2);
    }

    fn quality_section(report: &str) -> &str {
        let start = report.find("=== DATA QUALITY").expect("section present");
        let rest = &report[start..];
        let end = rest.find("=== TOKENS").expect("tokens follow the section");
        &rest[..end]
    }

    #[test]
    fn clean_reports_omit_the_data_quality_section() {
        let dir = tempfile::tempdir().unwrap();
        write_events(
            dir.path(),
            format!("{}\n", usage_line("TASK-1", 11)).as_bytes(),
        );
        let stats = load(dir.path());
        assert!(stats.source.is_clean());
        let entries = vec![("demo".to_string(), stats)];
        let now = at(2026, 6, 15, 12, 0);
        let report = render_report(&entries, now, 3, &TokenCosts::builtin());
        let compact = render_compact_report(&entries, now, 5, 3, &TokenCosts::builtin());
        assert!(!report.contains("DATA QUALITY"));
        assert!(!compact.contains("DATA QUALITY"));
        assert!(report.starts_with("=== TOKENS ===\n"));
        let mut cleared = entries.clone();
        cleared[0].1.source = SourceDiagnostics::default();
        assert_eq!(
            report,
            render_report(&cleared, now, 3, &TokenCosts::builtin())
        );
        assert_eq!(
            compact,
            render_compact_report(&cleared, now, 5, 3, &TokenCosts::builtin())
        );
        assert!(incomplete_summary(&entries).is_none());
    }

    #[test]
    fn dirty_reports_list_paths_cap_samples_and_share_the_section() {
        let path = PathBuf::from("/abs/path/.kanban/stats/events.jsonl");
        let mut samples = Vec::new();
        for line in [4usize, 9, 12, 20] {
            samples.push(LineIssue {
                line,
                reason: format!("reason {line} {}", "x".repeat(200)),
            });
        }
        let dirty = ProjectStats {
            source: SourceDiagnostics {
                path: path.clone(),
                loaded: 120,
                skipped_lines: 4,
                samples,
                read_error: None,
            },
            ..ProjectStats::default()
        };
        let unreadable = ProjectStats {
            source: SourceDiagnostics {
                path: PathBuf::from("/abs/other/.kanban/stats/events.jsonl"),
                read_error: Some("Permission denied (os error 13)".to_string()),
                ..SourceDiagnostics::default()
            },
            ..ProjectStats::default()
        };
        let clean = ProjectStats::default();
        let entries = vec![
            ("alpha".to_string(), unreadable),
            ("clean".to_string(), clean),
            ("beta".to_string(), dirty),
        ];
        let now = at(2026, 6, 15, 12, 0);
        let report = render_report(&entries, now, 3, &TokenCosts::builtin());
        let compact = render_compact_report(&entries, now, 5, 3, &TokenCosts::builtin());
        assert!(report.find("=== DATA QUALITY").unwrap() < report.find("=== TOKENS ===").unwrap());
        let section = quality_section(&report);
        assert_eq!(section, quality_section(&compact));
        assert!(section.contains(
            "4 malformed lines skipped, 1 events file unreadable (2 of 3 projects affected)"
        ));
        assert!(section.contains("alpha: /abs/other/.kanban/stats/events.jsonl"));
        assert!(section.contains("unreadable: Permission denied (os error 13)"));
        assert!(section.contains(&format!("beta: {}", path.display())));
        assert!(section.contains("4 malformed lines skipped, 120 records loaded"));
        assert!(section.contains("line 4:"));
        assert!(section.contains("line 12:"));
        assert!(!section.contains("line 20:"));
        assert!(section.contains("… 1 more malformed line not shown (stats_issue_samples = 3)"));
        let rendered_reason = section
            .lines()
            .find(|line| line.contains("line 4:"))
            .unwrap();
        let reason = rendered_reason.split_once(": ").unwrap().1;
        assert!(reason.chars().count() <= DIAGNOSTIC_REASON_CHARS);

        let counts_only = render_report(&entries, now, 0, &TokenCosts::builtin());
        let counts = quality_section(&counts_only);
        assert!(counts.contains(&format!("beta: {}", path.display())));
        assert!(counts.contains("4 malformed lines skipped, 120 records loaded"));
        assert!(
            !counts
                .lines()
                .any(|line| line.trim_start().starts_with("line "))
        );
        assert!(!counts.contains("more malformed"));
        assert_eq!(
            incomplete_summary(&entries).as_deref(),
            Some("stats report incomplete: 4 malformed lines skipped, 1 events file unreadable")
        );
    }

    #[test]
    fn record_usage_keeps_the_whole_prompt_in_input_and_fresh_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        record_usage(
            dir.path(),
            "TASK-1",
            "ses-1",
            50,
            Some(&TokenBreakdown {
                input: 200,
                output: 20,
                cache_read: 800,
                cache_write: 10,
            }),
            &tags("claude", "anthropic/opus"),
        );
        let raw = fs::read_to_string(events_path(dir.path())).unwrap();
        let line = raw.lines().next().unwrap();
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["input"], 1010);
        assert_eq!(value["input_fresh"], 200);
        #[derive(Deserialize)]
        struct OldView {
            input: i64,
        }
        let old: OldView = serde_json::from_str(line).unwrap();
        assert_eq!(old.input, 1010);
        let loaded = load(dir.path());
        let breakdown = loaded.usage[0].breakdown.unwrap();
        assert_eq!(breakdown.input, 200);
        assert_eq!(breakdown.prompt_total(), 1010);
    }

    #[test]
    fn fresh_cache_and_mixed_aggregate_without_moving_the_token_total() {
        let dir = tempfile::tempdir().unwrap();
        let lines = [
            // New record: input is the whole prompt, input_fresh is uncached.
            r#"{"kind":"usage","ts":"2026-06-01T10:00:00","task_id":"TASK-1","tokens":1110,"backend":"claude","model":"anthropic/claude","input":1010,"input_fresh":200,"output":20,"cache_read":800,"cache_write":10}"#,
            // Old split: fresh is recovered as input - cache_read - cache_write.
            r#"{"kind":"usage","ts":"2026-06-01T11:00:00","task_id":"TASK-1","tokens":500,"backend":"codex","model":"openai/gpt","input":1000,"output":100,"cache_read":800,"cache_write":100}"#,
            // Tokens only: mixed, never added to the fresh or cache columns.
            r#"{"kind":"usage","ts":"2026-06-01T12:00:00","task_id":"TASK-2","tokens":300,"backend":"claude","model":"opus"}"#,
        ];
        write_events(dir.path(), format!("{}\n", lines.join("\n")).as_bytes());
        let stats = load(dir.path());
        assert_eq!(
            stats.usage.iter().map(|usage| usage.tokens).sum::<i64>(),
            1910
        );
        let split: Vec<_> = stats
            .usage
            .iter()
            .filter_map(|usage| usage.breakdown.as_ref())
            .collect();
        assert_eq!(split.len(), 2);
        let fresh: i64 = split.iter().map(|row| row.input).sum();
        let cache_read: i64 = split.iter().map(|row| row.cache_read).sum();
        let cache_write: i64 = split.iter().map(|row| row.cache_write).sum();
        let output: i64 = split.iter().map(|row| row.output).sum();
        assert_eq!(
            (fresh, cache_read, cache_write, output),
            (300, 1600, 110, 120)
        );
        assert_eq!(fresh + cache_read + cache_write, 1010 + 1000);
        assert!(stats.usage[2].breakdown.is_none());

        let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
            r#"
            anthropic:
              usd_per_mtok_input: 3.0
            openai:
              usd_per_mtok_input: 2.0
            default:
              usd_per_mtok_input: 1.0
            "#,
        )
        .unwrap();
        let costs = TokenCosts::from_mapping(raw.as_mapping().unwrap());
        let entries = vec![("demo".to_string(), stats)];
        let now = at(2026, 6, 1, 18, 0);
        let report = render_report(&entries, now, 3, &costs);
        let compact = render_compact_report(&entries, now, 5, 3, &costs);
        for body in [&report, &compact] {
            assert!(body.contains("Total tokens: 1,910"), "{body}");
            assert!(body.contains("Fresh input:"), "{body}");
            assert!(body.contains("300"), "{body}");
            assert!(body.contains("Cache read:"), "{body}");
            assert!(body.contains("1,600"), "{body}");
            assert!(body.contains("Cache write:"), "{body}");
            assert!(body.contains("Mixed (no split):"), "{body}");
            assert!(body.contains("Cost equivalent:"), "{body}");
            assert!(body.contains("incl. 300 mixed tok at 1.0×"), "{body}");
            assert!(body.contains("[≈ $"), "{body}");
            assert!(
                body.contains("name") && body.contains("cache_r") && body.contains("eq"),
                "{body}"
            );
        }
        // June 1 2026 is a Monday, so every window in the report includes the rows.
        assert_eq!(report.matches("Mixed (no split):").count(), 3);

        let priced = costs.weights_for(Some("claude"), Some("anthropic/claude"));
        let eq = priced.equivalent(&TokenBreakdown {
            input: 200,
            output: 20,
            cache_read: 800,
            cache_write: 10,
        });
        assert!((eq - 392.5).abs() < 1e-9, "{eq}");
    }

    #[test]
    fn a_mixed_only_window_names_the_mixed_bucket() {
        let mut stats = ProjectStats::default();
        stats.usage.push(Usage {
            task_id: "TASK-1".to_string(),
            session_id: None,
            breakdown: None,
            ts: at(2026, 6, 1, 10, 0),
            tokens: 40,
            backend: Some("claude".to_string()),
            model: None,
            effort: None,
            agent: None,
        });
        let entries = vec![("demo".to_string(), stats)];
        let report = render_report(&entries, at(2026, 6, 15, 12, 0), 3, &TokenCosts::builtin());
        assert!(report.contains("Mixed (no split):"));
        assert!(report.contains("40"));
        assert!(report.contains("incl. 40 mixed tok at 1.0×"));
        assert!(!report.contains("[≈ $"));
    }
}
