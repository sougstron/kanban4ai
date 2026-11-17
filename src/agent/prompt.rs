use crate::core::config::{BoardConfig, Config, OrchestrationSettings, PromptmaxStyle};
use crate::core::context::result_digest;
use crate::core::error::Result;
use crate::core::models::{Message, MessageKind, MessageStatus, Role, Task, TaskStatus};
use crate::core::project::Roots;
use crate::core::storage::Storage;
use crate::core::thread::ThreadManager;

/// Session-contract line that replaces the usual "ask when unclear" instruction
/// while silence mode is on. The full rule is [`silence_mode_note`].
fn clarification_line(task: &Task) -> String {
    if task.silence {
        "- Silence mode: do not ask the human. Decide unclear points yourself and record each dispute with suggest (details below).\n".to_string()
    } else {
        format!(
            "- Ask whenever clarification is needed with: \"$KANBAN_CMD\" ask {} <question> --agent\n",
            task.id
        )
    }
}

/// Shared "how to finish" clause for executor and designer prompts. The
/// surrounding sentence names what an unfinished reply strands.
fn long_wait_finish_clause(silence: bool) -> &'static str {
    if silence {
        "the done command above, or kanban wait that declared waiting, or the waiting/detach command if you are waiting on a declared long-running result. Do not ask; a question is refused"
    } else {
        "the done command above, the ask command if you are blocked, or kanban wait that declared waiting, or the waiting/detach command if you are waiting on a declared long-running result"
    }
}

fn silence_mode_note(task_id: &str) -> String {
    format!(
        "\nSilence mode (mandatory): do not ask the human anything. \"$KANBAN_CMD\" ask and \
\"$KANBAN_CMD\" ask-form are refused for this task. Decide every ambiguous point yourself, \
using the task text and the project conventions, and keep working. Record each decision you \
were unsure about with: \"$KANBAN_CMD\" suggest {task_id} <what you decided and the alternatives> \
so the human can review it later. Do not stop for clarification.\n"
    )
}

/// Ask-form guidance, or the silence rule when questions are forbidden.
fn append_question_guidance(
    prompt: &mut String,
    task: &Task,
    session_id: &str,
    form_file: &str,
    schema_help: bool,
) {
    if task.silence {
        prompt.push_str(&silence_mode_note(&task.id));
        return;
    }
    prompt.push_str(&format!(
        "- To ask the human one or more questions, prefer a strict YAML form over free text so \
each question renders with selectable options. Write {form_file} then submit it:\n  \
\"$KANBAN_CMD\" ask-form {} --file {form_file} --agent --session {session_id}\n",
        task.id
    ));
    if schema_help {
        prompt.push_str("  Schema and examples: \"$KANBAN_CMD\" ask-form --help.\n");
    }
}

fn append_interactive_wait(prompt: &mut String, task: &Task, session_id: &str) {
    if task.silence || !task.interactive {
        return;
    }
    prompt.push_str(&format!(
        "- This task is interactive: for blocking questions use \"$KANBAN_CMD\" ask {} <question> --agent --wait --session {}; for non-blocking ideas use \"$KANBAN_CMD\" suggest.\n",
        task.id, session_id
    ));
}

