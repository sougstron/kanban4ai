//! How much of each provider's subscription limits one task consumed.
//!
//! Providers only ever report a window's *total* spend, never who spent it,
//! so the per-task share is reconstructed from two append-only logs:
//!
//! - **Limit history** (`<store>/limits-history.jsonl`): every time a window's
//!   used percentage (or reset time) changes, one reading is appended — from
//!   the cached snapshot ([`crate::core::limits`] `store`) and from the claude
//!   bridge, which headless claude runs feed on every API call.
//! - **Token samples** (`.kanban/stats/samples.jsonl`, per project): the
//!   cumulative, cost-weighted token count of each agent session — a zero at
//!   session start, one on every heartbeat while the transcript keeps
//!   growing, and the final tally at close — tagged with the session's
//!   provider and role (executor/designer/reviewer).
//!
//! [`attribute`] then walks consecutive readings of each window: the increase
//! between them is split across every session (in every registered project)
//! on that provider in proportion to the weighted tokens each spent during
//! that interval, read off a linear interpolation of its samples. Parallel
//! tasks and uneven consumption are therefore handled by construction.
//!
//! Accepted approximations: provider usage outside kanban that overlaps a
//! kanban run is attributed to the runs; usage before the first reading or
//! after the last one in a window is lost; a rolling (tick-regenerating)
//! quota only counts its increases; and the token weights below are list-price
//! ratios, not each provider's real limit formula. Only sessions started
//! after this module existed have samples.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::limits::LimitWindow;
use crate::core::project::{ProjectStore, store_root};
use crate::core::telemetry::{SessionProgress, TokenBreakdown};

/// Relative cost of each token kind, so a cache-read-heavy session does not
/// look as expensive as one generating the same count of output tokens. Only
/// the ratios matter: the weights are compared between sessions of the same
/// provider, never converted into a percentage on their own.
const WEIGHT_UNCACHED_INPUT: f64 = 1.0;
const WEIGHT_CACHE_WRITE: f64 = 1.25;
const WEIGHT_CACHE_READ: f64 = 0.1;
const WEIGHT_OUTPUT: f64 = 5.0;

/// Two readings whose reset times differ by less than this are the same
/// period: some providers recompute `resets_at` slightly on every response.
const SAME_PERIOD_SLACK_SECS: i64 = 600;

/// Bytes of the history file scanned for the previous reading of a window.
const HISTORY_TAIL_BYTES: u64 = 64 * 1024;

/// One reading of one provider window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitReading {
    /// Unix seconds when the provider reported it.
    pub at: i64,
    pub provider: String,
    pub label: String,
    /// Percentage of the window already used.
    pub used: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rolling: bool,
}

/// Cumulative weighted tokens of one session at one moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenSample {
    /// Unix seconds.
    pub at: i64,
    pub task_id: String,
    pub session_id: String,
    pub weight: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// The share of one provider window a task consumed under one role.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitShare {
    pub provider: String,
    pub label: String,
    pub role: String,
    /// Percentage points of the window.
    pub percent: f64,
}

/// Cost-weighted size of a token split (see the `WEIGHT_*` constants).
pub fn weigh(breakdown: &TokenBreakdown) -> f64 {
    let uncached = (breakdown.input - breakdown.cache_read - breakdown.cache_write).max(0);
    uncached as f64 * WEIGHT_UNCACHED_INPUT
        + breakdown.cache_write as f64 * WEIGHT_CACHE_WRITE
        + breakdown.cache_read as f64 * WEIGHT_CACHE_READ
        + breakdown.output as f64 * WEIGHT_OUTPUT
}

