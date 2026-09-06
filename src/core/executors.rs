//! Limit-aware selection for the board-level executor pools
//! (`orchestration.executors`).
//!
//! The pool is a PRE-launch gate: before a task starts, its pool is walked in
//! order and the first candidate whose provider has quota headroom runs. Only
//! the cached limits snapshot is read — never a blocking fetch — so the
//! selection is safe to run under the board lock inside the dispatch claim.
//!
//! This is deliberately pure: a hand-built [`LimitsSnapshot`] in, a
//! [`PoolChoice`] out, no I/O. The post-mortem failover (a run that dies on a
//! 429 the pre-flight gate could not predict) remains
//! `Operations::advance_role_roster`.

use crate::core::config::{ExecutorPools, PoolThresholds, RoleCandidate};
use crate::core::limits::{LimitWindow, LimitsSnapshot, provider_for};

/// Which executor pool to walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pool {
    /// The "smart" pool: reviews, designers, and tasks with
    /// `role_profile: middle`.
    Middle,
    /// The working pool: the default executor assignment.
    Cheap,
}

impl Pool {
    pub fn name(self) -> &'static str {
        match self {
            Pool::Middle => "middle",
            Pool::Cheap => "cheap",
        }
    }

    pub(crate) fn roster(self, pools: &ExecutorPools) -> &[RoleCandidate] {
        match self {
            Pool::Middle => &pools.middle,
            Pool::Cheap => &pools.cheap,
        }
    }
}

/// What the pool walk decided.
#[derive(Debug, Clone, PartialEq)]
pub enum PoolChoice {
    /// A candidate with headroom, at its (zero-based) pool position.
    Candidate {
        candidate: RoleCandidate,
        index: u32,
    },
    /// Every candidate is out of quota. `wake_at` is the earliest known
    /// provider reset among the blocking windows plus the configured grace
    /// (`ask_grace_secs`), or `now + 15 min` when no reset is known so the
    /// task cannot park forever. `blocked` names the blocking windows
    /// (`claude 5h`) for the thread note.
    AllBlocked { wake_at: i64, blocked: Vec<String> },
    /// The pool is empty: the caller keeps the task's own assignment.
    NoPool,
}

/// Fallback retry horizon when no blocking window names a reset time.
const NO_RESET_FALLBACK_SECS: i64 = 15 * 60;

/// Walk `pool` in order; the first candidate whose provider clears the
/// configured thresholds wins.
pub fn select(
    pools: &ExecutorPools,
    pool: Pool,
    snapshot: Option<&LimitsSnapshot>,
    now: i64,
) -> PoolChoice {
    let roster = pool.roster(pools);
    if roster.is_empty() {
        return PoolChoice::NoPool;
    }
    let mut blocked: Vec<String> = Vec::new();
    let mut earliest_reset: Option<i64> = None;
    for (index, candidate) in roster.iter().enumerate() {
        match blocking_window(snapshot, candidate, &pools.thresholds, now) {
            None => {
                return PoolChoice::Candidate {
                    candidate: candidate.clone(),
                    index: index as u32,
                };
            }
            Some((provider, window)) => {
                let name = format!("{provider} {}", window.label);
                if !blocked.contains(&name) {
                    blocked.push(name);
                }
                if let Some(resets_at) = window.resets_at {
                    earliest_reset = Some(earliest_reset.map_or(resets_at, |at| at.min(resets_at)));
                }
            }
        }
    }
    let wake_at =
        earliest_reset.map_or(now + NO_RESET_FALLBACK_SECS, |at| at + pools.ask_grace_secs);
    PoolChoice::AllBlocked { wake_at, blocked }
}