/// Context-economy rules shared by every role: keep tool output small without
/// hiding what the task needs. This text is charged to every run, so keep it
/// tight.
fn append_context_economy(prompt: &mut String) {
    prompt.push_str(
        "\nContext economy (keep tool output small; never hide what the task needs):\n\
- Start narrow: search first and read only the files and line ranges you need; never re-read output you already have without a reason.\n\
- Large outputs (test runs, logs, diffs, wide searches): print a short summary with a limited number of examples/failures/conflicts, write the full output to a file, and name its path; then read only the slices you need.\n\
- Never hide errors or facts needed to solve the task, and never silently cap stdout of external tools — if you truncate, say so and point to the full output.\n\
- kanban4ai's own large reports have a compact mode with the full result kept reachable: \"$KANBAN_CMD\" stats --compact.\n",
    );
}

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
        Role::Promptmaxer => return build_promptmaxer_prompt(roots, task, session_id),
        Role::Designer => return build_designer_prompt(roots, task, session_id),
        Role::Reviewer => return build_reviewer_prompt(roots, task, session_id),
        Role::Executor if task.is_subagent() => {
            return build_subagent_prompt(roots, task, session_id);
        }
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
    let clarification_line = clarification_line(task);
    let finish_clause = long_wait_finish_clause(task.silence);

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
{clarification_line}\
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
- The final command of your reply must be {finish_clause}. \
Ending a reply without one of those strands the task and forces an automatic resume.\n",
        task.id,
        task.title,
        roots.work_path.display(),
        task.id,
        task.id,
        task.id,
        task.id,
        task.id,
        task.id
    );
    prompt.push_str(&format!(
        "- Proactively record non-blocking ideas, risks, or better alternatives you notice \
(don't block on them, keep working) with: \"$KANBAN_CMD\" suggest {} <idea>\n\
- If the outcome is files for the human to look at (a report, generated images), keep them in one \
folder that is not gitignored (or outside the project) and pin it with: \"$KANBAN_CMD\" result {} <path> \
— the human opens it from Review with o. One path per task; pinning again replaces it.\n",
        task.id, task.id
    ));
    append_question_guidance(&mut prompt, task, session_id, &form_file, true);
    append_context_economy(&mut prompt);
    if task.permission() == Some("readonly") {
        prompt.push_str(
            "\nReadonly mode (mandatory): investigate and report, but do not create, edit, \
delete, rename, or move any project file, including plans, notes, generated files, and build \
artifacts. Do not run formatters, fixers, builds, tests, or commands that write into the project. \
Board operations remain fully available: you may create or edit tasks and use the required \
callbacks through kanban4ai, including finishing this task as instructed below. The usual role \
and column rules still apply. ",
        );
        if task.silence {
            prompt.push_str(
                "Questions are refused, so do not write an ask form. If the task appears to \
require a project-file change, report the proposed change through context or suggest instead \
of applying it.\n",
            );
        } else {
            prompt.push_str(
                "The only other permitted write is the absolute `.kanban` form path given above. \
If the task appears to require a project-file change, report the proposed change through \
context or suggest instead of applying it.\n",
            );
        }
    }
    append_interactive_wait(&mut prompt, task, session_id);
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
        prompt.push_str(&absolute_asset_links(roots, task.description.trim()));
    }
    append_promptmax_brief(roots, task, &mut prompt);
    append_upstream_results(roots, task, &mut prompt)?;
    append_spawn_helpers(roots, task, &mut prompt)?;
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
}

/// Prompt of a subagent (TASK-594, TASK-595): a child run inside its
/// parent's execution. Deliberately short (≤ 2 KB without the task text,
/// pinned by `tests/subagent_prompt_test.rs`): no
/// board, column, backup, result-pin, reviewer, wait or spawn rules, no
/// project-wide instructions, no parent thread or upstream digest. The
/// subagent sees its task, `instructions/subagent.md`, and only the context
/// its parent handed over with `sub spawn --context`.
fn build_subagent_prompt(roots: Roots<'_>, task: &Task, session_id: &str) -> Result<String> {
    let mut prompt = subagent_contract(roots, task, session_id);
    append_subagent_instructions(roots, &mut prompt);
    if let Some(context) = task.sub_context.as_deref() {
        prompt.push_str("\nContext from parent:\n");
        prompt.push_str(context.trim());
        prompt.push('\n');
    }
    prompt.push_str("\nTask:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(&absolute_asset_links(roots, task.description.trim()));
    }
    // Its own thread only: follow-ups the parent sent with `sub send` and
    // what earlier runs of this subagent recorded.
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
}

/// The fixed part of a subagent's prompt: who it is, where it works, and the
/// one way it finishes.
fn subagent_contract(roots: Roots<'_>, task: &Task, session_id: &str) -> String {
    let parent = task.parent_task.as_deref().unwrap_or("-");
    let mut prompt = format!(
        "Subagent {id}: {title}\n\
You are subagent {id} of kanban4ai task {parent}. Work in: {work}\n\
KANBAN_SESSION={session_id}; use \"$KANBAN_CMD\" for kanban commands.\n",
        id = task.id,
        title = task.title,
        work = roots.work_path.display(),
    );
    if task.branch.is_some() {
        prompt.push_str(if task.seed_commit.is_some() {
            "- Your checkout is your own branch, seeded from the parent's worktree (its uncommitted edits included). Commit freely; the parent decides whether to merge it.\n"
        } else {
            "- Your checkout is your own branch, cut from the project. Commit freely; the parent decides whether to merge it.\n"
        });
    }
    if task.permission() == Some("readonly") {
        prompt.push_str(
            "- Readonly (mandatory): investigate and report; do not modify project files or run commands that write into the project.\n",
        );
    }
    prompt.push_str(&format!(
        "- Run every command in the foreground and wait for it, however long: your session ends with your reply and background jobs die with it.\n\
- Never ask: nobody answers. Decide unclear points yourself and name them in the summary.\n\
- Finish, as the final command of your reply, with:\n\
  \"$KANBAN_CMD\" sub done {id} --session {session_id} --summary \"<result>\"\n\
  The summary is all the parent reads: what you found or changed, where, and what stays unclear.\n",
        id = task.id,
    ));
    prompt
}