/// Weighted tokens of a run so far: the split when the transcript has one,
/// otherwise the bare total.
pub fn weigh_progress(progress: &SessionProgress) -> Option<f64> {
    progress
        .breakdown
        .as_ref()
        .map(weigh)
        .or(progress.tokens.map(|tokens| tokens as f64))
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn append_line(path: &Path, value: &impl Serialize) {
    let Ok(line) = serde_json::to_string(value) else {
        return;
    };
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

fn read_lines<T: for<'de> Deserialize<'de>>(text: &str) -> Vec<T> {
    text.lines()
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// limit history
// ---------------------------------------------------------------------------

fn history_path() -> Option<PathBuf> {
    store_root()
        .ok()
        .map(|root| root.join("limits-history.jsonl"))
}

/// Append the windows of one provider reading that differ from the last
/// recorded reading of the same window. Best effort: a failed write must
/// never disturb the limits row that called in. Spend tallies (no ceiling)
/// are skipped.
pub fn record_readings(provider: &str, windows: &[LimitWindow], observed_at: i64) {
    // Unit tests drive the limits cache with fixtures; none may reach the
    // developer's real history file.
    if cfg!(test) {
        return;
    }
    let Some(path) = history_path() else { return };
    let last = last_readings(&path);
    for window in windows.iter().filter(|window| window.spent_usd.is_none()) {
        let reading = LimitReading {
            at: observed_at,
            provider: provider.to_string(),
            label: window.label.clone(),
            used: (100.0 - window.remaining_percent).clamp(0.0, 100.0),
            resets_at: window.resets_at,
            rolling: window.rolling,
        };
        // A reset time that only drifts (an idle rolling window recomputed
        // on every poll) is not news either.
        let unchanged = last
            .get(&(reading.provider.clone(), reading.label.clone()))
            .is_some_and(|previous| {
                previous.at >= reading.at
                    || ((previous.used - reading.used).abs() < 1e-6
                        && match (previous.resets_at, reading.resets_at) {
                            (Some(x), Some(y)) => (x - y).abs() <= SAME_PERIOD_SLACK_SECS,
                            (x, y) => x == y,
                        })
            });
        if !unchanged {
            append_line(&path, &reading);
        }
    }
}

/// The newest reading of each `(provider, label)` in the history file's tail.
fn last_readings(path: &Path) -> HashMap<(String, String), LimitReading> {
    let mut text = String::new();
    if let Ok(mut file) = File::open(path) {
        let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        let start = len.saturating_sub(HISTORY_TAIL_BYTES);
        if file.seek(SeekFrom::Start(start)).is_ok() {
            let mut bytes = Vec::new();
            let _ = file.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    read_lines::<LimitReading>(&text)
        .into_iter()
        .map(|reading| ((reading.provider.clone(), reading.label.clone()), reading))
        .collect()
}

fn load_history() -> Vec<LimitReading> {
    history_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|text| read_lines(&text))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// token samples
// ---------------------------------------------------------------------------

fn samples_path(project_path: &Path) -> PathBuf {
    project_path
        .join(".kanban")
        .join("stats")
        .join("samples.jsonl")
}

/// Append one session's cumulative weighted tokens. `provider`/`role` are
/// what the session was launched as; [`attribute`] takes the first sample
/// that carries them, so later samples may omit nothing harmful.
pub fn record_sample(
    project_path: &Path,
    task_id: &str,
    session_id: &str,
    weight: f64,
    provider: Option<&str>,
    role: Option<&str>,
) {
    append_line(
        &samples_path(project_path),
        &TokenSample {
            at: now_secs(),
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            weight,
            provider: provider.map(str::to_string),
            role: role.map(str::to_string),
        },
    );
}

fn load_samples(project_path: &Path) -> Vec<TokenSample> {
    fs::read_to_string(samples_path(project_path))
        .map(|text| read_lines(&text))
        .unwrap_or_default()
}

/// The task's share of every provider window it touched, for the TUI
/// Analytics panel. `since` (Unix seconds, the task's creation) keeps a
/// recycled id from inheriting an abandoned task's sessions. Sessions of
/// every registered project count toward the split, since they all draw on
/// the same subscriptions.
pub fn task_limit_shares(project_path: &Path, task_id: &str, since: i64) -> Vec<LimitShare> {
    let own = load_samples(project_path);
    let sessions: HashSet<String> = own
        .iter()
        .filter(|sample| sample.task_id == task_id && sample.at >= since)
        .map(|sample| sample.session_id.clone())
        .collect();
    if sessions.is_empty() {
        return Vec::new();
    }
    let own_root = fs::canonicalize(project_path).unwrap_or_else(|_| project_path.to_path_buf());
    let mut samples = own;
    if let Ok(projects) = ProjectStore::open().and_then(|store| store.list()) {
        for project in projects {
            let root = fs::canonicalize(&project.data_root).unwrap_or(project.data_root);
            if root != own_root {
                samples.extend(load_samples(&root));
            }
        }
    }
    attribute(&sessions, &samples, &load_history())
}

// ---------------------------------------------------------------------------
// attribution
// ---------------------------------------------------------------------------

struct Curve {
    provider: Option<String>,
    role: Option<String>,
    points: Vec<(i64, f64)>,
}

impl Curve {
    /// Cumulative weight at `t`: linear between samples, flat outside them.
    fn at(&self, t: i64) -> f64 {
        let Some(&(first_at, first)) = self.points.first() else {
            return 0.0;
        };
        if t <= first_at {
            return first;
        }
        for pair in self.points.windows(2) {
            let ((a_at, a), (b_at, b)) = (pair[0], pair[1]);
            if t <= b_at {
                if b_at == a_at {
                    return b;
                }
                return a + (b - a) * (t - a_at) as f64 / (b_at - a_at) as f64;
            }
        }
        self.points.last().map_or(0.0, |point| point.1)
    }

    fn spent(&self, from: i64, to: i64) -> f64 {
        (self.at(to) - self.at(from)).max(0.0)
    }
}

/// Split every observed increase of every window among the sessions that
/// spent tokens on its provider meanwhile, and sum what `task_sessions`
/// received, per provider, window and role. Pure: see [`task_limit_shares`]
/// for the I/O.
pub fn attribute(
    task_sessions: &HashSet<String>,
    samples: &[TokenSample],
    readings: &[LimitReading],
) -> Vec<LimitShare> {
    let mut curves: HashMap<&str, Curve> = HashMap::new();
    let mut sorted: Vec<&TokenSample> = samples.iter().collect();
    sorted.sort_by_key(|sample| sample.at);
    for sample in sorted {
        let curve = curves
            .entry(sample.session_id.as_str())
            .or_insert_with(|| Curve {
                provider: None,
                role: None,
                points: Vec::new(),
            });
        if curve.provider.is_none() {
            curve.provider = sample.provider.clone();
        }
        if curve.role.is_none() {
            curve.role = sample.role.clone();
        }
        // Cumulative counts never shrink; a lower re-read is noise.
        let floor = curve.points.last().map_or(0.0, |point| point.1);
        curve.points.push((sample.at, sample.weight.max(floor)));
    }

    let providers: HashSet<&str> = task_sessions
        .iter()
        .filter_map(|session| curves.get(session.as_str())?.provider.as_deref())
        .collect();
    let mut series: HashMap<(&str, &str), Vec<&LimitReading>> = HashMap::new();
    for reading in readings
        .iter()
        .filter(|reading| providers.contains(reading.provider.as_str()))
    {
        series
            .entry((reading.provider.as_str(), reading.label.as_str()))
            .or_default()
            .push(reading);
    }

    let mut totals: HashMap<(String, String, String), f64> = HashMap::new();
    for ((provider, label), mut list) in series {
        list.sort_by_key(|reading| reading.at);
        for pair in list.windows(2) {
            let Some((from, to, delta)) = increase(pair[0], pair[1]) else {
                continue;
            };
            let on_provider = curves
                .iter()
                .filter(|(_, curve)| curve.provider.as_deref() == Some(provider));
            let mut total = 0.0;
            let mut mine: Vec<(&str, f64)> = Vec::new();
            for (session, curve) in on_provider {
                let spent = curve.spent(from, to);
                total += spent;
                if spent > 0.0 && task_sessions.contains(*session) {
                    mine.push((curve.role.as_deref().unwrap_or("executor"), spent));
                }
            }
            if total <= 0.0 {
                continue;
            }
            for (role, spent) in mine {
                *totals
                    .entry((provider.to_string(), label.to_string(), role.to_string()))
                    .or_default() += delta * spent / total;
            }
        }
    }

    let mut shares: Vec<LimitShare> = totals
        .into_iter()
        .map(|((provider, label, role), percent)| LimitShare {
            provider,
            label,
            role,
            percent,
        })
        .collect();
    shares.sort_by(|a, b| {
        (&a.provider, &a.label, role_rank(&a.role)).cmp(&(
            &b.provider,
            &b.label,
            role_rank(&b.role),
        ))
    });
    shares
}

/// Display order of roles: the run's own order of events.
pub fn role_rank(role: &str) -> u8 {
    match role {
        "designer" => 0,
        "executor" => 1,
        "reviewer" => 2,
        _ => 3,
    }
}

/// The interval and percentage points a window grew by between two
/// consecutive readings, or `None` when it did not grow. A reset between
/// them means the later reading was spent entirely after the reset.
fn increase(a: &LimitReading, b: &LimitReading) -> Option<(i64, i64, f64)> {
    let same_period = a.rolling
        || b.rolling
        || match (a.resets_at, b.resets_at) {
            (Some(x), Some(y)) => (x - y).abs() <= SAME_PERIOD_SLACK_SECS || b.at < x,
            _ => true,
        };
    let (from, delta) = if same_period {
        (a.at, b.used - a.used)
    } else {
        (a.resets_at.unwrap_or(a.at).clamp(a.at, b.at), b.used)
    };
    (delta > 0.0 && b.at > from).then_some((from, b.at, delta))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(at: i64, used: f64, resets_at: i64) -> LimitReading {
        LimitReading {
            at,
            provider: "claude".into(),
            label: "5h".into(),
            used,
            resets_at: Some(resets_at),
            rolling: false,
        }
    }

    fn sample(session: &str, at: i64, weight: f64, role: Option<&str>) -> TokenSample {
        TokenSample {
            at,
            task_id: format!("T-{session}"),
            session_id: session.into(),
            weight,
            provider: role.map(|_| "claude".to_string()),
            role: role.map(str::to_string),
        }
    }

    fn one(name: &str) -> HashSet<String> {
        HashSet::from([name.to_string()])
    }

    #[test]
    fn parallel_sessions_split_by_tokens_spent_in_each_interval() {
        // a spends 300 in [0,100] while b spends 100; then only b spends.
        let samples = vec![
            sample("a", 0, 0.0, Some("executor")),
            sample("a", 100, 300.0, None),
            sample("b", 0, 0.0, Some("reviewer")),
            sample("b", 100, 100.0, None),
            sample("b", 200, 600.0, None),
        ];
        let readings = vec![
            reading(0, 10.0, 10_000),
            reading(100, 18.0, 10_000),
            reading(200, 23.0, 10_000),
        ];
        let a = attribute(&one("a"), &samples, &readings);
        assert_eq!(a.len(), 1);
        assert!((a[0].percent - 6.0).abs() < 1e-9, "{a:?}");
        assert_eq!(a[0].role, "executor");
        let b = attribute(&one("b"), &samples, &readings);
        assert!((b[0].percent - 7.0).abs() < 1e-9, "{b:?}");
        assert_eq!(b[0].role, "reviewer");
    }

    #[test]
    fn a_reset_counts_the_new_period_from_zero() {
        let samples = vec![
            sample("a", 0, 0.0, Some("executor")),
            sample("a", 300, 300.0, None),
        ];
        // The window resets at 150; the 4% read at 300 was all spent since.
        let readings = vec![reading(0, 90.0, 150), reading(300, 4.0, 18_150)];
        let shares = attribute(&one("a"), &samples, &readings);
        assert!((shares[0].percent - 4.0).abs() < 1e-9, "{shares:?}");
    }

    #[test]
    fn an_increase_with_no_kanban_tokens_is_nobodys() {
        let samples = vec![
            sample("a", 0, 0.0, Some("executor")),
            sample("a", 50, 100.0, None),
        ];
        let readings = vec![
            reading(0, 10.0, 10_000),
            reading(50, 12.0, 10_000),
            reading(500, 30.0, 10_000),
        ];
        let shares = attribute(&one("a"), &samples, &readings);
        assert!((shares[0].percent - 2.0).abs() < 1e-9, "{shares:?}");
    }

    #[test]
    fn weights_favour_output_over_cache_reads() {
        let output = TokenBreakdown {
            output: 100,
            ..Default::default()
        };
        let cached = TokenBreakdown {
            input: 100,
            cache_read: 100,
            ..Default::default()
        };
        assert!(weigh(&output) > 10.0 * weigh(&cached));
    }
}
