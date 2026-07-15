use std::path::Path;

use crate::core::error::Result;
use crate::core::models::{Message, MessageKind, Task};
use crate::core::thread::ThreadManager;

pub fn build_agent_prompt(
    project_path: &Path,
    task: &Task,
    session_id: &str,
    revert: bool,
) -> Result<String> {
    if revert {
        return Ok(build_revert_prompt(project_path, task, session_id));
    }

    let mut prompt = format!(
        "Task: {}: {}\n\n\
You are a delegated kanban4ai agent working in project: {}\n\
Work only on task {}. The user-authored task is below.\n\n\
Session contract:\n\
- KANBAN_SESSION is set to {session_id}.\n\
- KANBAN_TASK_ID is set to {}.\n\
- KANBAN_CMD is set to the current kanban4ai executable; use \"$KANBAN_CMD\" instead of bare kanban so callbacks target this binary.\n\
- Before editing an existing file, copy it to .kanban/backups/{}/ preserving the repo-relative path.\n\
- Record important progress with: \"$KANBAN_CMD\" context {} <text> --source agent\n\
- Keep the session alive with: \"$KANBAN_CMD\" heartbeat --session {session_id}\n\
- When implementation and verification are complete, run: \"$KANBAN_CMD\" done {} --session {session_id} --agent\n\
- If blocked, ask with: \"$KANBAN_CMD\" ask {} <question> --agent\n\
- This session is non-interactive and terminates the moment you end your reply; background tasks, \
monitors, and \"notifications\" die with it, so nothing you launch can re-invoke you later.\n\
- Long-running foreground commands are safe: the board heartbeats for you while your process runs, \
with no time limit. Prefer blocking in the foreground and collecting every result before you \
continue; never end a reply while anything you launched is still running or unread.\n\
- If a result will take too long to block on (a heavy query, an external job), start the work \
detached with its output redirected to a file, then declare the wait and end your reply: \
\"$KANBAN_CMD\" waiting {} --session {session_id} --eta <expected-seconds> --note <what you wait for>\n\
  The board relaunches you after the deadline (eta plus a safety buffer) to check the result; \
declare waiting again from the new session if it needs more time.\n\
- The final command of your reply must be the done command above, the ask command if you are \
blocked, or the waiting command if you are waiting on a declared long-running result. Ending a \
reply without one of those strands the task and forces an automatic resume.\n",
        task.id,
        task.title,
        project_path.display(),
        task.id,
        task.id,
        task.id,
        task.id,
        task.id,
        task.id,
        task.id
    );
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
    append_thread_context(project_path, task, &mut prompt)?;
    Ok(prompt)
}

fn append_thread_context(project_path: &Path, task: &Task, prompt: &mut String) -> Result<()> {
    let thread = ThreadManager::new(project_path)?.load(&task.id)?;
    let messages: Vec<_> = thread
        .messages
        .into_iter()
        .filter(|message| {
            !matches!(message.kind, MessageKind::System | MessageKind::Task)
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

fn build_revert_prompt(project_path: &Path, task: &Task, session_id: &str) -> String {
    format!(
        "Task: {}: revert {}\n\n\
You are a delegated kanban4ai revert agent working in project: {}\n\
Restore every file from .kanban/backups/{}/ to its original repo-relative path.\n\
Do not make unrelated edits. KANBAN_CMD is set to the current kanban4ai executable; \
use \"$KANBAN_CMD\" instead of bare kanban. After restoring, verify the files exist, \
record context with \"$KANBAN_CMD\" context {} <text> --source agent, then run: \
\"$KANBAN_CMD\" done {} --session {session_id} --agent\n\
KANBAN_SESSION={session_id}\nKANBAN_TASK_ID={}\nKANBAN_CMD=$KANBAN_CMD\n",
        task.id,
        task.title,
        project_path.display(),
        task.id,
        task.id,
        task.id,
        task.id
    )
}
