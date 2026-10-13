//! The auto-mode arbiter: a one-shot agent run that answers a task's open
//! questions in place of the absent human (see [`crate::core::auto_mode`]).
//!
//! [`spawn_arbiter`] starts `kanban arbiter <TASK-ID>` detached (own session,
//! so it outlives the TUI or daemon tick that started it); that process runs
//! [`run_arbiter`], which launches the configured arbiter backend with the
//! task context and lets it answer through `kanban answer`. Whatever is still
//! open when the backend exits — or is killed at `arbiter_timeout` — gets the
//! decide answer, so a question never outlives an arbiter run.

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::agent::backends::{backend_args, backend_config, resolve_bot_launch_settings};
use crate::agent::tmux::resolve_callback_binary;
use crate::core::auto_mode::AUTO_DECIDE_ANSWER;
use crate::core::error::{KanbanError, Result};
use crate::core::models::{Message, Task};
use crate::core::operations::Operations;
use crate::core::project::Roots;
use crate::core::storage::atomic_write_text;
use crate::core::timefmt;

fn kanban_cmd() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(resolve_callback_binary)
}

/// Start the detached arbiter process for one task.
pub(crate) fn spawn_arbiter(roots: Roots<'_>, task_id: &str) -> Result<bool> {
    let Some(exe) = kanban_cmd() else {
        return Ok(false);
    };
    let mut cmd = Command::new(exe);
    cmd.args(["arbiter", task_id])
        .current_dir(roots.work_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(id) = roots.project_id {
        cmd.env("KANBAN_PROJECT", id);
    }
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(true)
}

/// The arbiter's whole brief: the task, its gathered context, and the open
/// questions with the exact command that answers each one.
pub fn arbiter_prompt(task: &Task, context: &str, questions: &[Message]) -> String {
    let mut prompt = format!(
        "You are the arbiter of kanban4ai auto mode. The human who owns this board is not \
available; you answer the open questions of a delegated agent in their place.\n\n\
Task: {}: {}\n\nTask description:\n{}\n\nTask context:\n{}\n\nOpen questions:\n",
        task.id,
        task.title,
        task.description.trim(),
        context.trim()
    );
    for question in questions {
        prompt.push_str(&format!("- {}: {}\n", question.id, question.body.trim()));
        if !question.variants.is_empty() {
            prompt.push_str(&format!("  Options: {}\n", question.variants.join(" | ")));
        }
    }
    prompt.push_str(&format!(
        "\nRules:\n\
- Decide each question the way the task author would most likely want, from the task text, \
the context above, and the project conventions (AGENTS.md / CLAUDE.md). You may read files in \
the current directory to inform the decision.\n\
- Do not create, edit, or delete any file, and do not run any kanban command other than answer.\n\
- Prefer the choice that keeps the work moving safely without a human. When options are \
listed, answer with one of them verbatim followed by a one-sentence reason.\n\
- Answer every question with exactly one command each:\n  \
\"$KANBAN_CMD\" answer {} <MSG-ID> \"<answer>\"\n\
- Then stop.\n",
        task.id
    ));
    prompt
}

/// Run the arbiter for one task in the foreground (the body of the hidden
/// `kanban arbiter` command). Returns how many questions fell back to the
/// decide answer.
pub fn run_arbiter(ops: &Operations, task_id: &str) -> Result<usize> {
    let result = arbiter_pass(ops, task_id);
    let fallback = match ops.auto_answerable_questions(task_id) {
        Ok(left) if !left.is_empty() => {
            ops.post_queue_note(
                task_id,
                &format!(
                    "⚠ auto mode: the arbiter left {} question(s) open — deciding without it",
                    left.len()
                ),
            );
            ops.auto_answer_open_questions(task_id, AUTO_DECIDE_ANSWER)?
        }
        _ => 0,
    };
    let _ = fs::remove_file(ops.arbiter_marker(task_id));
    ops.auto_resume_after_answers(task_id)?;
    result.map(|()| fallback)
}

fn arbiter_pass(ops: &Operations, task_id: &str) -> Result<()> {
    let mode = ops.auto_mode();
    let questions = ops.auto_answerable_questions(task_id)?;
    if !mode.enabled || questions.is_empty() {
        return Ok(());
    }
    let task = ops
        .get_task(task_id)?
        .ok_or_else(|| KanbanError::Invalid(format!("no such task: {task_id}")))?;
    let config = ops.config.load()?;
    let settings = resolve_bot_launch_settings(&config, &mode.arbiter_bot())?;
    let backend = backend_config(&config, &settings.backend)?;
    let prompt = arbiter_prompt(&task, &ops.task_context(task_id)?, &questions);

    let logs = ops.storage.kanban_dir.join("logs");
    fs::create_dir_all(&logs)?;
    let stem = format!(
        "arbiter-{task_id}-{}",
        timefmt::now().format("%Y%m%d-%H%M%S")
    );
    let prompt_file = logs.join(format!("{stem}.prompt.txt"));
    atomic_write_text(&prompt_file, &prompt)?;
    let mut args = backend_args(
        &settings.backend,
        &backend,
        settings.model.as_deref(),
        settings.effort.as_deref(),
        settings.agent.as_deref(),
        None,
        &prompt,
    );
    if settings.backend == "grok" {
        args.push("--prompt-file".to_string());
        args.push(prompt_file.display().to_string());
    } else {
        args.push(prompt);
    }
    let log = fs::File::create(logs.join(format!("{stem}.log")))?;
    ops.post_queue_note(
        task_id,
        &format!(
            "⚖ auto mode: arbiter {}{} answering {} question(s)",
            settings.backend,
            settings
                .model
                .as_deref()
                .map(|m| format!("/{m}"))
                .unwrap_or_default(),
            questions.len()
        ),
    );

    let roots = ops.roots();
    let mut cmd = Command::new(&backend.command);
    cmd.args(&args)
        .current_dir(ops.task_cwd(task_id))
        .env("KANBAN_TASK_ID", task_id)
        .env("KANBAN_DATA_DIR", roots.kanban_dir())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if let Some(exe) = kanban_cmd() {
        cmd.env("KANBAN_CMD", exe);
    }
    if let Some(id) = roots.project_id {
        cmd.env("KANBAN_PROJECT", id);
    }
    let mut child = cmd.spawn()?;
    wait_with_timeout(&mut child, Duration::from_secs(mode.arbiter_timeout_secs))
}

/// Wait for the arbiter backend, killing it at the deadline. A sweep only
/// treats the run as lost a grace period past this same deadline.
fn wait_with_timeout(child: &mut std::process::Child, limit: Duration) -> Result<()> {
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if started.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(KanbanError::Invalid(format!(
                "arbiter timed out after {}s",
                limit.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