/// `.kanban/instructions/subagent.md`: instructions only subagents read.
/// The project-wide `instructions:` and the other role files never reach a
/// subagent.
fn append_subagent_instructions(roots: Roots<'_>, prompt: &mut String) {
    let path = roots.data_path("instructions").join("subagent.md");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let text = text.trim();
    if !text.is_empty() {
        prompt.push_str(&format!(
            "\nProject instructions for subagents (from {}):\n{text}\n",
            path.display()
        ));
    }
}

/// Small follow-up sent when Codex/pi/omp reopen their native conversation. The
/// backend already has the original task, rules, tool history, and its own
/// replies. Refresh the board session identity and current project instructions,
/// then append only the thread delta.
pub fn build_resume_prompt<'a>(
    roots: impl Into<Roots<'a>>,
    task: &Task,
    session_id: &str,
    previous_session_id: &str,
    role: Role,
) -> Result<String> {
    let roots = roots.into();
    if task.is_subagent() {
        let mut prompt = format!(
            "Resume subagent {id}. The board session changed; earlier session ids are superseded.\n\
KANBAN_SESSION={session_id}. Finish, as the final command of your reply, with: \
\"$KANBAN_CMD\" sub done {id} --session {session_id} --summary \"<result>\". \
Continue from the latest state; do not repeat completed work.",
            id = task.id
        );
        if task.permission() == Some("readonly") {
            prompt.push_str("\nReadonly remains mandatory: do not modify project files.");
        }
        append_subagent_instructions(roots, &mut prompt);
        append_thread_delta(roots, task, previous_session_id, &mut prompt)?;
        return Ok(prompt);
    }
    let finish = match role {
        Role::Executor => format!(
            "Finish with: \"$KANBAN_CMD\" done {} --session {session_id} --agent",
            task.id
        ),
        Role::Orchestrator => format!(
            "Submit the plan with \"$KANBAN_CMD\" plan {} --file <plan.yaml> --session {session_id} --agent, then finish with: \"$KANBAN_CMD\" done {} --session {session_id} --agent",
            task.id, task.id
        ),
        Role::Promptmaxer => format!(
            "Write the brief to {}, then finish with: \"$KANBAN_CMD\" done {} --session {session_id} --agent",
            roots.promptmax_brief(&task.id).display(),
            task.id
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
    if task.permission() == Some("readonly") {
        prompt.push_str(
            "\nReadonly mode remains mandatory: do not modify project files or run commands that \
write into the project. Board operations remain fully available through kanban4ai, including \
creating or editing tasks and finishing this task; usual role and column rules still apply.",
        );
    }
    if task.silence {
        prompt.push_str(
            "\nSilence mode remains mandatory: do not ask. Decide unclear points yourself and \
record each dispute with \"$KANBAN_CMD\" suggest.",
        );
    }
    append_project_instructions(roots, &mut prompt);
    if role == Role::Executor {
        append_open_subagents(roots, task, &mut prompt)?;
    }
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
    let clarification_line = clarification_line(task);
    let finish_clause = long_wait_finish_clause(task.silence);

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
{clarification_line}\
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
- The final command of your reply must be {finish_clause}. \
Ending a reply without one of those strands the design phase and forces an automatic resume.\n",
        task.id,
        task.title,
        roots.work_path.display(),
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
    append_question_guidance(&mut prompt, task, session_id, &form_file, true);
    append_context_economy(&mut prompt);
    append_interactive_wait(&mut prompt, task, session_id);
    prompt.push_str(role_column_block(Role::Designer));
    append_role_instructions(roots, Role::Designer, &mut prompt);
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(&absolute_asset_links(roots, task.description.trim()));
    }
    append_promptmax_brief(roots, task, &mut prompt);
    append_upstream_results(roots, task, &mut prompt)?;
    append_spawn_helpers(roots, task, &mut prompt)?;
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
}

/// The promptmaxer's brief, appended after the user task for the phases that
/// run after it. Only a finished pass (`promptmaxed`) counts, so a stale file
/// from an earlier run never reaches a task that has not been promptmaxed again.
fn append_promptmax_brief(roots: Roots<'_>, task: &Task, prompt: &mut String) {
    if !task.promptmaxed {
        return;
    }
    let path = roots.promptmax_brief(&task.id);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    prompt.push_str(&format!(
        "\nPrepared brief (written for you by the promptmaxer after researching this task; from {}). \
Follow it; where it disagrees with the user task above, the user task wins:\n{text}\n",
        path.display()
    ));
}

/// Promptmax-pass prompt: an optional pre-launch bot that researches the task
/// and writes a self-contained English brief for the executor (and the
/// designer, when one runs next) to `.kanban/prompts/<task>.md`. Like the
/// designer it never implements and finishes with `kanban done`, which hands
/// the slot to the next phase. `style` picks how much it spells out.
fn build_promptmaxer_prompt(roots: Roots<'_>, task: &Task, session_id: &str) -> Result<String> {
    let config = Config::new(roots.data_root);
    let orch = config.get_orchestration()?;
    let board = config.load()?;
    let brief = roots.promptmax_brief(&task.id).display().to_string();
    let form_file = roots
        .data_path("forms")
        .join(format!("{}.ask.yaml", task.id))
        .display()
        .to_string();
    let detached_log = roots
        .data_path("detached")
        .join("<task>-<stamp>.log")
        .display()
        .to_string();
    let clarification_line = clarification_line(task);
    let finish_clause = long_wait_finish_clause(task.silence);
    let id = &task.id;

    let mut prompt = format!(
        "Task: {id}: {title}\n\n\
You are the kanban4ai PROMPTMAXER for project: {work}\n\
Work only on task {id}. The user-authored task is below.\n\n\
Your job is to research, not to implement. Another model (the executor) will do the work right \
after you, starting from the user task plus the brief you write. Find every piece of hidden, \
contextual knowledge it would otherwise have to dig up or guess — what to take, which stack, \
where to go and where not to, how to split the work, whether to use subagents — and write it \
down as one self-contained brief, so the executor has no questions left and simply does the \
job well.\n\n\
Session contract:\n\
- KANBAN_SESSION is set to {session_id}.\n\
- KANBAN_TASK_ID is set to {id}.\n\
- KANBAN_CMD is set to the current kanban4ai executable; use \"$KANBAN_CMD\" instead of bare kanban so callbacks target this binary.\n\
- Write the brief, as Markdown, to: {brief}\n\
  Create the folder if it is missing. That file (and the ask form below) is the only thing you write.\n\
- Then record a one-line summary with: \"$KANBAN_CMD\" context {id} <text> --source agent\n\
- Keep the session alive with: \"$KANBAN_CMD\" heartbeat --session {session_id}\n\
- When the brief is written, finish with: \"$KANBAN_CMD\" done {id} --session {session_id} --agent\n\
  That completes the promptmax phase only; done is refused while the brief file is empty. The \
task stays In Progress and the board starts the next bot with your brief in its prompt.\n\
- Do not implement the task. Do not edit, create, or delete project files. Do not run commands \
that change the repo (builds and tests that only read are fine when they reveal something the \
brief needs).\n\
- Do not move the task between columns (no take/move to Review/Done).\n\
{clarification_line}\
- This session is non-interactive and terminates the moment you end your reply; background tasks, \
monitors, and \"notifications\" die with it, so nothing you launch can re-invoke you later.\n\
- Long-running foreground commands are safe: the board heartbeats for you while your process runs, \
with no time limit. Prefer blocking in the foreground and collecting every result before you \
continue; never end a reply while anything you launched is still running or unread.\n\
- If a result will take too long to block on, never start it as a plain shell background job: \
this session's whole process group dies with your reply. Instead run: \
\"$KANBAN_CMD\" detach {id} --session {session_id} --eta <expected-seconds> --note <what you wait for> -- <command> [args...]\n\
  It survives this session, logs to {detached_log} plus a .status file, and declares the wait. \
To wait on something you did not detach, use \
\"$KANBAN_CMD\" waiting {id} --session {session_id} --eta <expected-seconds> --note <what you wait for>\n\
  Either way the board relaunches you after the deadline to check the result. \
Details: \"$KANBAN_CMD\" detach --help, \"$KANBAN_CMD\" waiting --help.\n\
- The final command of your reply must be {finish_clause}. \
Ending a reply without one of those strands the promptmax phase and forces an automatic resume.\n\
- Proactively record non-blocking ideas, risks, or better alternatives you notice with: \
\"$KANBAN_CMD\" suggest {id} <idea>\n",
        title = task.title,
        work = roots.work_path.display(),
    );
    append_question_guidance(&mut prompt, task, session_id, &form_file, true);
    if !task.silence {
        prompt.push_str(
            "- Ask only what the repo, the thread, and the task cannot answer and what would \
change the brief materially (scope, an ambiguous requirement, a choice between designs). Batch \
everything into one ask form, wait for the answer, and write the answers into the brief as \
decisions. Never ask what you can look up yourself.\n",
        );
    }
    append_context_economy(&mut prompt);
    append_interactive_wait(&mut prompt, task, session_id);
    prompt.push_str(role_column_block(Role::Promptmaxer));
    append_promptmax_run_facts(roots, task, &orch, &board, &mut prompt)?;
    prompt.push_str(PROMPTMAX_BRIEF_RULES);
    prompt.push_str(match orch.promptmax.style {
        PromptmaxStyle::Mega => PROMPTMAX_STYLE_MEGA,
        PromptmaxStyle::Expanded => PROMPTMAX_STYLE_EXPANDED,
    });
    append_role_instructions(roots, Role::Promptmaxer, &mut prompt);
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(&absolute_asset_links(roots, task.description.trim()));
    }
    append_upstream_results(roots, task, &mut prompt)?;
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
}

