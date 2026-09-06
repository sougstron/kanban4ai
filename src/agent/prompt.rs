use crate::core::compaction::compact_text;
use crate::core::config::Config;
use crate::core::error::Result;
use crate::core::models::{Message, MessageKind, MessageStatus, Role, Task};
use crate::core::project::Roots;
use crate::core::storage::Storage;
use crate::core::thread::ThreadManager;

/// Assemble the prompt handed to a delegated agent.
///
/// Every board path in it is absolute: the agent's working directory is the
/// code folder, while the board lives under `data_root`, so a relative
/// `.kanban/…` instruction would make the agent write into the user's repo.
/// `role` selects the column-ownership block and, for designer/reviewer,
/// the whole prompt body.
pub fn build_agent_prompt<'a>(
    roots: impl Into<Roots<'a>>,
    task: &Task,
    session_id: &str,
    revert: bool,
    role: Role,
) -> Result<String> {
    let roots = roots.into();
    if revert {
        return Ok(build_revert_prompt(roots, task, session_id));
    }
    match role {
        Role::Orchestrator => return build_orchestrator_prompt(roots, task, session_id),
        Role::Designer => return build_designer_prompt(roots, task, session_id),
        Role::Reviewer => return build_reviewer_prompt(roots, task, session_id),
        Role::Executor => {}
    }
    let backups_dir = roots.data_path("backups").join(&task.id);
    let backups_dir = format!("{}/", backups_dir.display());
    let form_file = roots
        .data_path("forms")
        .join(format!("{}.ask.yaml", task.id));
    let form_file = form_file.display().to_string();
    let detached_log = roots
        .data_path("detached")
        .join("<task>-<stamp>.log")
        .display()
        .to_string();

    let mut prompt = format!(
        "Task: {}: {}\n\n\
You are a delegated kanban4ai agent working in project: {}\n\
Work only on task {}. The user-authored task is below.\n\n\
Session contract:\n\
- KANBAN_SESSION is set to {session_id}.\n\
- KANBAN_TASK_ID is set to {}.\n\
- KANBAN_CMD is set to the current kanban4ai executable; use \"$KANBAN_CMD\" instead of bare kanban so callbacks target this binary.\n\
- Before editing an existing file, copy it to {backups_dir} preserving the repo-relative path.\n\
- Record important progress with: \"$KANBAN_CMD\" context {} <text> --source agent\n\
- Keep the session alive with: \"$KANBAN_CMD\" heartbeat --session {session_id}\n\
- When implementation and verification are complete, run: \"$KANBAN_CMD\" done {} --session {session_id} --agent\n\
- Ask whenever clarification is needed with: \"$KANBAN_CMD\" ask {} <question> --agent\n\
- This session is non-interactive and terminates the moment you end your reply; background tasks, \
monitors, and \"notifications\" die with it, so nothing you launch can re-invoke you later.\n\
- Long-running foreground commands are safe: the board heartbeats for you while your process runs, \
with no time limit. Prefer blocking in the foreground and collecting every result before you \
continue; never end a reply while anything you launched is still running or unread.\n\
- If a result will take too long to block on, never start it as a plain shell background job: \
this session's whole process group dies with your reply. Instead run: \
\"$KANBAN_CMD\" detach {} --session {session_id} --eta <expected-seconds> --note <what you wait for> -- <command> [args...]\n\
  It survives this session, logs to {detached_log} plus a .status file, and declares the wait. \
To wait on something you did not detach, use \
\"$KANBAN_CMD\" waiting {} --session {session_id} --eta <expected-seconds> --note <what you wait for>\n\
  Either way the board relaunches you after the deadline to check the result. \
Details: \"$KANBAN_CMD\" detach --help, \"$KANBAN_CMD\" waiting --help.\n\
- The final command of your reply must be the done command above, the ask command if you are \
blocked, or the waiting/detach command if you are waiting on a declared long-running result. \
Ending a reply without one of those strands the task and forces an automatic resume.\n",
        task.id,
        task.title,
        roots.work_path.display(),
        task.id,
        task.id,
        task.id,
        task.id,
        task.id,
        task.id,
        task.id
    );
    prompt.push_str(&format!(
        "- Proactively record non-blocking ideas, risks, or better alternatives you notice \
(don't block on them, keep working) with: \"$KANBAN_CMD\" suggest {} <idea>\n",
        task.id
    ));
    prompt.push_str(&format!(
        "- To ask the human one or more questions, prefer a strict YAML form over free text so \
each question renders with selectable options. Write {form_file} then submit it:\n  \
\"$KANBAN_CMD\" ask-form {} --file {form_file} --agent --session {session_id}\n  \
Schema and examples: \"$KANBAN_CMD\" ask-form --help.\n",
        task.id
    ));
    if task.interactive {
        prompt.push_str(&format!(
            "- This task is interactive: for blocking questions use \"$KANBAN_CMD\" ask {} <question> --agent --wait --session {}; for non-blocking ideas use \"$KANBAN_CMD\" suggest.\n",
            task.id, session_id
        ));
    }
    prompt.push_str(role_column_block(Role::Executor));
    if let (Some(branch), Some(worktree)) = (&task.branch, &task.worktree) {
        let checkout = roots
            .data_path("worktrees")
            .join(worktree)
            .display()
            .to_string();
        prompt.push_str(&format!(
            "\nIsolation: you are working in an isolated git checkout at {checkout} on branch \
{branch}. It was cut from a live snapshot of the project folder, so it already contains the \
human's latest work — including uncommitted changes. Commit freely on this branch; it merges \
back into the project when the task is done. Do not create, switch, or delete branches, and do \
not touch the project folder's own checkout.\n"
        ));
    }
    append_role_instructions(roots, Role::Executor, &mut prompt);
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(task.description.trim());
    }
    append_upstream_results(roots, task, &mut prompt)?;
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
}

