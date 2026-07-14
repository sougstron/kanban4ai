use std::path::Path;

use crate::core::models::Task;

pub fn build_agent_prompt(
    project_path: &Path,
    task: &Task,
    session_id: &str,
    revert: bool,
) -> String {
    if revert {
        return build_revert_prompt(project_path, task, session_id);
    }

    let mut prompt = format!(
        "Task: {}: {}\n\n\
You are a delegated kanban4ai agent working in project: {}\n\
Work only on task {}. The user-authored task is below.\n\n\
Session contract:\n\
- KANBAN_SESSION is set to {session_id}.\n\
- KANBAN_TASK_ID is set to {}.\n\
- Before editing an existing file, copy it to .kanban/backups/{}/ preserving the repo-relative path.\n\
- Record important progress with: kanban context {} <text> --source agent\n\
- Keep the session alive with: kanban heartbeat --session {session_id}\n\
- When implementation and verification are complete, run: kanban done {} --session {session_id} --agent\n\
- If blocked, ask with: kanban ask {} <question> --agent\n",
        task.id,
        task.title,
        project_path.display(),
        task.id,
        task.id,
        task.id,
        task.id,
        task.id,
        task.id
    );
    if task.interactive {
        prompt.push_str(&format!(
            "- This task is interactive: for blocking questions use kanban ask {} <question> --agent --wait --session {}; for non-blocking ideas use kanban suggest.\n",
            task.id, session_id
        ));
    }
    prompt.push_str("\nUser task:\n");
    if task.description.trim().is_empty() {
        prompt.push_str(&task.title);
    } else {
        prompt.push_str(task.description.trim());
    }
    prompt
}

fn build_revert_prompt(project_path: &Path, task: &Task, session_id: &str) -> String {
    format!(
        "Task: {}: revert {}\n\n\
You are a delegated kanban4ai revert agent working in project: {}\n\
Restore every file from .kanban/backups/{}/ to its original repo-relative path.\n\
Do not make unrelated edits. After restoring, verify the files exist, record context with \
kanban context {} <text> --source agent, then run: kanban done {} --session {session_id} --agent\n\
KANBAN_SESSION={session_id}\nKANBAN_TASK_ID={}\n",
        task.id,
        task.title,
        project_path.display(),
        task.id,
        task.id,
        task.id,
        task.id
    )
}
