use crate::core::error::Result;
use crate::core::models::{Message, MessageKind, MessageStatus, Role, Task};
use crate::core::project::Roots;
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
- If blocked, ask with: \"$KANBAN_CMD\" ask {} <question> --agent\n\
- This session is non-interactive and terminates the moment you end your reply; background tasks, \
monitors, and \"notifications\" die with it, so nothing you launch can re-invoke you later.\n\
- Long-running foreground commands are safe: the board heartbeats for you while your process runs, \
with no time limit. Prefer blocking in the foreground and collecting every result before you \
continue; never end a reply while anything you launched is still running or unread.\n\
- If a result will take too long to block on (a heavy query, an external job), never start it as \
a plain shell background job: this session's whole process group is killed when your reply ends, \
so backgrounded work silently dies even after you declare a wait. Instead run: \
\"$KANBAN_CMD\" detach {} --session {session_id} --eta <expected-seconds> --note <what you wait for> -- <command> [args...]\n\
  It starts the command fully detached (it survives this session), appends its output to \
{detached_log}, writes the exit code to the matching .status file, and \
declares the wait for you. If you must detach manually instead, launch with setsid and nohup, \
redirect stdin/stdout/stderr away from the terminal to a result file, and then declare the wait \
yourself before ending your reply: \
\"$KANBAN_CMD\" waiting {} --session {session_id} --eta <expected-seconds> --note <what you wait for>\n\
  Either way the board relaunches you after the deadline (eta plus a safety buffer) to check the \
recorded result; declare waiting again from the new session if it needs more time.\n\
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
Schema (options are optional; prompt is required; add as many questions as you need):\n    \
questions:\n      - prompt: <question text>\n        options: [<choice A>, <choice B>]\n      \
- prompt: <another question>\n",
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
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(task.description.trim());
    }
    append_thread_context(roots, task, &mut prompt)?;
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
- If blocked, ask with: \"$KANBAN_CMD\" ask {} <question> --agent\n\
- This session is non-interactive and terminates the moment you end your reply; background tasks, \
monitors, and \"notifications\" die with it, so nothing you launch can re-invoke you later.\n\
- Long-running foreground commands are safe: the board heartbeats for you while your process runs, \
with no time limit. Prefer blocking in the foreground and collecting every result before you \
continue; never end a reply while anything you launched is still running or unread.\n\
- If a result will take too long to block on (a heavy query, an external job), never start it as \
a plain shell background job: this session's whole process group is killed when your reply ends, \
so backgrounded work silently dies even after you declare a wait. Instead run: \
\"$KANBAN_CMD\" detach {} --session {session_id} --eta <expected-seconds> --note <what you wait for> -- <command> [args...]\n\
  It starts the command fully detached (it survives this session), appends its output to \
{detached_log}, writes the exit code to the matching .status file, and \
declares the wait for you. If you must detach manually instead, launch with setsid and nohup, \
redirect stdin/stdout/stderr away from the terminal to a result file, and then declare the wait \
yourself before ending your reply: \
\"$KANBAN_CMD\" waiting {} --session {session_id} --eta <expected-seconds> --note <what you wait for>\n\
  Either way the board relaunches you after the deadline (eta plus a safety buffer) to check the \
recorded result; declare waiting again from the new session if it needs more time.\n\
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
Schema (options are optional; prompt is required; add as many questions as you need):\n    \
questions:\n      - prompt: <question text>\n        options: [<choice A>, <choice B>]\n      \
- prompt: <another question>\n",
        task.id
    ));
    if task.interactive {
        prompt.push_str(&format!(
            "- This task is interactive: for blocking questions use \"$KANBAN_CMD\" ask {} <question> --agent --wait --session {}; for non-blocking ideas use \"$KANBAN_CMD\" suggest.\n",
            task.id, session_id
        ));
    }
    prompt.push_str(role_column_block(Role::Designer));
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
- If blocked, ask with: \"$KANBAN_CMD\" ask {} <question> --agent\n\
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
    prompt.push_str(role_column_block(Role::Reviewer));
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

fn append_thread_context(roots: Roots<'_>, task: &Task, prompt: &mut String) -> Result<()> {
    let thread = ThreadManager::new(roots.data_root)?.load(&task.id)?;
    let messages: Vec<_> = thread
        .messages
        .into_iter()
        .filter(|message| {
            !matches!(
                message.kind,
                MessageKind::System | MessageKind::Task | MessageKind::AgentStep
            ) && message.status != MessageStatus::Rejected
                && !message.body.trim().is_empty()
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