/// Small follow-up sent when Codex/pi/omp reopen their native conversation. The
/// backend already has the original task, rules, tool history, and its own
/// replies, so only the new board session identity and thread delta belong in
/// this turn.
pub fn build_resume_prompt<'a>(
    roots: impl Into<Roots<'a>>,
    task: &Task,
    session_id: &str,
    previous_session_id: &str,
    role: Role,
) -> Result<String> {
    let roots = roots.into();
    let finish = match role {
        Role::Executor => format!(
            "Finish with: \"$KANBAN_CMD\" done {} --session {session_id} --agent",
            task.id
        ),
        Role::Orchestrator => format!(
            "Submit the plan with \"$KANBAN_CMD\" plan {} --file <plan.yaml> --session {session_id} --agent, then finish with: \"$KANBAN_CMD\" done {} --session {session_id} --agent",
            task.id, task.id
        ),
        Role::Designer => format!(
            "Record the plan, then finish with: \"$KANBAN_CMD\" done {} --session {session_id} --agent",
            task.id
        ),
        Role::Reviewer => format!(
            "Finish only with: \"$KANBAN_CMD\" verdict {} --approve|--changes <text> --session {session_id} --agent",
            task.id
        ),
    };
    let mut prompt = format!(
        "Resume kanban4ai task {}. This is the same backend conversation, but the board session changed; all earlier session-id instructions are superseded.\n\
KANBAN_SESSION={session_id}; KANBAN_TASK_ID={}; use \"$KANBAN_CMD\" for callbacks.\n\
Heartbeat: \"$KANBAN_CMD\" heartbeat --session {session_id}\n\
{finish}. Continue from the latest state; do not repeat work already completed.",
        task.id, task.id
    );
    append_thread_delta(roots, task, previous_session_id, &mut prompt)?;
    Ok(prompt)
}