/// What the brief must plan around: who runs it, under which permission, and
/// what the subagent settings of this task allow.
fn append_promptmax_run_facts(
    roots: Roots<'_>,
    task: &Task,
    orch: &OrchestrationSettings,
    board: &BoardConfig,
    prompt: &mut String,
) -> Result<()> {
    use crate::core::config::SpawnPolicy;
    let executor = crate::agent::backends::resolve_task_launch_settings(board, task)?;
    let model = executor.model.as_deref().unwrap_or("backend default");
    let mut facts = format!(
        "\nRun facts (plan the brief around them):\n\
- Executor: backend {}, model {model}{}.\n",
        executor.backend,
        executor
            .effort
            .as_deref()
            .map(|e| format!(", effort {e}"))
            .unwrap_or_default(),
    );
    if orch.designer_enabled_for(task) && !task.designed {
        facts.push_str(
            "- A designer bot runs after you and before the executor; it reads your brief and \
records the implementation plan. Still give the full breakdown: the designer refines it.\n",
        );
    }
    if orch.reviewer_enabled_for(task) {
        facts.push_str("- A reviewer bot checks the result before the human sees it.\n");
    }
    facts.push_str(if task.permission() == Some("readonly") {
        "- Permission: readonly. The executor investigates and reports; it must not change project files. Plan an investigation, not edits.\n"
    } else {
        "- Permission: the executor may edit project files.\n"
    });
    if task.silence {
        facts.push_str("- Silence mode: the executor cannot ask the human, so the brief must settle every open point.\n");
    }
    if let (Some(branch), Some(_)) = (&task.branch, &task.worktree) {
        facts.push_str(&format!(
            "- Isolation: the executor works in its own git worktree on branch {branch} and commits there.\n"
        ));
    }
    let effective =
        crate::core::operations::Operations::new(roots.data_root).effective_spawn(task)?;
    if effective.policy == SpawnPolicy::Forbidden || effective.max_children == 0 {
        facts.push_str("- Subagents: forbidden for this task. Plan the work for a single agent and say so in the brief.\n");
    } else {
        facts.push_str(&format!(
            "- Subagents: {} for this task (policy {}), at most {} per run, {} running at once. \
Each subagent starts on its own branch seeded from the executor's worktree and sees only its \
description and the context excerpts handed to it.\n",
            if effective.requires_helper {
                "mandatory (at least one)"
            } else {
                "allowed"
            },
            effective.policy.as_str(),
            effective.max_children,
            orch.subagents.max_parallel_per_parent,
        ));
    }
    prompt.push_str(&facts);
    Ok(())
}

