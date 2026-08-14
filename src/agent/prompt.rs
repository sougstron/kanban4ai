use crate::core::error::Result;
use crate::core::models::{Message, MessageKind, MessageStatus, Task};
use crate::core::project::Roots;
use crate::core::thread::ThreadManager;

/// Assemble the prompt handed to a delegated agent.
///
/// Every board path in it is absolute: the agent's working directory is the
/// code folder, while the board lives under `data_root`, so a relative
/// `.kanban/…` instruction would make the agent write into the user's repo.
pub fn build_agent_prompt<'a>(
    roots: impl Into<Roots<'a>>,
    task: &Task,
    session_id: &str,
    revert: bool,
) -> Result<String> {
    let roots = roots.into();
    if revert {
        return Ok(build_revert_prompt(roots, task, session_id));
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
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(task.description.trim());
    }
    append_thread_context(roots, task, &mut prompt)?;
    Ok(prompt)
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