/// Planning-pass prompt: the designer records a plan on the thread and must
/// not implement or move the task. A later executor reads that plan through
/// [`append_thread_context`]. Finishing with `kanban done` completes the
/// design phase only — the board stays In Progress and starts the executor.
fn build_designer_prompt(roots: Roots<'_>, task: &Task, session_id: &str) -> Result<String> {
    let form_file = roots
        .data_path("forms")
        .join(format!("{}.ask.yaml", task.id));
    let form_file = form_file.display().to_string();
    let detached_log = roots
        .data_path("detached")
        .join("<task>-<stamp>.log")
        .display()
        .to_string();

    let mut prompt = format!(
        "Task: {}: {}\n\n\
You are the kanban4ai DESIGNER for project: {}\n\
Work only on task {}. The user-authored task is below.\n\n\
Your job is to plan, not to implement. Read the task, inspect the repo as needed,\
 and write a concrete plan. Do not edit project files. Do not move the task\
 between columns.\n\n\
Session contract:\n\
- KANBAN_SESSION is set to {session_id}.\n\
- KANBAN_TASK_ID is set to {}.\n\
- KANBAN_CMD is set to the current kanban4ai executable; use \"$KANBAN_CMD\" instead of bare kanban so callbacks target this binary.\n\
- Record the plan with: \"$KANBAN_CMD\" context {} <text> --source agent\n\
  Put the whole plan in context so the executor that runs next can see it.\n\
- Keep the session alive with: \"$KANBAN_CMD\" heartbeat --session {session_id}\n\
- When the plan is recorded, finish with: \"$KANBAN_CMD\" done {} --session {session_id} --agent\n\
  That completes the design phase only. The task stays In Progress; the board\n\
  then starts the assigned implementation bot. Do not treat done as \"the work is finished\".\n\
- Do not implement the task. Do not edit source files. Do not call commands that change the repo.\n\
- Do not move the task between columns (no take/move to Review/Done).\n\
- Ask whenever clarification is needed with: \"$KANBAN_CMD\" ask {} <question> --agent\n\
- This session is non-interactive and terminates the moment you end your reply; background tasks, \
monitors, and \"notifications\" die with it, so nothing you launch can re-invoke you later.\n\
- Long-running foreground commands are safe: the board heartbeats for you while your process runs, \
with no time limit. Prefer blocking in the foreground and collecting every result before you \
continue; never end a reply while anything you launched is still running or unread.\n\
- If a result will take too long to block on, never start it as a plain shell background job: \
this session's whole process group dies with your reply. Instead run: \
\"$KANBAN_CMD\" detach {} --session {session_id} --eta <expected-seconds> --note <what you wait for> -- <command> [args...]\n\
  It survives this session, logs to {detached_log} plus a .status file, and declares the wait. \
To wait on something you did not detach, use \
\"$KANBAN_CMD\" waiting {} --session {session_id} --eta <expected-seconds> --note <what you wait for>\n\
  Either way the board relaunches you after the deadline to check the result. \
Details: \"$KANBAN_CMD\" detach --help, \"$KANBAN_CMD\" waiting --help.\n\
- The final command of your reply must be the done command above, the ask command if you are \
blocked, or the waiting/detach command if you are waiting on a declared long-running result. \
Ending a reply without one of those strands the design phase and forces an automatic resume.\n",
        task.id,
        task.title,
        roots.work_path.display(),
        task.id,
        task.id,
        task.id,
        task.id,
        task.id,
        task.id,
        task.id
    );
    prompt.push_str(&format!(
        "- Proactively record non-blocking ideas, risks, or better alternatives you notice \
(don't block on them, keep planning) with: \"$KANBAN_CMD\" suggest {} <idea>\n",
        task.id
    ));
    prompt.push_str(&format!(
        "- To ask the human one or more questions, prefer a strict YAML form over free text so \
each question renders with selectable options. Write {form_file} then submit it:\n  \
\"$KANBAN_CMD\" ask-form {} --file {form_file} --agent --session {session_id}\n  \
Schema and examples: \"$KANBAN_CMD\" ask-form --help.\n",
        task.id
    ));
    if task.interactive {
        prompt.push_str(&format!(
            "- This task is interactive: for blocking questions use \"$KANBAN_CMD\" ask {} <question> --agent --wait --session {}; for non-blocking ideas use \"$KANBAN_CMD\" suggest.\n",
            task.id, session_id
        ));
    }
    prompt.push_str(role_column_block(Role::Designer));
    append_role_instructions(roots, Role::Designer, &mut prompt);
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(task.description.trim());
    }
    append_upstream_results(roots, task, &mut prompt)?;
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
}

