//! Durable, generation-addressed runner commands and answer wake receipts.
use super::*;
use crate::core::models::{MessageKind, RunMode, TaskStatus};
use crate::core::operations::Operations;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Command {
    task: String,
    node: String,
    generation: u64,
    action: String,
}

pub(super) fn validate(op: &Operation) -> Result<()> {
    let command: Command =
        serde_json::from_value(op.set["command"].clone()).map_err(|e| invalid(&e.to_string()))?;
    if op.id != op.op
        || op.project.is_empty()
        || !op.prev.is_empty()
        || op.set.len() != 1
        || command.generation == 0
        || command.node.is_empty()
        // A task UID, or a legacy display id that is its own UID.
        || command.task.is_empty()
        || !command.task.bytes().all(|b| {
            b.is_ascii_alphanumeric() || b == b'@' || b == b'-'
        })
        || !matches!(command.action.as_str(), "stop" | "rerun")
    {
        return Err(invalid("invalid runner command"));
    }
    Ok(())
}

/// Returns false for a local run. No connection is required to record intent.
pub(crate) fn request(board: &Path, task: &str, action: &str) -> Result<bool> {
    let Some(store) = store_for_board(board) else {
        return Ok(false);
    };
    if !enabled(store) {
        return Ok(false);
    }
    let journal = Journal::open(store)?;
    let alias = board
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("invalid board"))?;
    let records = journal.runs(alias, task)?;
    let target = records
        .values()
        .filter(|r| r.node_id != journal.node.node_id)
        .filter(|r| r.active() || action == "rerun")
        .max_by_key(|r| (r.generation, &r.node_id));
    let Some(target) = target else {
        return Ok(false);
    };
    if records
        .get(&journal.node.node_id)
        .is_some_and(|local| local.active() || local.generation >= target.generation)
    {
        return Ok(false);
    }
    let _board = crate::core::storage::Storage::new(board).lock()?;
    let _sync = lock_file_reentrant(&journal.root.join(".lock"))?;
    journal.recover()?;
    let mut state = projection::load_state(&journal.root)?.0;
    let before_uids = projection::uids_of(&state);
    let project = state
        .project_uids
        .get(alias)
        .ok_or_else(|| invalid("unknown project"))?
        .clone();
    let seq = state
        .vector
        .get(&journal.node.node_id)
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| invalid("operation sequence exhausted"))?;
    let clock: Hlc = journal.clock()?;
    let id = format!("{}:{seq}", journal.node.node_id);
    let command = Command {
        task: super::ids::task_uid(&state, &project, task),
        node: target.node_id.clone(),
        generation: target.generation,
        action: action.into(),
    };
    let operation = Operation {
        v: OP_FORMAT,
        op: id.clone(),
        id,
        extra: BTreeMap::new(),
        hlc: clock.tick(now_ms(), &journal.node.node_id)?,
        store: journal.node.store_id.clone(),
        project,
        entity: "run".into(),
        set: BTreeMap::from([(
            "command".into(),
            serde_json::to_value(command).map_err(|e| invalid(&e.to_string()))?,
        )]),
        prev: BTreeMap::new(),
        actor: "human".into(),
    };
    validate(&operation)?;
    apply_meta(&mut state, &operation);
    commit_local(&journal, &state, &before_uids, &[operation], None)?;
    Ok(true)
}

