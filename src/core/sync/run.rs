//! Runner-owned, portable run records. Session records never leave this machine.
use super::*;
use crate::core::models::{Role, RunPhase, Session, SessionStatus};
use crate::core::storage::Storage;
use crate::core::timefmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Queued,
    Running,
    Waiting,
    Finished,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunDescriptor {
    pub node_id: String,
    pub name: String,
    pub role: NodeRole,
    #[serde(rename = "gen")]
    pub generation: u64,
    pub state: RunState,
    pub phase: String,
    pub started_at: Option<String>,
    pub branch: Option<String>,
    pub heartbeat: String,
    pub code: Option<String>,
}

impl RunDescriptor {
    pub fn active(&self) -> bool {
        matches!(
            self.state,
            RunState::Queued | RunState::Running | RunState::Waiting
        )
    }

    pub fn label(&self) -> String {
        let state = match self.state {
            RunState::Queued => "queued",
            RunState::Waiting => "waiting",
            _ => "running",
        };
        format!("{state} on {}", self.name)
    }
}

pub(crate) fn store_for_board(board: &Path) -> Option<&Path> {
    let projects = board.parent()?;
    (projects.file_name()? == "projects")
        .then(|| projects.parent())
        .flatten()
}

impl Journal {
    pub fn runs(
        &self,
        project_alias: &str,
        task_id: &str,
    ) -> Result<BTreeMap<String, RunDescriptor>> {
        let state = self.state()?;
        let Some(project) = state.project_uids.get(project_alias) else {
            return Ok(BTreeMap::new());
        };
        decode_fields(state.fields.get(&key(project, "run", task_id)))
    }
}

/// No identity or sync files are created when reading a legacy in-place board.
pub fn remote_runs(board: &Path, task_id: &str) -> Result<Vec<RunDescriptor>> {
    Ok(remote_runs_for_tasks(board, [task_id])?
        .remove(task_id)
        .unwrap_or_default())
}

pub(crate) fn remote_runs_for_tasks<'a>(
    board: &Path,
    task_ids: impl IntoIterator<Item = &'a str>,
) -> Result<BTreeMap<String, Vec<RunDescriptor>>> {
    let mut result = BTreeMap::new();
    let Some(store) = store_for_board(board) else {
        return Ok(result);
    };
    if !store.join("sync/node.yaml").exists() {
        return Ok(result);
    }
    let journal = Journal::open(store)?;
    let state = journal.state()?;
    let alias = board
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid board path"))?;
    let Some(project) = state.project_uids.get(alias) else {
        return Ok(result);
    };
    for task_id in task_ids {
        let runs: Vec<_> = decode_fields(state.fields.get(&key(project, "run", task_id)))?
            .into_values()
            .filter(|run| run.node_id != journal.node.node_id && run.active())
            .collect();
        if !runs.is_empty() {
            result.insert(task_id.into(), runs);
        }
    }
    Ok(result)
}

fn decode_fields(fields: Option<&FieldState>) -> Result<BTreeMap<String, RunDescriptor>> {
    fields
        .into_iter()
        .flat_map(|f| &f.values)
        .map(|(node, value)| {
            let run = serde_json::from_value(value.clone()).map_err(|e| invalid(&e.to_string()))?;
            Ok((node.clone(), run))
        })
        .collect()
}

pub(super) fn validate(op: &Operation) -> Result<()> {
    for component in [&op.project, &op.id, &op.hlc.2] {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains(['/', '\\', '\0'])
        {
            return Err(invalid("invalid run path component"));
        }
    }
    if op.project.is_empty() || op.set.len() != 1 || !op.set.contains_key(&op.hlc.2) {
        return Err(invalid("run may only be written by its runner"));
    }
    let run: RunDescriptor =
        serde_json::from_value(op.set[&op.hlc.2].clone()).map_err(|e| invalid(&e.to_string()))?;
    if run.node_id != op.hlc.2
        || run.generation == 0
        || run.name.is_empty()
        || !run
            .name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || !matches!(
            run.phase.as_str(),
            "executor" | "designer" | "reviewer" | "orchestrator"
        )
    {
        return Err(invalid("invalid run descriptor"));
    }
    timefmt::parse(&run.heartbeat).map_err(|_| invalid("invalid run heartbeat"))?;
    if let Some(start) = &run.started_at {
        timefmt::parse(start).map_err(|_| invalid("invalid run start"))?;
    }
    if run
        .branch
        .as_deref()
        .is_some_and(|branch| branch.starts_with('/') || branch.contains('\\'))
    {
        return Err(invalid("run branch cannot contain a local path"));
    }
    Ok(())
}

pub(super) fn projection(
    root: &Path,
    project: &str,
    task: &str,
    fields: &FieldState,
) -> Result<(PathBuf, String)> {
    let runs = decode_fields(Some(fields))?;
    let path = root
        .join("meta")
        .join(project)
        .join("runs")
        .join(format!("{task}.yaml"));
    let text = serde_yaml_ng::to_string(&serde_json::json!({"runs": runs}))?;
    Ok((path, text))
}