/// Planning-pass prompt for the orchestrator: it decomposes the task into a
/// DAG of subtasks and submits it as a plan file. It never implements, and it
/// never starts anything itself — accepting the plan is what wires the graph,
/// and the board's own dispatcher runs it under the existing concurrency caps.
///
/// The whole contract lives here rather than in `AGENTS.md`: those files are
/// loaded into *every* agent session, so a plan-file schema nobody else can
/// use would be charged to every run on the board.
fn build_orchestrator_prompt(roots: Roots<'_>, task: &Task, session_id: &str) -> Result<String> {
    let plan_file = roots
        .data_path("plans")
        .join(format!("{}.plan.yaml", task.id));
    let plan_file = plan_file.display().to_string();
    let form_file = roots
        .data_path("forms")
        .join(format!("{}.ask.yaml", task.id));
    let form_file = form_file.display().to_string();
    let orch = Config::new(roots.data_root).get_orchestration()?;

    let mut prompt = format!(
        "Task: {}: {}\n\n\
You are the kanban4ai ORCHESTRATOR for project: {}\n\
Work only on task {}. The user-authored task is below.\n\n\
Your job is to decompose this task into a graph of subtasks and hand that graph to the board.\
 You do not implement anything, you do not edit project files, and you do not move tasks\
 between columns. The board runs the graph for you: each node becomes its own task with its\
 own agent session and its own context window.\n\n\
The graph is a DAG. Nodes are subtasks; an edge means two things at once — ordering (a node\
 starts only after every node it depends on has finished) and context (a node is prompted with\
 the results of exactly the nodes it depends on, and nothing else). Design the edges for the\
 second meaning as carefully as the first: the point of the graph is that each node reads a\
 small, relevant context instead of one huge shared history.\n\n\
Rules for a good plan:\n\
- Split by deliverable, not by activity. \"implement X\" and \"test X\" in one node is usually\
  better than two nodes that hand a half-finished X across an edge.\n\
- Parallel siblings must touch disjoint files. Each node runs in its own git worktree and its\
  branch is merged back when it finishes, so two siblings editing the same file will conflict.\
  If you cannot make them disjoint, sequence them with an edge instead.\n\
- Depth is cheap, width is risky. Prefer a chain where the work is genuinely sequential.\n\
- Write `needs:` for every node that has dependencies: one or two sentences saying what this\
  node must take from upstream. It is shown to that agent above the upstream results.\n\
- Keep the plan as small as the task allows; every node costs a full agent session.\n\n\
Plan file schema (YAML), written to {plan_file}:\n\
  summary: why the graph is shaped this way   # optional, recorded on this task's thread\n\
  nodes:\n\
    - key: schema                # plan-local handle other nodes depend on; not a task id\n\
      title: Add the depends_on field\n\
      description: |\n\
        The full instruction for this node's agent. Everything it needs that is not\n\
        coming from an upstream node belongs here.\n\
      depends_on: []             # plan keys and/or existing TASK-ids\n\
      needs: null                # what this node takes from upstream\n\
      role: null                 # model roster to run on (see below)\n\
      designer: false            # run the designer bot on this node\n\
      reviewer: false            # run the reviewer bot on this node\n\
Submit it with: \"$KANBAN_CMD\" plan {} --file {plan_file} --session {session_id} --agent\n\
The plan is validated before anything is created: unknown references, duplicate keys, cycles,\
 unknown roles and oversized plans are all rejected with a reason, and nothing is created until\
 it passes. Fix the file and submit again.\n\n",
        task.id,
        task.title,
        roots.work_path.display(),
        task.id,
        task.id,
    );
    prompt.push_str(&format!(
        "Limits: at most {} nodes in one plan.\n",
        orch.orchestrator.max_subtasks
    ));
    if orch.roles.is_empty() {
        prompt.push_str(
            "Model rosters: none are configured (orchestration.roles is empty), so leave `role`\
 unset — every node inherits this task's own backend and model.\n\n",
        );
    } else {
        prompt.push_str(
            "Model rosters you may assign with `role` (a node runs on the first entry; if that \
backend hits a subscription limit the board moves the node to the next one automatically):\n",
        );
        for (name, candidates) in &orch.roles {
            prompt.push_str(&format!(
                "- {name}: {}\n",
                candidates
                    .iter()
                    .map(|candidate| candidate.label())
                    .collect::<Vec<_>>()
                    .join(" → ")
            ));
        }
        prompt.push_str(
            "Pick the cheapest roster that can do each node's work; leave `role` unset to \
inherit this task's own backend and model.\n\n",
        );
    }
    prompt.push_str(&format!(
        "Session contract:\n\
- KANBAN_SESSION is set to {session_id}.\n\
- KANBAN_TASK_ID is set to {}.\n\
- KANBAN_CMD is set to the current kanban4ai executable; use \"$KANBAN_CMD\" instead of bare kanban so callbacks target this binary.\n\
- Record your reasoning about the decomposition with: \"$KANBAN_CMD\" context {} <text> --source agent\n\
- Keep the session alive with: \"$KANBAN_CMD\" heartbeat --session {session_id}\n\
- When the plan is accepted, finish with: \"$KANBAN_CMD\" done {} --session {session_id} --agent\n\
  That completes the planning phase only. This task returns to To Do as the graph's join node:\n\
  it waits for every subtask and then runs again as an ordinary executor to integrate and verify.\n\
  Finishing before a plan is accepted is refused.\n\
- Do not implement the task. Do not edit source files. Do not run commands that change the repo.\n\
- Do not move tasks between columns and do not start subtasks yourself; the dispatcher does that\n\
  under the board's concurrency caps.\n\
- Ask whenever the decomposition depends on something you cannot determine: \"$KANBAN_CMD\" ask {} <question> --agent\n\
- This session is non-interactive and terminates the moment you end your reply.\n\
- The final command of your reply must be the done command above, or the ask command if you are blocked.\n\
  Ending a reply without one strands the planning phase and forces an automatic resume.\n\
- Record non-blocking ideas or risks you notice with: \"$KANBAN_CMD\" suggest {} <idea>\n\
- To ask the human one or more questions, prefer a strict YAML form: write {form_file} then submit it with\n  \
\"$KANBAN_CMD\" ask-form {} --file {form_file} --agent --session {session_id}\n",
        task.id, task.id, task.id, task.id, task.id, task.id
    ));
    prompt.push_str(role_column_block(Role::Orchestrator));
    append_role_instructions(roots, Role::Orchestrator, &mut prompt);
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(task.description.trim());
    }
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
}

