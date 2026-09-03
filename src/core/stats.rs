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
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::core::error::Result;
use crate::core::limits::format_span;
use crate::core::models::Task;
use crate::core::project::ProjectStore;
use crate::core::timefmt;

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
#[derive(Debug, Clone, Default)]
pub struct Tags {
    pub backend: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
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
    },
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
        },
    );
}

fn load_records(project_path: &Path) -> Vec<Record> {
    let Ok(text) = fs::read_to_string(events_path(project_path)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .collect()
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
    pub ts: NaiveDateTime,
    pub tokens: i64,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectStats {
    pub spans: Vec<Span>,
    pub usage: Vec<Usage>,
}

/// Load and reconstruct one project's stats from its events file.
pub fn load(project_path: &Path) -> ProjectStats {
    pair_records(load_records(project_path))
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
                tokens,
                backend,
                model,
                effort,
                agent,
                ..
            } => usage.push(Usage {
                task_id,
                ts,
                tokens,
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
    ProjectStats { spans, usage }
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

fn tokens_window(entries: &[(String, ProjectStats)], window: Window, now: NaiveDateTime) -> String {
    let mut total = 0i64;
    let mut by_backend: Vec<(Option<String>, i64)> = Vec::new();
    let mut by_model: Vec<(Option<String>, i64)> = Vec::new();
    let mut by_project: Vec<(String, i64)> = Vec::new();
    for (project, stats) in entries {
        let mut project_total = 0i64;
        for usage in &stats.usage {
            if !window.includes(usage.ts, now) {
                continue;
            }
            total += usage.tokens;
            project_total += usage.tokens;
            by_backend.push((usage.backend.clone(), usage.tokens));
            by_model.push((usage.model.clone(), usage.tokens));
        }
        if project_total > 0 {
            by_project.push((project.clone(), project_total));
        }
    }
    by_project.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut out = format!("-- {} --\n", window.label());
    out.push_str(&format!("Total tokens: {}\n\n", format_tokens(total)));
    push_top_table(
        &mut out,
        "Top backends:",
        &fmt_rows(&group_sum(by_backend.into_iter()), format_tokens, None),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models (max 10):",
        &fmt_rows(
            &group_sum(by_model.into_iter()),
            format_tokens,
            Some(MODEL_TOP_N),
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top projects:",
        &fmt_rows(&by_project, format_tokens, None),
    );
    out
}

fn time_window(entries: &[(String, ProjectStats)], window: Window, now: NaiveDateTime) -> String {
    let mut running: Vec<Span> = Vec::new();
    let mut by_backend: Vec<(Option<String>, i64)> = Vec::new();
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
        &fmt_rows(&group_sum(by_backend.into_iter()), format_span, None),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models (max 10):",
        &fmt_rows(
            &group_sum(by_model.into_iter()),
            format_span,
            Some(MODEL_TOP_N),
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top projects:",
        &fmt_rows(&by_project, format_span, None),
    );
    out
}

/// The "Задачи" section: task counts and per-task averages, plus the four
/// cumulative (parallel tasks summed, not deduplicated) phase totals. All
/// time only — the task description does not split this section by month/week.
fn tasks_section(entries: &[(String, ProjectStats)]) -> String {
    let mut task_ids: HashSet<String> = HashSet::new();
    let mut by_backend_count: Vec<(Option<String>, String)> = Vec::new();
    let mut by_model_count: Vec<(Option<String>, String)> = Vec::new();
    let mut by_backend_tokens: Vec<(Option<String>, String, i64)> = Vec::new();
    let mut by_model_tokens: Vec<(Option<String>, String, i64)> = Vec::new();
    let mut total_tokens = 0i64;

    for (project, stats) in entries {
        for usage in &stats.usage {
            let key = project_task_key(project, &usage.task_id);
            task_ids.insert(key.clone());
            by_backend_count.push((usage.backend.clone(), key.clone()));
            by_model_count.push((usage.model.clone(), key.clone()));
            by_backend_tokens.push((usage.backend.clone(), key.clone(), usage.tokens));
            by_model_tokens.push((usage.model.clone(), key, usage.tokens));
            total_tokens += usage.tokens;
        }
    }

    let mut by_backend_time: Vec<(Option<String>, String, i64)> = Vec::new();
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
            None,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models by task count (max 10):",
        &fmt_rows(
            &group_task_count(by_model_count.into_iter()),
            |n| n.to_string(),
            Some(MODEL_TOP_N),
        ),
    );
    out.push('\n');
    out.push_str(&format!(
        "Average tokens per task: {}\n",
        format_avg_tokens(avg_tokens)
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
            None,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models by avg tokens/task (max 10):",
        &fmt_avg_rows(
            &group_average_per_task(by_model_tokens.into_iter()),
            format_avg_tokens,
            Some(MODEL_TOP_N),
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top backends by avg time/task:",
        &fmt_avg_rows(
            &group_average_per_task(by_backend_time.into_iter()),
            format_avg_span,
            None,
        ),
    );
    out.push('\n');
    push_top_table(
        &mut out,
        "Top models by avg time/task (max 10):",
        &fmt_avg_rows(
            &group_average_per_task(by_model_time.into_iter()),
            format_avg_span,
            Some(MODEL_TOP_N),
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

/// Render the full report (tokens, time, tasks) for a set of `(project name,
/// stats)` pairs, as plain text for the TUI text pager / `kanban stats`.
pub fn render_report(entries: &[(String, ProjectStats)], now: NaiveDateTime) -> String {
    let mut out = String::from("=== TOKENS ===\n\n");
    for window in Window::ALL {
        out.push_str(&tokens_window(entries, window, now));
        out.push('\n');
    }
    out.push_str("=== TIME ===\n\n");
    for window in Window::ALL {
        out.push_str(&time_window(entries, window, now));
        out.push('\n');
    }
    out.push_str(&tasks_section(entries));
    out
}

/// Load every registered project's stats and render the combined report —
/// the single entry point the TUI Stats window and `kanban stats` both call.
pub fn collect_store_report(now: NaiveDateTime) -> Result<String> {
    let store = ProjectStore::open()?;
    let entries: Vec<(String, ProjectStats)> = store
        .list()?
        .into_iter()
        .map(|project| {
            let stats = load(&project.data_root);
            (project.name, stats)
        })
        .collect();
    Ok(render_report(&entries, now))
}

#[cfg(test)]
mod tests {
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
            &tags("claude", "anthropic/opus"),
        );
        // A zero/negative reading is dropped rather than recorded as a free run.
        record_usage(
            dir.path(),
            "TASK-001",
            "ses-1",
            0,
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
    fn cross_project_task_ids_do_not_collide() {
        // Two different projects both use TASK-1; a bare-id HashSet would
        // undercount this as a single task.
        let mut stats_a = ProjectStats::default();
        stats_a.usage.push(Usage {
            task_id: "TASK-1".to_string(),
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
        let report = tasks_section(&entries);
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
        let report = render_report(&entries, timefmt::now());
        assert!(report.contains("=== TOKENS ==="));
        assert!(report.contains("=== TIME ==="));
        assert!(report.contains("=== TASKS (all time) ==="));
        assert!(report.contains("Total tokens: 42"));
        assert!(report.contains("Total tasks completed: 1"));
    }
}
