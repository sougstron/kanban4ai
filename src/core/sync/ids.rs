//! Task identity. Replicated task operations are keyed by an immutable task
//! UID; `TASK-N` is only the display name a node allocates locally as
//! max(live, historical) + 1. Tasks captured before UIDs existed keep their
//! display id as UID. When two offline nodes claim the same number, every
//! node derives the same winner (earliest creation HLC, node id breaks ties)
//! and renumbers the other to the next unclaimed number.
use super::*;
use crate::core::storage::Storage;

/// Task fields that hold other tasks' ids; replicated as UIDs.
const REFERENCES: [&str; 3] = ["parent_task", "chained_to", "depends_on"];

pub(super) fn number(id: &str) -> Option<u64> {
    id.strip_prefix("TASK-")?.parse().ok()
}

fn display_key(project: &str, display: &str) -> String {
    key(project, "task", display)
}

/// Replicated identity behind a local display id. Unknown ids are returned
/// unchanged, which also maps legacy UIDs and already-translated UIDs.
pub fn task_uid(state: &JournalState, project: &str, display: &str) -> String {
    state
        .task_uids
        .get(&display_key(project, display))
        .cloned()
        .unwrap_or_else(|| display.to_owned())
}

/// Current display id for a task UID; unknown UIDs are returned unchanged.
pub fn task_display(state: &JournalState, project: &str, uid: &str) -> String {
    displays(state, project)
        .remove(uid)
        .unwrap_or_else(|| uid.to_owned())
}

fn displays(state: &JournalState, project: &str) -> BTreeMap<String, String> {
    state
        .task_uids
        .iter()
        .filter_map(|(k, uid)| {
            let (p, _, display): (String, String, String) = serde_json::from_str(k).ok()?;
            (p == project).then(|| (uid.clone(), display))
        })
        .collect()
}

/// Bind a captured display id to its UID, minting one for a new task.
pub(super) fn capture_uid(
    state: &mut JournalState,
    project: &str,
    display: &str,
    created: bool,
) -> Result<String> {
    let k = display_key(project, display);
    if let Some(uid) = state.task_uids.get(&k) {
        return Ok(uid.clone());
    }
    let legacy = state.fields.contains_key(&key(project, "task", display));
    let uid = if created && !legacy {
        uuid()?
    } else {
        display.to_owned()
    };
    state.task_uids.insert(k, uid.clone());
    Ok(uid)
}

fn map_references(set: &mut Fields, map: impl Fn(&str) -> String) {
    for field in REFERENCES {
        match set.get_mut(field) {
            Some(Value::String(id)) => *id = map(id),
            Some(Value::Array(ids)) => {
                for id in ids.iter_mut() {
                    if let Value::String(text) = id {
                        *text = map(text);
                    }
                }
            }
            _ => (),
        }
    }
}

pub(super) fn references_to_uids(state: &JournalState, project: &str, set: &mut Fields) {
    map_references(set, |id| task_uid(state, project, id));
}

pub(super) fn references_to_displays(
    displays: &BTreeMap<String, String>,
    values: &mut BTreeMap<String, Value>,
) {
    map_references(values, |id| displays.get(id).cloned().unwrap_or(id.into()));
}

/// Deterministic UID -> (display, claimed display) for one project, a pure
/// function of the replicated operations. Deleted tasks keep their numbers.
pub(super) fn assign(
    state: &JournalState,
    project: &str,
) -> BTreeMap<String, (String, Option<String>)> {
    // Creation clock and claimed display id: the earliest operation, and the
    // earliest `id` value (legacy UIDs are their own claim).
    let mut created: Vec<(Hlc, String, Option<String>)> = Vec::new();
    let mut claimed: BTreeSet<u64> = BTreeSet::new();
    for (k, life) in &state.lifecycle {
        let Ok((uid_project, entity, id)) = serde_json::from_str::<(String, String, String)>(k)
        else {
            continue;
        };
        if uid_project != project || entity != "task" {
            continue;
        }
        if life.deleted.is_some() {
            claimed.extend(number(&id));
            if let Some(claim) = &life.claim {
                claimed.extend(number(&claim.id));
            }
        }
        let Some(hlc) = life.created.clone() else {
            continue;
        };
        let display = life
            .claim
            .as_ref()
            .map(|c| c.id.clone())
            .or_else(|| number(&id).map(|_| id.clone()));
        created.push((hlc, id, display));
    }
    let mut tasks: Vec<_> = created
        .into_iter()
        .map(|(hlc, uid, display)| {
            let display = display.or_else(|| number(&uid).map(|_| uid.clone()));
            (hlc, uid, display)
        })
        .collect();
    tasks.sort();
    claimed.extend(
        tasks
            .iter()
            .filter_map(|(_, _, d)| d.as_deref().and_then(number)),
    );
    let mut result = BTreeMap::new();
    let mut taken = BTreeSet::new();
    let mut losers = Vec::new();
    for (_, uid, display) in tasks {
        match display {
            Some(display) if taken.insert(number(&display).expect("claimed number")) => {
                result.insert(uid, (display.clone(), Some(display)));
            }
            display => losers.push((uid, display)),
        }
    }
    for (uid, display) in losers {
        let mut next = display.as_deref().and_then(number).unwrap_or(0) + 1;
        while claimed.contains(&next) {
            next += 1;
        }
        claimed.insert(next);
        result.insert(uid.to_owned(), (format!("TASK-{next:03}"), display));
    }
    result
}