fn build_reviewer_prompt(roots: Roots<'_>, task: &Task, session_id: &str) -> Result<String> {
    let form_file = roots
        .data_path("forms")
        .join(format!("{}.ask.yaml", task.id));
    let form_file = form_file.display().to_string();

    let mut prompt = format!(
        "Task: {}: {}\n\n\
You are the kanban4ai REVIEWER for project: {}\n\
Work only on task {}. The user-authored task is below.\n\n\
Your job is to check the result, not to implement fixes. Compare the work\
 against the task requirements and the project conventions in AGENTS.md and\
 CLAUDE.md (when those files exist). Read the task thread and context below\
 — that is where the executor recorded what it did, plus any prior review\
 feedback. Do not edit project files. Do not move the task between columns.\n\n\
Session contract:\n\
- KANBAN_SESSION is set to {session_id}.\n\
- KANBAN_TASK_ID is set to {}.\n\
- KANBAN_CMD is set to the current kanban4ai executable; use \"$KANBAN_CMD\" instead of bare kanban so callbacks target this binary.\n\
- Keep the session alive with: \"$KANBAN_CMD\" heartbeat --session {session_id}\n\
- The only way to finish is kanban verdict. Never call done, and never move the task to Done — Done is human-only.\n\
  Approve: \"$KANBAN_CMD\" verdict {} --approve --session {session_id} --agent\n\
  Request changes: \"$KANBAN_CMD\" verdict {} --changes <what to fix> --session {session_id} --agent\n\
  For a longer write-up, put the text in a file and pass --file <path> with --changes.\n\
- Ask whenever clarification is needed with: \"$KANBAN_CMD\" ask {} <question> --agent\n\
- This session is non-interactive and terminates the moment you end your reply; background tasks, \
monitors, and \"notifications\" die with it, so nothing you launch can re-invoke you later.\n\
- The final command of your reply must be one of the verdict commands above, or the ask command if you are blocked.\n\
  Ending a reply without a verdict strands the review and forces an automatic resume.\n",
        task.id,
        task.title,
        roots.work_path.display(),
        task.id,
        task.id,
        task.id,
        task.id,
        task.id
    );
    prompt.push_str(&format!(
        "- Proactively record non-blocking ideas, risks, or better alternatives you notice \
(don't block on them) with: \"$KANBAN_CMD\" suggest {} <idea>\n",
        task.id
    ));
    prompt.push_str(&format!(
        "- To ask the human one or more questions, prefer a strict YAML form over free text so \
each question renders with selectable options. Write {form_file} then submit it:\n  \
\"$KANBAN_CMD\" ask-form {} --file {form_file} --agent --session {session_id}\n",
        task.id
    ));
    if task.interactive {
        prompt.push_str(&format!(
            "- This task is interactive: for blocking questions use \"$KANBAN_CMD\" ask {} <question> --agent --wait --session {}; for non-blocking ideas use \"$KANBAN_CMD\" suggest.\n",
            task.id, session_id
        ));
    }
    prompt.push_str(role_column_block(Role::Reviewer));
    append_role_instructions(roots, Role::Reviewer, &mut prompt);
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(task.description.trim());
    }
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
}