/// Rules every brief follows, whatever its style.
const PROMPTMAX_BRIEF_RULES: &str = "
How to research:
- Read the task and the thread first, then the repo: entry points, the modules the change \
touches, the closest existing feature to copy, its tests, docs and config surfaces. Verify every \
path, symbol and command you name by actually opening or running it; never cite from memory.
- Hunt for what is not written in the task: conventions the code follows, invariants and \
compatibility constraints, generated or snapshot files that must change together, places that \
must stay untouched, and the traps an outsider would fall into.
- AGENTS.md, CLAUDE.md and similar agent files are already loaded into the executor's session. \
Do not restate them; name a rule from them only when it is decisive for this task.

How to write the brief:
- English only, strictly, even when the task is written in another language. Quote user-facing \
strings, identifiers or file contents verbatim in their original language when the result must \
contain them literally.
- Address the executor directly (\"Edit X\", \"Do not touch Y\"). It also gets the user task \
text, so restate the goal precisely instead of copying the task.
- Ground everything in `path:line` anchors and exact names. Mark anything you could not verify \
as an assumption, and say how the executor should check it.
- Settle ambiguity: record each decision you made (or the human's answer) with a one-line reason \
and the rejected alternative.
- Response language (mandatory section): name the language the user task is written in and \
instruct the executor to write everything addressed to people — context notes, suggestions, \
questions and the final summary — in that language. Code, identifiers, comments and commit \
messages follow the repo's conventions.

Brief layout (keep this order and these headings; drop a section only when it is truly empty):
1. Goal — the outcome in a few sentences and what \"done\" looks like to the user.
2. Response language.
3. Scope — in scope; out of scope; files, modules and behaviour that must not change.
4. Stack and environment — languages, frameworks, tools and versions actually in use, and the \
exact build, lint and test commands of this repo.
5. Codebase map — files and symbols to read and change, each with `path:line` and why; the \
existing implementation to mirror.
6. Hidden knowledge — conventions, invariants, gotchas, coupled files (tests, snapshots, \
fixtures, docs, config, migrations) and anything else an outsider would miss.
7. Work breakdown — ordered subtasks; each with the files it touches, the change, and how to \
verify it before moving on.
8. Subagents — the decision and why. \"None\" with a reason, or a numbered list: title, readonly \
or not, the exact description to pass, the context files to hand over, and how the executor \
merges or uses the result. Respect the run facts above: never exceed the caps, plan at least one \
when they are mandatory, none when forbidden. Use subagents only for independent, sizeable \
parallel work; never for something the executor finishes in a few steps.
9. Verification — exact commands with the expected result, plus any manual or visual check.
10. Decisions, assumptions and risks.
11. Definition of done — a checklist the executor ticks off before kanban done.
";

/// Expanded brief: what a capable model would look up first, nothing more.
const PROMPTMAX_STYLE_EXPANDED: &str = "
Style: EXPANDED. The executor is a capable model. Front-load what it would otherwise search for \
first: the exact entry points and files, the reference implementation to copy, the scope fence, \
the commands to build and test, and the non-obvious traps. Keep every section tight — bullets \
with `path:line`, one line per fact — and skip what a competent engineer reads off the code at a \
glance. Spend the words on what is surprising, not on what is obvious. Usually 80–200 lines.
";

/// Mega brief: everything spelled out so the weakest model cannot go wrong.
const PROMPTMAX_STYLE_MEGA: &str = "
Style: MEGA. Write for the weakest model that might run this brief: it follows instructions \
literally, does not explore on its own, and fills every gap with a guess. Leave nothing to \
inference:
- For every subtask give the file, the function, struct or section with its line anchor, the \
existing code it must mirror (quote the few lines that show the pattern), the exact edits in \
order, and the names to use for anything new.
- Spell out every edge case and how it must behave, every error path, and every place that must \
change together with the edit (call sites, tests, snapshots, docs, config defaults, CLI help).
- For each convention the change touches (naming, error handling, logging, formatting, tests, \
docs) give one concrete example from this repo.
- Give every command exactly as it must be typed, where to run it, and what success looks like; \
say what to do when it fails.
- Add a \"Common mistakes to avoid\" list and finish with a step-by-step checklist.
- Quote only the lines that matter — never whole files. Length is no concern; precision and \
completeness are.
";

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
    // The assignable profiles are the configured rosters *plus* the board's
    // executor pools, which is what `apply_plan` accepts — a prompt that only
    // listed `orchestration.roles` left an empty-roles board no way to ask for
    // the cheap pool, so every node silently ran on the planner's own model.
    let rosters: Vec<(String, String)> = orch
        .known_role_names()
        .into_iter()
        .filter_map(|name| {
            let label = orch
                .roster(&name)?
                .iter()
                .map(|candidate| candidate.label())
                .collect::<Vec<_>>()
                .join(" → ");
            Some((name, label))
        })
        .collect();
    let default_role = orch
        .orchestrator
        .default_role
        .profile()
        .filter(|profile| orch.roster(profile).is_some());
    if rosters.is_empty() {
        prompt.push_str(
            "Model rosters: none are configured (orchestration.roles and \
orchestration.executors are empty), so leave `role` unset — every node inherits this task's \
own backend and model.\n\n",
        );
    } else {
        prompt.push_str(
            "Model rosters you may assign with `role` (a node runs on the first entry; if that \
backend hits a subscription limit the board moves the node to the next one automatically):\n",
        );
        for (name, label) in &rosters {
            prompt.push_str(&format!("- {name}: {label}\n"));
        }
        match default_role {
            Some(default_role) => prompt.push_str(&format!(
                "Pick the cheapest roster that can do each node's work; a node with `role` unset \
runs on `{default_role}`. This task's own (larger) model is not inherited — ask for a roster \
explicitly when a node truly needs one.\n\n"
            )),
            None => prompt.push_str(
                "Pick the cheapest roster that can do each node's work; leave `role` unset to \
inherit this task's own backend and model.\n\n",
            ),
        }
    }
    let decompose_clause = if task.silence {
        "Silence mode: do not ask. When the decomposition is ambiguous, choose the graph yourself and record each fork with suggest (details below).".to_string()
    } else {
        format!(
            "Ask whenever the decomposition depends on something you cannot determine: \"$KANBAN_CMD\" ask {} <question> --agent",
            task.id
        )
    };
    let plan_finish = if task.silence {
        "The final command of your reply must be the done command above. Do not ask; a question is refused."
    } else {
        "The final command of your reply must be the done command above, or the ask command if you are blocked."
    };
    let ask_form_tail = if task.silence {
        silence_mode_note(&task.id)
    } else {
        format!(
            "- To ask the human one or more questions, prefer a strict YAML form: write {form_file} then submit it with\n  \
\"$KANBAN_CMD\" ask-form {} --file {form_file} --agent --session {session_id}\n",
            task.id
        )
    };
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
- {decompose_clause}\n\
- This session is non-interactive and terminates the moment you end your reply.\n\
- {plan_finish}\n\
  Ending a reply without one strands the planning phase and forces an automatic resume.\n\
- Record non-blocking ideas or risks you notice with: \"$KANBAN_CMD\" suggest {} <idea>\n\
{ask_form_tail}",
        task.id, task.id, task.id, task.id
    ));
    prompt.push_str(role_column_block(Role::Orchestrator));
    append_context_economy(&mut prompt);
    append_role_instructions(roots, Role::Orchestrator, &mut prompt);
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(&absolute_asset_links(roots, task.description.trim()));
    }
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
}