/// Display ids before and after one projection.
pub(super) struct Displays {
    current: BTreeMap<String, BTreeMap<String, String>>,
    previous: BTreeMap<String, BTreeMap<String, String>>,
    empty: BTreeMap<String, String>,
}

impl Displays {
    pub(super) fn known(&self, project: &str, uid: &str) -> bool {
        self.project(project).contains_key(uid)
    }

    pub(super) fn display(&self, project: &str, uid: &str) -> String {
        self.project(project)
            .get(uid)
            .cloned()
            .unwrap_or_else(|| uid.to_owned())
    }

    /// UID -> display for one project.
    pub(super) fn project(&self, project: &str) -> &BTreeMap<String, String> {
        self.current.get(project).unwrap_or(&self.empty)
    }

    /// The display id this node projected the task under before, if any.
    pub(super) fn previous(&self, project: &str, uid: &str) -> Option<String> {
        self.previous.get(project)?.get(uid).cloned()
    }

    /// `(project, uid, old display, new display)` for every local renumbering.
    pub(super) fn renamed(&self) -> Vec<(String, String, String, String)> {
        let mut result = Vec::new();
        for (project, uids) in &self.current {
            for (uid, display) in uids {
                if let Some(old) = self.previous(project, uid)
                    && old != *display
                {
                    result.push((project.clone(), uid.clone(), old, display.clone()));
                }
            }
        }
        result
    }
}

/// Re-derive every project's display ids from the replicated operations,
/// keeping retired ids as aliases of their task.
pub(super) fn reassign(state: &mut JournalState) -> Displays {
    let mut previous: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (k, uid) in &state.task_uids {
        if let Ok((project, _, display)) = serde_json::from_str::<(String, String, String)>(k) {
            previous
                .entry(project)
                .or_default()
                .insert(uid.clone(), display);
        }
    }
    let projects: BTreeSet<String> = state
        .lifecycle
        .iter()
        .filter_map(|(k, life)| {
            let (project, entity, _): (String, String, String) = serde_json::from_str(k).ok()?;
            (entity == "task" && life.created.is_some()).then_some(project)
        })
        .collect();
    let mut current = BTreeMap::new();
    for project in projects {
        let assigned = assign(state, &project);
        let known = previous.entry(project.clone()).or_default();
        for uid in assigned.keys() {
            // Legacy tasks were projected under their own UID.
            if !known.contains_key(uid)
                && number(uid).is_some()
                && !state.task_uids.contains_key(&display_key(&project, uid))
            {
                known.insert(uid.clone(), uid.clone());
            }
        }
        state.task_uids.retain(|k, _| {
            serde_json::from_str::<(String, String, String)>(k).is_ok_and(|(p, _, _)| p != project)
        });
        let mut displays = BTreeMap::new();
        for (uid, (display, claim)) in assigned {
            state
                .task_uids
                .insert(display_key(&project, &display), uid.clone());
            for retired in [claim, known.get(&uid).cloned()].into_iter().flatten() {
                if retired != display {
                    state
                        .task_aliases
                        .insert(display_key(&project, &retired), uid.clone());
                }
            }
            displays.insert(uid, display);
        }
        current.insert(project, displays);
    }
    Displays {
        current,
        previous,
        empty: BTreeMap::new(),
    }
}

/// Highest task number this store has ever known for the board, so a local
/// allocation never reuses a deleted, renumbered or replicated number.
pub(crate) fn historical_max(board: &Storage) -> Result<u64> {
    let path = board.tasks_dir.join("todo/TASK-001.md");
    let Some(target) = target(&path) else {
        return Ok(0);
    };
    if !enabled(&target.store) {
        return Ok(0);
    }
    let journal = Journal::open(&target.store)?;
    let state = journal.state()?;
    let Some(project) = state.project_uids.get(&target.project) else {
        return Ok(0);
    };
    let mut max = 0;
    for (k, uid) in state.task_uids.iter().chain(&state.task_aliases) {
        let (p, _, display): (String, String, String) =
            serde_json::from_str(k).map_err(|e| invalid(&e.to_string()))?;
        if p == *project {
            max = max
                .max(number(&display).unwrap_or(0))
                .max(number(uid).unwrap_or(0));
        }
    }
    for (k, life) in &state.lifecycle {
        let Ok((p, entity, id)) = serde_json::from_str::<(String, String, String)>(k) else {
            continue;
        };
        if p != *project || entity != "task" {
            continue;
        }
        max = max.max(number(&id).unwrap_or(0));
        if let Some(claim) = &life.claim {
            max = max.max(number(&claim.id).unwrap_or(0));
        }
    }
    Ok(max)
}

/// A retired display id (`TASK-N` before collision renumbering) -> current
/// display id. `None` when the board is not synced or the id was never renamed.
pub(crate) fn renamed(board: &Path, id: &str) -> Result<Option<String>> {
    let Some(store) = run::store_for_board(board) else {
        return Ok(None);
    };
    if !enabled(store) {
        return Ok(None);
    }
    let alias = board
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("invalid board"))?;
    let state = Journal::open(store)?.state()?;
    let Some(project) = state.project_uids.get(alias) else {
        return Ok(None);
    };
    Ok(state
        .task_aliases
        .get(&display_key(project, id))
        .map(|uid| task_display(&state, project, uid))
        .filter(|display| display != id))
}