/// The column-ownership contract each role is told, and that `move_task`
/// enforces for designer/reviewer sessions.
fn role_column_block(role: Role) -> &'static str {
    match role {
        Role::Executor => {
            "
Role: executor
Column ownership:
- You may finish with kanban done, which lands the task in Review or starts bot review when the reviewer is on.
- Never move a task to Done. Done is the human's column.
- Do not call kanban move to change columns; done is the completion command.
"
        }
        Role::Orchestrator => {
            "
Role: orchestrator
Column ownership:
- Do not move this task out of In Progress. Do not call kanban move.
- Do not implement the task and do not start any subtask yourself.
- Finish the planning phase only with kanban done, after kanban plan accepted your graph.
"
        }
        Role::Designer => {
            "
Role: designer
Column ownership:
- Do not move this task out of In Progress. Do not call kanban move.
- Do not implement the task.
- Finish the design phase only with kanban done after recording the plan on the thread. That does not complete the work.
"
        }
        Role::Reviewer => {
            "
Role: reviewer
Column ownership:
- Do not move this task. Do not call kanban move or kanban done.
- Do not implement fixes.
- Your only exit is kanban verdict.
"
        }
    }
}

/// Project instructions that belong to one role only, read from
/// `.kanban/instructions/<role>.md`.
///
/// `AGENTS.md` and `CLAUDE.md` are loaded into every agent session, so
/// anything written there is charged to every run on the board — including the
/// runs it does not apply to. A role file is the opposite: only the role it
/// names ever sees it, and only when that role is actually launched. Missing
/// or empty files are simply skipped.
fn append_role_instructions(roots: Roots<'_>, role: Role, prompt: &mut String) {
    let path = roots
        .data_path("instructions")
        .join(format!("{}.md", role.as_str()));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    prompt.push_str(&format!(
        "\nProject instructions for the {} role (from {}); they apply to you only:\n{text}\n",
        role.as_str(),
        path.display()
    ));
}