/// The quota window that makes `candidate` unusable right now, if any.
/// `None` means the candidate passes: its provider is unknown/not configured,
/// has no usable numbers, or every live window is at or above its floor.
fn blocking_window<'a>(
    snapshot: Option<&'a LimitsSnapshot>,
    candidate: &RoleCandidate,
    thresholds: &PoolThresholds,
    now: i64,
) -> Option<(&'static str, &'a LimitWindow)> {
    let provider = provider_for(candidate.backend.as_deref()?, candidate.model.as_deref())?;
    let limits = snapshot?.get(provider)?;
    if !limits.is_ready() {
        return None;
    }
    limits.live_windows(now).into_iter().find_map(|window| {
        let floor = match window.label.as_str() {
            "5h" => thresholds.five_hour_percent,
            _ => thresholds.week_percent,
        };
        (window.remaining_percent < floor).then_some((provider, window))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::PoolThresholds;
    use crate::core::limits::{LimitWindow, ProviderLimits, ProviderState};

    fn candidate(spec: &str) -> RoleCandidate {
        let (backend, model) = spec.split_once('/').expect("backend/model spec");
        RoleCandidate {
            backend: Some(backend.to_string()),
            model: Some(model.to_string()),
            effort: None,
            agent: None,
        }
    }

    fn pools(middle: &[&str], cheap: &[&str]) -> ExecutorPools {
        ExecutorPools {
            middle: middle.iter().map(|spec| candidate(spec)).collect(),
            cheap: cheap.iter().map(|spec| candidate(spec)).collect(),
            thresholds: PoolThresholds {
                week_percent: 5.0,
                five_hour_percent: 15.0,
            },
            ask_grace_secs: 60,
        }
    }

    /// `(provider, configured, windows)` windows are `(label, remaining %, resets_at)`.
    type SnapshotSpec<'a> = [(&'a str, bool, Vec<(&'a str, f64, Option<i64>)>)];

    fn snapshot(entries: &SnapshotSpec<'_>) -> LimitsSnapshot {
        let providers = entries
            .iter()
            .map(|(provider, ready, windows)| {
                let mut limits = ProviderLimits {
                    provider: (*provider).to_string(),
                    state: if *ready {
                        ProviderState::Ready
                    } else {
                        ProviderState::NotConfigured
                    },
                    windows: Vec::new(),
                    observed_at: None,
                };
                limits.windows = windows
                    .iter()
                    .map(|(label, remaining, resets_at)| LimitWindow {
                        label: (*label).to_string(),
                        remaining_percent: *remaining,
                        resets_at: *resets_at,
                        rolling: false,
                    })
                    .collect();
                limits
            })
            .collect();
        LimitsSnapshot {
            fetched_at: 1_000,
            providers,
        }
    }

    #[test]
    fn first_candidate_with_headroom_wins() {
        let pools = pools(&[], &["claude/haiku", "codex/gpt-5.5"]);
        // First candidate is over its 5h floor; the second must be picked.
        let limits = snapshot(&[("claude", true, vec![("5h", 10.0, Some(2_000))])]);
        let choice = select(&pools, Pool::Cheap, Some(&limits), 1_500);
        assert_eq!(
            choice,
            PoolChoice::Candidate {
                candidate: candidate("codex/gpt-5.5"),
                index: 1,
            }
        );
    }

    #[test]
    fn first_candidate_passes_when_quota_is_fine() {
        let pools = pools(&[], &["claude/haiku"]);
        let limits = snapshot(&[(
            "claude",
            true,
            vec![("5h", 66.0, Some(2_000)), ("7d", 95.0, Some(9_000))],
        )]);
        let choice = select(&pools, Pool::Cheap, Some(&limits), 1_500);
        assert_eq!(
            choice,
            PoolChoice::Candidate {
                candidate: candidate("claude/haiku"),
                index: 0,
            }
        );
    }

    #[test]
    fn unknown_provider_passes_the_gate() {
        let pools = pools(&[], &["opencode/some-local-model"]);
        // No snapshot at all: the candidate cannot be checked, so it runs.
        let choice = select(&pools, Pool::Cheap, None, 1_500);
        assert!(matches!(choice, PoolChoice::Candidate { index: 0, .. }));
    }

    #[test]
    fn empty_pool_is_nopool() {
        let pools = pools(&[], &[]);
        assert_eq!(select(&pools, Pool::Cheap, None, 1_500), PoolChoice::NoPool);
    }

    #[test]
    fn all_blocked_wake_at_is_earliest_reset_plus_grace() {
        let pools = pools(&[], &["claude/haiku", "codex/gpt-5.5"]);
        let limits = snapshot(&[
            // blocked on both windows, resets later
            (
                "claude",
                true,
                vec![("5h", 1.0, Some(3_000)), ("7d", 1.0, Some(9_000))],
            ),
            // blocked, resets earlier
            ("codex", true, vec![("5h", 1.0, Some(2_500))]),
        ]);
        let choice = select(&pools, Pool::Cheap, Some(&limits), 1_500);
        match choice {
            PoolChoice::AllBlocked { wake_at, blocked } => {
                assert_eq!(wake_at, 2_500 + 60, "earliest reset plus grace");
                assert!(blocked.contains(&"claude 5h".to_string()), "{blocked:?}");
                assert!(blocked.contains(&"codex 5h".to_string()), "{blocked:?}");
            }
            other => panic!("expected AllBlocked, got {other:?}"),
        }
    }

    #[test]
    fn all_blocked_without_reset_falls_back_fifteen_minutes() {
        let pools = pools(&[], &["claude/haiku"]);
        let limits = snapshot(&[("claude", true, vec![("5h", 1.0, None)])]);
        let choice = select(&pools, Pool::Cheap, Some(&limits), 1_500);
        match choice {
            PoolChoice::AllBlocked { wake_at, .. } => {
                assert_eq!(wake_at, 1_500 + 15 * 60);
            }
            other => panic!("expected AllBlocked, got {other:?}"),
        }
    }

    #[test]
    fn weekly_floor_uses_week_threshold_for_other_labels() {
        let pools = pools(&[], &["claude/haiku"]);
        // A `mon` window below the week floor blocks; at exactly the floor
        // (inclusive) it passes.
        let limits = snapshot(&[("claude", true, vec![("mon", 4.9, None)])]);
        assert!(matches!(
            select(&pools, Pool::Cheap, Some(&limits), 1_500),
            PoolChoice::AllBlocked { .. }
        ));
        let limits = snapshot(&[("claude", true, vec![("mon", 5.0, None)])]);
        assert!(matches!(
            select(&pools, Pool::Cheap, Some(&limits), 1_500),
            PoolChoice::Candidate { .. }
        ));
    }
}
