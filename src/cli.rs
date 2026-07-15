//! The `kanban` command-line interface (clap). Output text mirrors the Python
//! CLI so existing agent prompts and scripts keep working unchanged.

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command as ProcessCommand, ExitCode, Stdio};

use clap::{Parser, Subcommand};

use crate::agent::{attach_to_session, resolve_opencode_agent};
use crate::core::compaction::{CompactionManager, CompactionStatus};
use crate::core::config::Config;
use crate::core::context::ContextManager;
use crate::core::error::{KanbanError, Result};
use crate::core::models::Task;
use crate::core::operations::{AgentExitOutcome, Operations, QuestionRef, TaskPatch};
use crate::core::session::{SessionManager, estimate_session_tokens};
use crate::core::storage::NewTask;
use crate::core::timefmt;

#[derive(Parser)]
#[command(
    name = "kanban",
    version,
    about = "Kanban board for local task management within projects."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new task.
    Create {
        title: String,
        /// Task description
        #[arg(long, default_value = "")]
        description: String,
        /// Model to use when this task is delegated (interpreted by the chosen agent backend)
        #[arg(long = "model")]
        ai_model: Option<String>,
        /// Reasoning effort (claude: low/medium/high/xhigh/max; opencode: a model variant)
        #[arg(long = "effort")]
        ai_effort: Option<String>,
        /// Agent backend to run this task (e.g. opencode, claude)
        #[arg(long = "backend")]
        agent_backend: Option<String>,
        /// opencode agent persona for this task (e.g. sisyphus); opencode backend only
        #[arg(long = "agent-name")]
        agent_name: Option<String>,
        /// Enable interactive ask/wait guidance for delegated agents
        #[arg(long)]
        interactive: bool,
        /// Auto-run this task when target task (e.g. TASK-029) reaches Review
        #[arg(long = "chain-to")]
        chained_to: Option<String>,
    },
    /// Initialize kanban board in current or specified directory.
    Init {
        /// Project path to initialize
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// List all tasks.
    List {
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
        /// Search in title/description
        #[arg(long)]
        search: Option<String>,
        /// Sort field
        #[arg(long, default_value = "created")]
        sort: String,
        /// Sort order
        #[arg(long, default_value = "desc")]
        order: String,
        /// Output format
        #[arg(long = "format", value_parser = ["table", "json"], default_value = "table")]
        output_format: String,
    },
    /// Take a task for work (links to an agent session).
    Take {
        task_id: String,
        /// Agent session ID
        #[arg(long)]
        session: Option<String>,
        /// Agent mode (enforces rules)
        #[arg(long)]
        agent: bool,
    },
    /// Mark a task as done (only from Review, user only).
    Done {
        task_id: String,
        /// Agent session ID
        #[arg(long)]
        session: Option<String>,
        /// Agent mode
        #[arg(long)]
        agent: bool,
    },
    /// Append context to a task.
    Context {
        task_id: String,
        text: String,
        /// Append context from file
        #[arg(long)]
        file: Option<String>,
        /// Context source
        #[arg(long, default_value = "agent")]
        source: String,
    },
    /// Add a question to a task (triggers questions pipeline).
    Ask {
        task_id: String,
        question: String,
        /// Agent is asking
        #[arg(long)]
        agent: bool,
        /// Block until the question is answered or times out
        #[arg(long = "wait")]
        wait_for_answer: bool,
        /// Suggested answer variant (repeatable)
        #[arg(long = "variants")]
        variants: Vec<String>,
        /// Override wait timeout in seconds
        #[arg(long)]
        timeout: Option<i64>,
        /// Session ID for heartbeat while waiting
        #[arg(long)]
        session: Option<String>,
    },
    /// Answer a question on a task (QUESTION_REF is a MSG-id like MSG-002, or a numeric index).
    Answer {
        task_id: String,
        question_ref: String,
        answer: String,
    },
    /// List open question/suggestion messages for a task.
    Questions { task_id: String },
    /// Add a suggestion to a task.
    Suggest { task_id: String, suggestion: String },
    /// Set the review-edits buffer on a task (folded into the thread on next re-run).
    Edits { task_id: String, text: String },
    /// Fold pending review edits into the thread and re-run the task's agent.
    Rerun {
        task_id: String,
        /// Session ID for the relaunched agent
        #[arg(long)]
        session: Option<String>,
    },
    /// Launch a revert agent using files saved under .kanban/backups/<task>.
    Revert {
        task_id: String,
        /// Session ID for the revert agent
        #[arg(long)]
        session: Option<String>,
    },
    /// Move a task to another column.
    Move {
        task_id: String,
        target_status: String,
        /// Agent mode
        #[arg(long)]
        agent: bool,
    },
    /// Chain TASK_ID to run when TARGET_ID reaches Review (or --clear to unset).
    Chain {
        task_id: String,
        target_id: Option<String>,
        /// Remove the chain from this task
        #[arg(long)]
        clear: bool,
    },
    /// Move all Done tasks to Archive.
    #[command(name = "archive-done")]
    ArchiveDone,
    /// List archived tasks.
    Archive {
        /// Search archived task title/description
        #[arg(long)]
        search: Option<String>,
        /// Output format
        #[arg(long = "format", value_parser = ["table", "json"], default_value = "table")]
        output_format: String,
    },
    /// Show task details.
    Show {
        task_id: String,
        /// Include full context
        #[arg(long = "with-context")]
        with_context: bool,
    },
    /// Compact context for a task.
    Compact {
        task_id: String,
        /// Force compaction below threshold
        #[arg(long)]
        force: bool,
    },
    /// Update session heartbeat.
    Heartbeat {
        /// Session ID
        #[arg(long)]
        session: Option<String>,
    },
    /// Declare that the agent is waiting for a long-running result. The
    /// session stays alive until eta × waiting_eta_multiplier seconds from
    /// now; if the agent process exits meanwhile, it is relaunched at that
    /// deadline to check the result. Call again to extend the wait.
    Waiting {
        task_id: String,
        /// Expected wait in seconds (default: waiting_default_eta threshold)
        #[arg(long)]
        eta: Option<i64>,
        /// What is being waited for
        #[arg(long)]
        note: Option<String>,
        /// Session ID
        #[arg(long)]
        session: Option<String>,
    },
    /// Run a command detached from the agent session so it survives the
    /// session's exit, then declare a wait for its result. Output is appended
    /// to .kanban/detached/<task>-<stamp>.log, the exit code is written to
    /// the matching .status file, and the agent is relaunched after the wait
    /// deadline to check them.
    Detach {
        task_id: String,
        /// Expected wait in seconds (default: waiting_default_eta threshold)
        #[arg(long)]
        eta: Option<i64>,
        /// What is being waited for
        #[arg(long)]
        note: Option<String>,
        /// Session ID
        #[arg(long)]
        session: Option<String>,
        /// Command to run detached (specify after --)
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Check for crashed sessions.
    #[command(name = "check-sessions")]
    CheckSessions,
    /// Recover a crashed task.
    Recover { task_id: String },
    /// List active sessions.
    Sessions,
    /// Launch the TUI kanban board.
    Tui,
    /// Attach to a running agent session for a task.
    Attach { task_id: String },
    /// Internal command used by the agent runtime wrapper to reconcile process exit.
    #[command(name = "agent-exit", hide = true)]
    AgentExit {
        task_id: String,
        #[arg(long)]
        session: String,
        #[arg(long)]
        status: i32,
    },
    #[command(name = "wait-resume", hide = true)]
    WaitResume {
        task_id: String,
        #[arg(long)]
        session: String,
    },
    /// Internal command used by the agent runtime wrapper: match a configured
    /// opencode agent name against `<command> agent list` and print the
    /// registered form. Runs inside the spawned session so the slow opencode
    /// CLI startup never blocks the launching process.
    #[command(name = "resolve-agent", hide = true)]
    ResolveAgent {
        requested: String,
        /// Backend CLI to query (the configured opencode command)
        #[arg(long)]
        command: String,
    },
}

fn env_session(explicit: Option<String>) -> String {
    explicit
        .or_else(|| std::env::var("KANBAN_SESSION").ok())
        .unwrap_or_else(|| "default".to_string())
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli.command.unwrap_or(Command::Tui)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(command: Command) -> Result<ExitCode> {
    let ops = Operations::new(".");
    match command {
        Command::Create {
            title,
            description,
            ai_model,
            ai_effort,
            agent_backend,
            agent_name,
            interactive,
            chained_to,
        } => {
            let task = ops.create_task(NewTask {
                title,
                description,
                ai_model,
                ai_effort,
                agent_backend,
                agent_name,
                interactive,
                chained_to: chained_to.filter(|c| !c.is_empty()),
            })?;
            println!("Created task {}: {}", task.id, task.title);
            if let Some(chained_to) = &task.chained_to {
                println!("Chained to {chained_to} (auto-runs when it reaches Review)");
            }
        }
        Command::Init { path } => {
            let ops = Operations::new(&path);
            ops.storage.init_board()?;
            println!("Initialized kanban board at {path}/.kanban");
        }
        Command::List {
            status,
            search,
            sort,
            order,
            output_format,
        } => {
            let tasks = ops.list_tasks(status.as_deref(), search.as_deref(), &sort, &order)?;
            if output_format == "json" {
                println!("{}", tasks_to_json(&tasks)?);
            } else if tasks.is_empty() {
                println!("No tasks found.");
            } else {
                println!("{:<12} {:<15} Title", "ID", "Status");
                println!("{}", "-".repeat(48));
                for task in tasks {
                    println!(
                        "{:<12} {:<15} {}",
                        task.id,
                        task.status.as_str(),
                        task.title
                    );
                }
            }
        }
        Command::Take {
            task_id,
            session,
            agent,
        } => {
            let session_id = env_session(session);
            match ops.take_task(&task_id, &session_id, agent)? {
                Some(task) => {
                    println!("Task {task_id} assigned to session {session_id}");
                    println!("Status: {}", task.status);
                    if !task.description.is_empty() {
                        println!("\nDescription:\n{}", task.description);
                    }
                }
                None => {
                    eprintln!("Failed to take task {task_id}");
                    if agent {
                        eprintln!("Check if you already have an active task.");
                    }
                }
            }
        }
        Command::Done {
            task_id,
            session,
            agent,
        } => {
            let session_id = env_session(session);
            match ops.complete_task(&task_id, &session_id, agent) {
                Ok(Some(task)) => println!("Task {task_id} marked as {}", task.status),
                Ok(None) => eprintln!("Failed to complete task {task_id}"),
                Err(err) => eprintln!("Error: {err}"),
            }
        }
        Command::Context {
            task_id,
            text,
            file,
            source,
        } => {
            let text = match file {
                Some(path) => std::fs::read_to_string(&path)?,
                None => text,
            };
            ContextManager::new(".").append_context(&task_id, &text, &source, &ops.storage)?;
            println!("Context added to {task_id}");
        }
        Command::Ask {
            task_id,
            question,
            agent,
            wait_for_answer,
            variants,
            timeout,
            session,
        } => {
            if wait_for_answer {
                let session_id = env_session(session);
                let message = ops.ask_and_wait(
                    &task_id,
                    &question,
                    Some(&session_id),
                    variants,
                    timeout,
                    None,
                )?;
                match message.and_then(|m| m.answer) {
                    Some(answer) => println!("{answer}"),
                    None => eprintln!("Failed to add question to {task_id}"),
                }
                return Ok(ExitCode::SUCCESS);
            }

            let source = if agent { "agent" } else { "user" };
            match ops.ask_question(&task_id, &question, source, variants)? {
                Some(task) => {
                    println!("Question added to {task_id}");
                    if task.has_questions {
                        println!("Task has pending questions.");
                    }
                }
                None => eprintln!("Failed to add question to {task_id}"),
            }
        }
        Command::Answer {
            task_id,
            question_ref,
            answer,
        } => {
            let question_ref =
                if question_ref.chars().all(|c| c.is_ascii_digit()) && !question_ref.is_empty() {
                    QuestionRef::Index(question_ref.parse().unwrap_or(usize::MAX))
                } else {
                    QuestionRef::MsgId(question_ref)
                };
            match ops.answer_question(&task_id, question_ref, &answer)? {
                Some(_) => println!("Answer added to {task_id}"),
                None => eprintln!("Failed to answer question on {task_id}"),
            }
        }
        Command::Questions { task_id } => {
            let messages = ops.list_open_messages(&task_id)?;
            if messages.is_empty() {
                println!("No open messages.");
            } else {
                for message in messages {
                    let variants = if message.variants.is_empty() {
                        String::new()
                    } else {
                        format!(" | variants: {}", message.variants.join(", "))
                    };
                    println!(
                        "{} [{}] {}{}",
                        message.id, message.kind, message.body, variants
                    );
                }
            }
        }
        Command::Suggest {
            task_id,
            suggestion,
        } => match ops.suggest_improvement(&task_id, &suggestion, "agent", vec![])? {
            Some(_) => println!("Suggestion added to {task_id}"),
            None => eprintln!("Failed to add suggestion to {task_id}"),
        },
        Command::Edits { task_id, text } => match ops.set_review_edits(&task_id, &text)? {
            Some(_) => println!("Review edits saved on {task_id}"),
            None => eprintln!("Task {task_id} not found"),
        },
        Command::Rerun { task_id, session } => {
            match ops.rerun_review_task(&task_id, session.as_deref())? {
                Some(task) => println!(
                    "Task {task_id} re-running ({})",
                    task.session.as_deref().unwrap_or("none")
                ),
                None => eprintln!("Task {task_id} not found"),
            }
        }
        Command::Revert { task_id, session } => {
            let session_id = session.unwrap_or_else(|| {
                format!("ses-revert-{}", timefmt::now().format("%Y%m%d-%H%M%S"))
            });
            if ops.launch_revert(&task_id, &session_id)? {
                println!("Task {task_id} revert launched ({session_id})");
            } else {
                eprintln!("Failed to launch revert for {task_id}");
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Move {
            task_id,
            target_status,
            agent,
        } => match ops.move_task(&task_id, &target_status, agent) {
            Ok(Some(_)) => println!("Task {task_id} moved to {target_status}"),
            Ok(None) => eprintln!("Task {task_id} not found"),
            Err(KanbanError::Permission(msg)) => eprintln!("Permission denied: {msg}"),
            Err(err) => eprintln!("Error: {err}"),
        },
        Command::Chain {
            task_id,
            target_id,
            clear,
        } => return chain_command(&ops, &task_id, target_id.as_deref(), clear),
        Command::ArchiveDone => {
            let archived = ops.archive_done_tasks()?;
            println!("Archived {} done task(s).", archived.len());
        }
        Command::Archive {
            search,
            output_format,
        } => {
            let tasks = ops.list_archived_tasks(search.as_deref())?;
            if output_format == "json" {
                println!("{}", tasks_to_json(&tasks)?);
            } else if tasks.is_empty() {
                println!("No archived tasks found.");
            } else {
                println!("{:<12} {:<19} Title", "ID", "Updated");
                println!("{}", "-".repeat(60));
                for task in tasks {
                    let updated = task.updated_at.format("%Y-%m-%d %H:%M");
                    println!("{:<12} {:<19} {}", task.id, updated.to_string(), task.title);
                }
            }
        }
        Command::Show {
            task_id,
            with_context,
        } => {
            let Some(task) = ops.get_task(&task_id)? else {
                eprintln!("Task {task_id} not found");
                return Ok(ExitCode::SUCCESS);
            };
            println!("ID: {}", task.id);
            println!("Title: {}", task.title);
            println!("Status: {}", task.status);
            if let Some(agent_backend) = &task.agent_backend {
                println!("Agent: {agent_backend}");
            }
            if let Some(agent_name) = &task.agent_name {
                println!("Agent persona: {agent_name}");
            }
            if let Some(ai_model) = &task.ai_model {
                println!("AI Model: {ai_model}");
            }
            if let Some(ai_effort) = &task.ai_effort {
                println!("Effort: {ai_effort}");
            }
            if let Some(chained_to) = &task.chained_to {
                println!("Chained to: {chained_to}");
            }
            if let Some(session) = &task.session {
                println!("Session: {session}");
            }
            println!("Created: {}", timefmt::format(&task.created_at));
            println!("Updated: {}", timefmt::format(&task.updated_at));
            if !task.description.is_empty() {
                println!("\nDescription:\n{}", task.description);
            }
            if with_context {
                let context = ContextManager::new(".").get_context(&task_id, &ops.storage)?;
                if !context.is_empty() {
                    println!("\nContext:\n{context}");
                }
            }
        }
        Command::Compact { task_id, force } => {
            let config = Config::new(".");
            let threshold = config.get_threshold("context_auto_compact")? as usize;
            let result = CompactionManager::new(".").compact_context(&task_id, threshold, force)?;
            match result.status {
                CompactionStatus::Compacted => {
                    println!(
                        "Context compacted: {} -> {} bytes",
                        result.before, result.after
                    );
                    println!("Reduction: {} bytes", result.reduction());
                }
                CompactionStatus::BelowThreshold => println!(
                    "Context size ({} bytes) below threshold. Use --force to compact anyway.",
                    result.before
                ),
                CompactionStatus::NoContext => println!("No context found for this task."),
            }
        }
        Command::Heartbeat { session } => {
            let session_id = env_session(session);
            SessionManager::new(".").heartbeat(&session_id)?;
            println!("Heartbeat updated for session {session_id}");
        }
        Command::Waiting {
            task_id,
            eta,
            note,
            session,
        } => {
            let session_id = env_session(session);
            let deadline = ops.declare_waiting(&task_id, &session_id, eta, note.as_deref())?;
            println!(
                "Wait recorded for {task_id} (session {session_id}). \
                 Relaunch deadline: {deadline}. End your reply now; you will be relaunched \
                 after the deadline to check the result."
            );
        }
        Command::Detach {
            task_id,
            eta,
            note,
            session,
            command,
        } => {
            let session_id = env_session(session);
            let job = ops.detach_command(&task_id, &session_id, eta, note.as_deref(), &command)?;
            println!(
                "Detached pid {} for {task_id} (session {session_id}).\n\
                 Output: {}\n\
                 Exit code file: {}\n\
                 Relaunch deadline: {}. End your reply now; you will be relaunched \
                 after the deadline to check the result.",
                job.pid,
                job.log_file.display(),
                job.status_file.display(),
                job.deadline
            );
        }
        Command::CheckSessions => {
            let resumed = ops.resume_expired_waits()?;
            for (task_id, session_id) in &resumed {
                println!("Resumed {task_id} after wait deadline → {session_id}");
            }
            let timeout = Config::new(".").get_threshold("session_heartbeat_timeout")?;
            let crashed = SessionManager::new(".").check_sessions(timeout)?;
            if crashed.is_empty() {
                println!("No crashed sessions found.");
            } else {
                println!("Found {} crashed sessions:", crashed.len());
                for session in crashed {
                    println!("  {} (task: {})", session.id, session.task_id);
                }
            }
        }
        Command::Recover { task_id } => match ops.recover_task(&task_id)? {
            Some(_) => {
                println!("Task {task_id} recovered and moved to To Do");
            }
            None => eprintln!("Task {task_id} not found"),
        },
        Command::Sessions => {
            let session_mgr = SessionManager::new(".");
            let active = session_mgr.list_active_sessions();
            if active.is_empty() {
                println!("No active sessions.");
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "{:<28} {:<36} {:<12} {:<16} Last Seen",
                "Session ID", "Task", "Tokens", "Started"
            );
            println!("{}", "-".repeat(108));
            for session in active {
                let started = session.started_at.format("%Y-%m-%d %H:%M").to_string();
                let last_seen = session.last_seen.format("%Y-%m-%d %H:%M").to_string();
                let task_label = match ops.get_task(&session.task_id)? {
                    Some(task) => truncate_chars(&format!("{} {}", task.id, task.title), 36),
                    None => match session.name.as_deref() {
                        Some(name) => truncate_chars(&format!("{} {}", session.task_id, name), 36),
                        None => session.task_id.clone(),
                    },
                };
                let tokens = estimate_session_tokens(Path::new("."), &session.id);
                let tokens_label = tokens.map_or("unknown".to_string(), format_thousands);
                println!(
                    "▶ {:<26} {:<36} {:<12} {:<16} {}",
                    session.id, task_label, tokens_label, started, last_seen
                );
            }
        }
        Command::Tui => {
            crate::tui::run(".")?;
        }
        Command::Attach { task_id } => {
            let Some(task) = ops.storage.load_task(&task_id)? else {
                eprintln!("Task {task_id} not found");
                return Ok(ExitCode::FAILURE);
            };
            let Some(session_id) = task.session.as_deref() else {
                eprintln!("Task {task_id} has no active session");
                return Ok(ExitCode::FAILURE);
            };
            return Ok(if attach_to_session(session_id)? {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        Command::AgentExit {
            task_id,
            session,
            status,
        } => match ops.reconcile_agent_exit(&task_id, &session, status)? {
            AgentExitOutcome::Closed => println!("Session {session} for {task_id} closed"),
            AgentExitOutcome::Waiting => {
                spawn_wait_resume_monitor(&task_id, &session)?;
                println!(
                    "Session {session} for {task_id} is in a declared wait; \
                     the agent will be relaunched at the wait deadline"
                )
            }
            AgentExitOutcome::Resumed(new_session) => println!(
                "Session {session} for {task_id} ended without done/ask/waiting; \
                 auto-resumed as {new_session}"
            ),
            AgentExitOutcome::ResumeExhausted => {
                eprintln!(
                    "Session {session} for {task_id} ended without done/ask/waiting and the \
                     auto-resume budget is spent; session marked crashed"
                );
                return Ok(ExitCode::FAILURE);
            }
            AgentExitOutcome::LaunchFailed(new_session) => {
                eprintln!(
                    "Session {session} for {task_id} ended without done/ask/waiting, but auto-resume launch failed; {new_session} marked crashed"
                );
                return Ok(ExitCode::FAILURE);
            }
            AgentExitOutcome::Crashed => {
                eprintln!("Session {session} for {task_id} crashed with status {status}");
                return Ok(ExitCode::FAILURE);
            }
        },
        Command::WaitResume { task_id, session } => {
            wait_resume_monitor(&ops, &task_id, &session)?;
        }
        Command::ResolveAgent { requested, command } => {
            println!("{}", resolve_opencode_agent(&command, &requested));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn wait_resume_monitor(ops: &Operations, task_id: &str, session_id: &str) -> Result<()> {
    let session_mgr = SessionManager::new(".");
    loop {
        let Some(session) = session_mgr.load_session(session_id) else {
            return Ok(());
        };
        if session.task_id != task_id
            || session.status != crate::core::models::SessionStatus::Active
        {
            return Ok(());
        }
        let Some(deadline) = session.wait_until else {
            return Ok(());
        };
        let now = timefmt::now();
        if deadline > now
            && let Ok(wait) = (deadline - now).to_std()
        {
            std::thread::sleep(wait.min(std::time::Duration::from_secs(60)));
            continue;
        }
        let resumed = ops.resume_expired_waits()?;
        if !resumed.is_empty() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

fn spawn_wait_resume_monitor(task_id: &str, session_id: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = ProcessCommand::new(exe);
    command
        .arg("wait-resume")
        .arg(task_id)
        .arg("--session")
        .arg(session_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()?;
    Ok(())
}

fn chain_command(
    ops: &Operations,
    task_id: &str,
    target_id: Option<&str>,
    clear: bool,
) -> Result<ExitCode> {
    if !clear && target_id.is_none() {
        let Some(task) = ops.get_task(task_id)? else {
            eprintln!("Task {task_id} not found");
            return Ok(ExitCode::SUCCESS);
        };
        match &task.chained_to {
            Some(chained_to) => println!("{task_id} is chained to {chained_to}"),
            None => println!("{task_id} is not chained to any task"),
        }
        return Ok(ExitCode::SUCCESS);
    }

    let new_value = if clear {
        None
    } else {
        let target_id = target_id.expect("checked above");
        if ops.get_task(target_id)?.is_none() {
            eprintln!("Target task {target_id} not found");
            return Ok(ExitCode::SUCCESS);
        }
        if target_id == task_id {
            eprintln!("A task cannot be chained to itself");
            return Ok(ExitCode::SUCCESS);
        }
        Some(target_id.to_string())
    };

    let updated = ops.update_task(
        task_id,
        TaskPatch {
            chained_to: Some(new_value.clone()),
            ..Default::default()
        },
    )?;
    if updated.is_none() {
        eprintln!("Task {task_id} not found");
        return Ok(ExitCode::SUCCESS);
    }
    match new_value {
        Some(target) => {
            println!("{task_id} chained to {target} (auto-runs when it reaches Review)")
        }
        None => println!("Chain removed from {task_id}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn tasks_to_json(tasks: &[Task]) -> Result<String> {
    let values: Vec<serde_json::Value> = tasks.iter().map(task_to_json).collect();
    serde_json::to_string_pretty(&values)
        .map_err(|e| crate::core::error::KanbanError::Invalid(e.to_string()))
}

/// Full task dict (including `description`) in the Python `to_dict` key order.
fn task_to_json(task: &Task) -> serde_json::Value {
    serde_json::json!({
        "id": task.id,
        "title": task.title,
        "description": task.description,
        "status": task.status.as_str(),
        "session": task.session,
        "created_at": timefmt::format(&task.created_at),
        "updated_at": timefmt::format(&task.updated_at),
        "has_questions": task.has_questions,
        "context_file": task.context_file,
        "context_size": task.context_size,
        "ai_model": task.ai_model,
        "ai_effort": task.ai_effort,
        "agent_backend": task.agent_backend,
        "agent_name": task.agent_name,
        "interactive": task.interactive,
        "chained_to": task.chained_to,
        "review_edits": task.review_edits,
    })
}

fn format_thousands(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