/// Runs outside sync locks. Receipts survive duplicate delivery and compaction.
pub(crate) fn process(ops: &Operations) -> Result<()> {
    let Some(store) = store_for_board(ops.data_root()) else {
        return Ok(());
    };
    if !enabled(store) {
        return Ok(());
    }
    let journal = Journal::open(store)?;
    let alias = ops
        .data_root()
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("invalid board"))?;
    let _control = lock_file_reentrant(&ops.data_root().join(".kanban/runner-control.lock"))?;
    let state = journal.state()?;
    let Some(project) = state.project_uids.get(alias) else {
        return Ok(());
    };
    let path = ops.data_root().join(".kanban/runner-control.json");
    let mut receipts: BTreeSet<String> = load(&path)?;
    let mut commands = Vec::new();
    let mut answers: BTreeMap<String, Vec<(String, Hlc)>> = BTreeMap::new();
    let mut stops: BTreeMap<String, Hlc> = BTreeMap::new();
    for (key_text, fields) in &state.fields {
        let (uid, entity, id): (String, String, String) =
            serde_json::from_str(key_text).map_err(|e| invalid(&e.to_string()))?;
        if &uid != project {
            continue;
        }
        if entity == "run"
            && let Some(value) = fields.values.get("command")
        {
            let mut command: Command =
                serde_json::from_value(value.clone()).map_err(|e| invalid(&e.to_string()))?;
            command.task = super::ids::task_display(&state, project, &command.task);
            if command.node == journal.node.node_id
                && command.action == "stop"
                && state
                    .fields
                    .get(&key(
                        project,
                        "run",
                        &super::ids::task_uid(&state, project, &command.task),
                    ))
                    .and_then(|fields| fields.values.get(&journal.node.node_id))
                    .and_then(|run| run.get("gen"))
                    .and_then(Value::as_u64)
                    == Some(command.generation)
            {
                let clock = &fields.clocks["command"];
                stops
                    .entry(command.task.clone())
                    .and_modify(|last| *last = last.clone().max(clock.clone()))
                    .or_insert_with(|| clock.clone());
            }
            if command.node == journal.node.node_id && !receipts.contains(&id) {
                commands.push((fields.clocks["command"].clone(), id, command));
            }
        } else if entity == "message"
            && fields.values.get("status").and_then(Value::as_str) == Some("answered")
            && fields.values.get("kind").and_then(Value::as_str) == Some("question")
            && let Some(clock) = fields.clocks.get("answer")
            && clock.2 != journal.node.node_id
        {
            let receipt = format!("answer:{id}:{}:{}:{}", clock.0, clock.1, clock.2);
            if fields.values.get("author").and_then(Value::as_str)
                == Some(crate::core::operations::EXECUTOR_POOL_QUESTION_SOURCE)
                && fields.values.get("answer").and_then(Value::as_str)
                    == Some(crate::core::operations::WAIT_FOR_QUOTA_VARIANT)
            {
                continue;
            }
            if !receipts.contains(&receipt)
                && let Some(task) = fields.values.get("task_id").and_then(Value::as_str)
            {
                answers
                    .entry(super::ids::task_display(&state, project, task))
                    .or_default()
                    .push((receipt, clock.clone()));
            }
        }
    }
    commands.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, id, command) in commands {
        if journal
            .runs(alias, &command.task)?
            .get(&journal.node.node_id)
            .is_some_and(|run| run.generation == command.generation)
        {
            if let Some(task) = ops.get_task(&command.task)? {
                match command.action.as_str() {
                    "stop" => {
                        if let Some(session) = task.session.as_deref() {
                            ops.stop_session(session)?;
                        } else if task.run_phase == Some(crate::core::models::RunPhase::Queued) {
                            ops.dequeue_task(&task.id)?;
                        }
                    }
                    "rerun" => {
                        pin_runner(ops, &task, &journal.node.name)?;
                        let mode = if ops.queue_can_dispatch()? {
                            RunMode::Queued
                        } else {
                            RunMode::Immediate
                        };
                        if task.status == TaskStatus::Review {
                            ops.rerun_review_task(&task.id, None, mode)?;
                        } else if task.status == TaskStatus::InProgress {
                            ops.rerun_in_progress_task(&task.id, None, mode)?;
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
        receipts.insert(id);
        atomic_write_text_raw(&path, &json(&receipts)?)?;
    }
    for (id, task_receipts) in answers {
        // Only the node that owns the latest run may wake a synced answer.
        let runs = journal.runs(alias, &id)?;
        let wake = task_receipts
            .iter()
            .any(|(_, clock)| stops.get(&id).is_none_or(|stop| clock > stop));
        if wake
            && runs
                .values()
                .max_by_key(|r| (r.generation, &r.node_id))
                .is_some_and(|run| run.node_id == journal.node.node_id)
            && let Some(task) = ops.get_task(&id)?
        {
            let tm = crate::core::thread::ThreadManager::new(ops.data_root())?;
            if task.status == TaskStatus::InProgress {
                pin_runner(ops, &task, &journal.node.name)?;
            }
            if !tm
                .open_messages(&id, Some(MessageKind::Question))?
                .is_empty()
            {
                continue;
            }
            if task.run_phase != Some(crate::core::models::RunPhase::Queued)
                && let Err(error) = ops.resume_answered_agent(&id, task.session.as_deref())
            {
                tm.post(
                    &id,
                    crate::core::models::MessageRole::System,
                    MessageKind::System,
                    &format!("Answer recorded, but the agent could not be relaunched: {error}"),
                    None,
                    vec![],
                    Some("kanban".into()),
                )?;
            }
        }
        receipts.extend(task_receipts.into_iter().map(|(receipt, _)| receipt));
        atomic_write_text_raw(&path, &json(&receipts)?)?;
    }
    Ok(())
}

fn pin_runner(ops: &Operations, task: &crate::core::models::Task, name: &str) -> Result<()> {
    if task
        .runner
        .as_ref()
        .is_none_or(|runner| runner.as_str() != name)
    {
        ops.update_task(
            &task.id,
            crate::core::operations::TaskPatch {
                runner: Some(Some(crate::core::config::Runner::Node(name.into()))),
                ..Default::default()
            },
        )?;
    }
    Ok(())
}

pub(crate) fn process_store(store: &Path) -> Result<()> {
    for project in crate::core::project::ProjectStore::at(store).list()? {
        process(&Operations::for_project(&project))?;
    }
    Ok(())
}