/// Append a descriptor to the same durable redo transaction as its local input.
pub(super) fn capture(
    journal: &Journal,
    target: &Target,
    project: &str,
    inputs: (&Value, &Value),
    state: &mut JournalState,
    clock: &mut Hlc,
    operations: &mut Vec<Operation>,
) -> Result<()> {
    let (before, after) = inputs;
    if !matches!(target.entity, "task" | "session") || after.is_null() {
        return Ok(());
    }
    let board = target.store.join("projects").join(&target.project);
    let (task_id, task, session) = if target.entity == "session" {
        let session = Session::deserialize(after).map_err(|e| invalid(&e.to_string()))?;
        let task = Storage::new(&board).load_task(&session.task_id)?;
        (
            session.task_id.clone(),
            std::borrow::Cow::Owned(
                task.map(|t| serde_json::to_value(t).expect("task serialization"))
                    .unwrap_or(Value::Null),
            ),
            Some(session),
        )
    } else {
        let session = after.get("session").and_then(Value::as_str).and_then(|id| {
            crate::core::session::SessionManager::validate_session_id(id).ok()?;
            let path = board.join(".kanban/sessions").join(format!("{id}.yaml"));
            fs::read_to_string(path)
                .ok()
                .and_then(|text| serde_yaml_ng::from_str::<Session>(&text).ok())
                .filter(|session| session.task_id == target.id)
        });
        (
            target.id.clone(),
            std::borrow::Cow::Borrowed(after),
            session,
        )
    };
    let records = decode_fields(state.fields.get(&key(project, "run", &task_id)))?;
    let old = records.get(&journal.node.node_id);
    // Waiting for an explicit remote runner does not claim a local run.
    // Otherwise its replicated queued descriptor would exclude the real runner.
    let remote_runner = task
        .get("runner")
        .and_then(Value::as_str)
        .and_then(crate::core::config::Runner::parse)
        .and_then(crate::core::config::Runner::node_role)
        .is_some_and(|role| role != journal.node.role);
    let queued = target.entity == "task"
        && task.get("run_phase").and_then(Value::as_str) == Some("queued")
        && !remote_runner;
    // An ordinary board edit on a peer must never create or claim a run.
    if target.entity == "task"
        && queued
        && before.get("run_phase") == task.get("run_phase")
        && old.is_none()
    {
        return Ok(());
    }
    if !queued && session.is_none() && !old.is_some_and(|run| run.state == RunState::Queued) {
        return Ok(());
    }
    let started_at = if queued {
        None
    } else {
        session.as_ref().map(|s| timefmt::format(&s.started_at))
    };
    // Closing an old session after a new launch must not finish the new run.
    if target.entity == "session"
        && before != &Value::Null
        && old.is_some_and(|r| r.started_at != started_at)
    {
        return Ok(());
    }
    let new_run = old.is_none_or(|r| r.started_at != started_at && r.state != RunState::Queued);
    let generation = if new_run {
        records
            .values()
            .map(|r| r.generation)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid("run generation exhausted"))?
    } else {
        old.expect("existing run").generation
    };
    let phase: Option<RunPhase> = task
        .get("run_phase")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|e| invalid(&e.to_string()))?;
    let state_of_run = if queued && target.entity == "task" {
        RunState::Queued
    } else {
        match session.as_ref().map(|s| s.status) {
            Some(SessionStatus::Closed) => RunState::Finished,
            Some(SessionStatus::Crashed) => RunState::Failed,
            None => RunState::Finished,
            _ if session.as_ref().is_some_and(|s| s.wait_until.is_some()) => RunState::Waiting,
            _ => RunState::Running,
        }
    };
    let heartbeat = session
        .as_ref()
        .map(|s| timefmt::format(&s.last_seen))
        .unwrap_or_else(|| timefmt::format(&timefmt::now()));
    let mut run = RunDescriptor {
        node_id: journal.node.node_id.clone(),
        name: journal.node.name.clone(),
        role: journal.node.role,
        generation,
        state: state_of_run,
        phase: phase
            .map(|p| Role::from_phase(Some(p)).as_str().to_owned())
            .or_else(|| old.map(|r| r.phase.clone()))
            .unwrap_or_else(|| "executor".into()),
        started_at,
        branch: task
            .get("branch")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| old.and_then(|r| r.branch.clone())),
        heartbeat,
        code: None,
    };
    if let Some(old) = old {
        let config: Value = match fs::read_to_string(target.store.join("config.yaml")) {
            Ok(text) => serde_yaml_ng::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(e) => return Err(e.into()),
        };
        let interval = config
            .get("sync")
            .and_then(|c| c.get("run_heartbeat"))
            .map(|v| {
                v.as_i64().filter(|n| *n > 0).ok_or_else(|| {
                    invalid("sync.run_heartbeat must be a positive number of seconds")
                })
            })
            .transpose()?
            .unwrap_or(60);
        let age = timefmt::parse(&run.heartbeat)
            .ok()
            .zip(timefmt::parse(&old.heartbeat).ok())
            .map(|(new, old)| (new - old).num_seconds())
            .unwrap_or(interval);
        if age < interval {
            run.heartbeat = old.heartbeat.clone();
        }
        if &run == old {
            return Ok(());
        }
    }
    let seq = state
        .vector
        .get(&journal.node.node_id)
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| invalid("operation sequence exhausted"))?;
    *clock = clock.tick(now_ms(), &journal.node.node_id)?;
    let previous = state
        .fields
        .get(&key(project, "run", &task_id))
        .and_then(|f| f.clocks.get(&journal.node.node_id));
    let op = Operation {
        op: format!("{}:{seq}", journal.node.node_id),
        hlc: clock.clone(),
        store: journal.node.store_id.clone(),
        project: project.into(),
        entity: "run".into(),
        id: task_id,
        set: BTreeMap::from([(
            journal.node.node_id.clone(),
            serde_json::to_value(run).map_err(|e| invalid(&e.to_string()))?,
        )]),
        prev: previous
            .map(|hlc| BTreeMap::from([(journal.node.node_id.clone(), hlc.clone())]))
            .unwrap_or_default(),
        actor: "kanban".into(),
    };
    validate(&op)?;
    apply_meta(state, &op);
    operations.push(op);
    Ok(())
}