/// The results of the tasks this one depends on — the context half of a DAG
/// edge.
///
/// Only `depends_on` carries context. `chained_to` deliberately does not: a
/// chain is a human's "run this next", and the two tasks often share nothing
/// but their order. A dependency is the orchestrator saying this node's work
/// *is* downstream of that one, so the node is prompted with what upstream
/// produced instead of starting blind.
///
/// The digest is assembled from board artifacts that already exist — each
/// dependency's recorded context and harvested final reply — run through the
/// same rule-based compaction as everything else, so no model is called to
/// summarize and the result is deterministic. The whole section is capped by
/// `orchestration.orchestrator.upstream_budget_chars`, split evenly across the
/// dependencies, which is the point: a node should read a small distillation
/// of upstream, not its transcript.
fn append_upstream_results(roots: Roots<'_>, task: &Task, prompt: &mut String) -> Result<()> {
    if task.depends_on.is_empty() {
        return Ok(());
    }
    if let Some(needs) = task
        .needs
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        prompt.push_str(&format!(
            "\n\nWhat this task needs from upstream (written by the orchestrator that planned it):\n{needs}\n"
        ));
    }
    let budget = Config::new(roots.data_root)
        .get_orchestration()?
        .orchestrator
        .upstream_budget_chars
        .max(0) as usize;
    if budget == 0 {
        return Ok(());
    }
    let per_dependency = (budget / task.depends_on.len()).max(200);
    let storage = Storage::new(roots.data_root);
    let threads = ThreadManager::new(roots.data_root)?;

    prompt.push_str("\n\nUpstream results (the tasks this one depends on):\n");
    for dependency in &task.depends_on {
        let Some(upstream) = storage.load_task(dependency)? else {
            prompt.push_str(&format!(
                "- {dependency}: no longer on the board; treat its work as unavailable.\n"
            ));
            continue;
        };
        prompt.push_str(&format!(
            "- {} [{}] {}\n",
            upstream.id,
            upstream.status.as_str(),
            upstream.title.trim()
        ));
        let digest = upstream_digest(&threads, &upstream.id, per_dependency)?;
        if digest.is_empty() {
            prompt.push_str("  (finished without recording a result)\n");
            continue;
        }
        for line in digest.lines() {
            prompt.push_str("  ");
            prompt.push_str(line);
            prompt.push('\n');
        }
    }
    Ok(())
}

/// One dependency's result, newest first and truncated to `budget` characters.
/// Newest first because the last thing a task recorded is its conclusion; the
/// earlier entries are the road to it and are the right thing to lose.
fn upstream_digest(threads: &ThreadManager, task_id: &str, budget: usize) -> Result<String> {
    let thread = threads.load(task_id)?;
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;
    for message in thread
        .messages
        .iter()
        .rev()
        .filter(|message| message.kind == MessageKind::Context)
        .filter(|message| message.status != MessageStatus::Rejected)
    {
        let body = compact_text(message.body.trim());
        if body.is_empty() {
            continue;
        }
        let remaining = budget.saturating_sub(used);
        if remaining == 0 {
            kept.push("…(earlier upstream context omitted)".to_string());
            break;
        }
        let body = if body.chars().count() > remaining {
            let truncated: String = body.chars().take(remaining).collect();
            format!("{truncated}…")
        } else {
            body
        };
        used += body.chars().count();
        kept.push(body);
    }
    kept.reverse();
    Ok(kept.join("\n"))
}