fn build_reviewer_prompt(roots: Roots<'_>, task: &Task, session_id: &str) -> Result<String> {
    let form_file = roots
        .data_path("forms")
        .join(format!("{}.ask.yaml", task.id));
    let form_file = form_file.display().to_string();
    let clarification_line = clarification_line(task);
    let verdict_finish = if task.silence {
        "The final command of your reply must be one of the verdict commands above. Do not ask; a question is refused."
    } else {
        "The final command of your reply must be one of the verdict commands above, or the ask command if you are blocked."
    };

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
{clarification_line}\
- This session is non-interactive and terminates the moment you end your reply; background tasks, \
monitors, and \"notifications\" die with it, so nothing you launch can re-invoke you later.\n\
- {verdict_finish}\n\
  Ending a reply without a verdict strands the review and forces an automatic resume.\n",
        task.id,
        task.title,
        roots.work_path.display(),
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
    append_question_guidance(&mut prompt, task, session_id, &form_file, false);
    append_context_economy(&mut prompt);
    append_interactive_wait(&mut prompt, task, session_id);
    prompt.push_str(role_column_block(Role::Reviewer));
    append_role_instructions(roots, Role::Reviewer, &mut prompt);
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(&absolute_asset_links(roots, task.description.trim()));
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
        Role::Promptmaxer => {
            "
Role: promptmaxer
Column ownership:
- Do not move this task out of In Progress. Do not call kanban move.
- Do not implement the task.
- Finish the promptmax phase only with kanban done after writing the brief. That does not complete the work.
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

/// Read the current project-wide settings for fresh launches and native resumes.
fn append_project_instructions(roots: Roots<'_>, prompt: &mut String) {
    if let Ok(Some(text)) = Config::new(roots.data_root).get_instructions() {
        prompt.push_str(&format!(
            "\nProject instructions (from Project Settings); they apply to every task in this project and supersede earlier Project Settings instructions:\n{text}\n"
        ));
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
///
/// The project-wide instructions from Project Settings (`instructions:` in
/// `.kanban/config.yaml`) come first: every role of every task on the board
/// gets them whenever they are not blank.
fn append_role_instructions(roots: Roots<'_>, role: Role, prompt: &mut String) {
    append_project_instructions(roots, prompt);
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
        let digest = result_digest(&threads, &upstream.id, per_dependency)?;
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
        append_message(roots, prompt, &message);
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
            append_message(roots, prompt, &message);
        }
    }
    Ok(())
}

fn append_message(roots: Roots<'_>, prompt: &mut String, message: &Message) {
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
    prompt.push_str(&absolute_asset_links(roots, message.body.trim()));
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

/// Pasted images are stored as `![…](.kanban/assets/…)` relative to the data
/// root, but agents run in the code folder or a worktree where that path does
/// not resolve; point them at the absolute file instead.
fn absolute_asset_links(roots: Roots<'_>, text: &str) -> String {
    let assets = roots.data_path("assets");
    text.replace("](.kanban/assets/", &format!("]({}/", assets.display()))
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

fn append_spawn_helpers(roots: Roots<'_>, task: &Task, prompt: &mut String) -> Result<()> {
    use crate::core::config::SpawnPolicy;
    if Role::from_phase(task.run_phase) != Role::Executor {
        return Ok(());
    }
    let ops = crate::core::operations::Operations::new(roots.data_root);
    let effective = ops.effective_spawn(task)?;
    // Only advertise subagents when spawning is actually possible: a
    // Forbidden policy or a zero max_children would refuse every attempt.
    if effective.policy != SpawnPolicy::Forbidden && effective.max_children > 0 {
        prompt.push_str(&format!(
            "\nSubagents (side work inside your own run; a static graph of visible tasks is kanban plan, not this):\n\
1. \"$KANBAN_CMD\" sub spawn --parent {id} <title> --description <instructions> [--context <file>]... [--readonly]. \
It starts at once off the board, on its own branch seeded from your worktree, and sees only the description and the --context excerpts you give it.\n\
2. \"$KANBAN_CMD\" sub wait declares the wait (exit 3): end your reply. You are relaunched when all of them finish; there is no timer.\n\
3. \"$KANBAN_CMD\" sub output <id> shows its summary, diffstat and changed files.\n\
4. \"$KANBAN_CMD\" sub merge <id> into your worktree or \"$KANBAN_CMD\" sub discard <id>; \"$KANBAN_CMD\" sub send <id> <message> asks it a follow-up.\n\
Never put waiting --eta on subagents, never redo their work, never run done for them. Your own done is refused while they are live.\n",
            id = task.id
        ));
        if effective.requires_helper {
            prompt.push_str("Spawn policy (mandatory): create at least one subagent with kanban sub spawn before done; done is refused otherwise.\n");
        }
    }
    append_open_subagents(roots, task, prompt)?;
    append_legacy_helpers(roots, task, prompt)
}

/// The parent's subagents it still has to deal with — not merged or
/// discarded yet, or failed — as the same compact table its wake note shows.
fn append_open_subagents(roots: Roots<'_>, task: &Task, prompt: &mut String) -> Result<()> {
    let table =
        crate::core::operations::Operations::new(roots.data_root).subagent_table(&task.id, true)?;
    if !table.is_empty() {
        prompt.push_str("\nYour subagents still to collect:\n");
        prompt.push_str(&table);
    }
    Ok(())
}

/// Legacy `spawned: true` helpers (visible board tasks) keep their result
/// digest.
fn append_legacy_helpers(roots: Roots<'_>, task: &Task, prompt: &mut String) -> Result<()> {
    let ops = crate::core::operations::Operations::new(roots.data_root);
    let children: Vec<_> = ops
        .storage
        .get_all_tasks()?
        .into_iter()
        .filter(|child| {
            child.spawned && !child.is_subagent() && child.parent_task.as_deref() == Some(&task.id)
        })
        .collect();
    if children.is_empty() {
        return Ok(());
    }
    prompt.push_str("\nSpawned helpers (board tasks):\n");
    let budget = ops
        .config
        .get_orchestration()?
        .orchestrator
        .upstream_budget_chars
        .max(0) as usize
        / children.len();
    let threads = ThreadManager::new(roots.data_root)?;
    for child in children {
        prompt.push_str(&format!(
            "- {} [{}]: {}\n",
            child.id, child.status, child.title
        ));
        if matches!(child.status, TaskStatus::Review | TaskStatus::Done) {
            prompt.push_str(&result_digest(&threads, &child.id, budget)?);
            prompt.push('\n');
        }
    }
    Ok(())
}