fn append_thread_context(roots: Roots<'_>, task: &Task, prompt: &mut String) -> Result<()> {
    let thread = ThreadManager::new(roots.data_root)?.load(&task.id)?;
    let origins_with_agent_context: std::collections::HashSet<_> = thread
        .messages
        .iter()
        .filter(|message| {
            message.kind == MessageKind::Context && message.author.as_deref() == Some("agent")
        })
        .filter_map(|message| message.origin.clone())
        .collect();
    let messages: Vec<_> = thread
        .messages
        .into_iter()
        .filter(|message| {
            !(matches!(
                message.kind,
                MessageKind::System | MessageKind::Task | MessageKind::AgentStep
            ) || message.status == MessageStatus::Rejected
                || message.body.trim().is_empty()
                // The captured whole-session reply frequently repeats context
                // explicitly posted by that same run. Keep the explicit,
                // concise progress record and do not replay the run twice.
                || (message.author.as_deref() == Some("agent-reply")
                    && message.origin.as_ref().is_some_and(|origin| origins_with_agent_context.contains(origin))))
        })
        .collect();
    if messages.is_empty() {
        return Ok(());
    }

    prompt.push_str("\n\nThread context and review feedback:\n");
    for message in messages {
        append_message(prompt, &message);
    }
    Ok(())
}

fn append_thread_delta(
    roots: Roots<'_>,
    task: &Task,
    previous_session_id: &str,
    prompt: &mut String,
) -> Result<()> {
    let thread = ThreadManager::new(roots.data_root)?.load(&task.id)?;
    let previous_origin = format!("agent:{previous_session_id}");
    let start = thread
        .messages
        .iter()
        .rposition(|message| message.origin.as_deref() == Some(previous_origin.as_str()))
        .map_or(0, |index| index + 1);
    let messages: Vec<_> = thread
        .messages
        .into_iter()
        .skip(start)
        .filter(|message| {
            !matches!(
                message.kind,
                MessageKind::System | MessageKind::Task | MessageKind::AgentStep
            ) && message.status != MessageStatus::Rejected
                && !message.body.trim().is_empty()
        })
        .collect();
    if !messages.is_empty() {
        prompt.push_str("\n\nNew thread context since the previous run:\n");
        for message in messages {
            append_message(prompt, &message);
        }
    }
    Ok(())
}

fn append_message(prompt: &mut String, message: &Message) {
    prompt.push_str("- [");
    prompt.push_str(message.role.as_str());
    prompt.push(' ');
    prompt.push_str(message.kind.as_str());
    prompt.push(' ');
    prompt.push_str(&message.id);
    if let Some(author) = message
        .author
        .as_deref()
        .filter(|author| !author.trim().is_empty())
    {
        prompt.push_str(" by ");
        prompt.push_str(author.trim());
    }
    if let Some(origin) = message
        .origin
        .as_deref()
        .filter(|origin| !origin.trim().is_empty())
    {
        prompt.push_str(" origin=");
        prompt.push_str(origin.trim());
    }
    prompt.push_str("] ");
    prompt.push_str(message.body.trim());
    if let Some(answer) = message
        .answer
        .as_deref()
        .filter(|answer| !answer.trim().is_empty())
    {
        prompt.push_str("\n  Answer: ");
        prompt.push_str(answer.trim());
    }
    prompt.push('\n');
}

fn build_revert_prompt(roots: Roots<'_>, task: &Task, session_id: &str) -> String {
    let backups_dir = roots.data_path("backups").join(&task.id);
    format!(
        "Task: {}: revert {}\n\n\
You are a delegated kanban4ai revert agent working in project: {}\n\
Restore every file from {}/ to its original repo-relative path.\n\
Do not make unrelated edits. KANBAN_CMD is set to the current kanban4ai executable; \
use \"$KANBAN_CMD\" instead of bare kanban. After restoring, verify the files exist, \
record context with \"$KANBAN_CMD\" context {} <text> --source agent, then run: \
\"$KANBAN_CMD\" done {} --session {session_id} --agent\n\
KANBAN_SESSION={session_id}\nKANBAN_TASK_ID={}\nKANBAN_CMD=$KANBAN_CMD\n",
        task.id,
        task.title,
        roots.work_path.display(),
        backups_dir.display(),
        task.id,
        task.id,
        task.id
    )
}
