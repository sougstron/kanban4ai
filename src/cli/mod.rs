//! The `kanban` command-line interface (clap). Output text mirrors the Python
//! CLI so existing agent prompts and scripts keep working unchanged.

mod bridge;
mod daemon;
mod init;
mod project;
mod resolve;

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command as ProcessCommand, ExitCode, Stdio};

use clap::{Parser, Subcommand};

use project::ProjectCommand;

use crate::agent::{attach_to_session, resolve_opencode_agent};
use crate::core::ask_form::AskForm;
use crate::core::compaction::{CompactionManager, CompactionStatus};
use crate::core::config::Config;
use crate::core::context::ContextManager;
use crate::core::error::{KanbanError, Result};
use crate::core::limits;
use crate::core::models::{RunMode, Task, TaskStatus};
use crate::core::operations::{
    AgentExitOutcome, LandOutcome, Operations, QuestionRef, TaskPatch, Verdict, WaitWake,
};
use crate::core::session::{SessionManager, estimate_session_tokens};
use crate::core::storage::NewTask;
use crate::core::timefmt;
use crate::core::update;

#[derive(Parser)]
#[command(
    name = "kanban",
    version,
    about = "Kanban board for local task management within projects."
)]
pub struct Cli {
    /// Project id, name, or work path (overrides cwd / $KANBAN_PROJECT)
    #[arg(long, global = true)]
    project: Option<String>,
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
        /// Run the project designer bot for this task even if it is off board-wide
        #[arg(long)]
        designer: bool,
        /// Run the project reviewer bot for this task even if it is off board-wide
        #[arg(long)]
        reviewer: bool,
        /// Auto-run this task when target task (e.g. TASK-029) reaches Review
        #[arg(long = "chain-to")]
        chained_to: Option<String>,
    },
    /// Register a project in the store (migrating a local .kanban if present).
    Init {
        /// Folder to register
        #[arg(long, default_value = ".")]
        path: String,
        /// Copy the local .kanban into the store instead of moving it
        #[arg(long)]
        copy: bool,
        /// Move even when the board has active agent sessions
        #[arg(long)]
        force: bool,
    },
    /// Manage registered projects.
    #[command(subcommand)]
    Project(ProjectCommand),
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
    /// Post one or more questions from a strict YAML form file (see AGENTS.md
    /// for the schema). Each entry becomes a question whose `options` are the
    /// selectable answer variants.
    AskForm {
        task_id: String,
        /// Path to the YAML form file
        #[arg(long)]
        file: String,
        /// Agent is asking
        #[arg(long)]
        agent: bool,
        /// Session ID for agent ownership check
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
    /// Reject (quarantine) a thread message so it is excluded from future
    /// prompts and gathered context, while staying visible for audit.
    Reject { task_id: String, msg_id: String },
    /// Restore a previously rejected thread message.
    Unreject { task_id: String, msg_id: String },
    /// Set the review-edits buffer on a task (folded into the thread on next re-run).
    Edits { task_id: String, text: String },
    /// Bot-reviewer verdict: approve (human Review) or request changes.
    Verdict {
        task_id: String,
        /// Accept the work and move the task to human Review.
        #[arg(long, group = "verdict")]
        approve: bool,
        /// Request changes (written into the review-edits buffer).
        #[arg(long, group = "verdict")]
        changes: Option<String>,
        /// Read the requested changes from a file instead of (or in addition to) --changes.
        #[arg(long)]
        file: Option<String>,
        /// Session ID of the reviewer bot.
        #[arg(long)]
        session: Option<String>,
        /// Agent mode (required).
        #[arg(long)]
        agent: bool,
    },
    /// Fold pending review edits into the thread and re-run the task's agent.
    Rerun {
        task_id: String,
        /// Session ID for the relaunched agent
        #[arg(long)]
        session: Option<String>,
        /// Bypass the queue and launch immediately (debug).
        #[arg(long)]
        now: bool,
    },
    /// Launch a revert agent using files saved under the board's backups/<task>.
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
    /// to the board's detached/<task>-<stamp>.log, the exit code is written to
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
    /// Pump the queue and crash-restart schedule without a TUI.
    Daemon {
        /// Seconds between ticks (default: store `daemon.interval`, or 60)
        #[arg(long)]
        interval: Option<u64>,
        /// Run a single tick and exit (cron / systemd timer)
        #[arg(long)]
        once: bool,
    },
    /// Recover a crashed task.
    Recover { task_id: String },
    /// Land an isolated task branch into the work folder (re-runs the merge
    /// after a conflict was resolved, or lands a `land: manual` board).
    Integrate { task_id: String },
    /// Stop the running agent session for a task (leaves the task In Progress).
    Stop { task_id: String },

    /// List active sessions.
    Sessions,
    /// Show remaining subscription limits for the agent providers (claude, grok, zai, synthetic, yolo).
    Limits {
        /// Output format
        #[arg(long = "format", value_parser = ["table", "json"], default_value = "table")]
        output_format: String,
        /// Poll the providers even when the cached snapshot is still fresh
        #[arg(long)]
        refresh: bool,
        #[command(subcommand)]
        bridge: Option<bridge::LimitsBridge>,
    },
    /// Check GitHub Releases for a newer kanban4ai; without --check also
    /// install it: download, verify the published SHA-256, and atomically
    /// replace the running binary. A pacman-owned binary is never touched —
    /// its package-manager upgrade command is printed instead.
    Update {
        /// Report only: print what the newest release is, download nothing
        #[arg(long)]
        check: bool,
    },
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
    /// Internal command used by the agent runtime wrapper: read a backend's
    /// stream-json transcript on stdin and print human-readable text on stdout,
    /// keeping the session log readable while the raw JSONL is captured to a
    /// transcript file for input-provenance harvesting.
    #[command(name = "format-stream", hide = true)]
    FormatStream,
    /// Internal command invoked by the claude statusline bridge shim: record
    /// the rate_limits Claude Code pipes to the statusline on stdin.
    #[command(name = "statusline-bridge", hide = true)]
    StatuslineBridge,
}

/// Read a file named by an agent on the command line (`context --file`,
/// `ask-form --file`).
///
/// An agent's working directory is the code folder while the board lives under
/// the data root, so a relative `.kanban/…` path — the shape agents used
/// before the split, and still write out of habit — is retried against the
/// board before giving up.
fn read_agent_file(ops: &Operations, path: &str) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let candidate = Path::new(path);
            if candidate.is_absolute() {
                return Err(err.into());
            }
            // `.kanban/forms/x.yaml` and `forms/x.yaml` both land in the board.
            let relative = candidate.strip_prefix(".kanban").unwrap_or(candidate);
            let in_board = ops.data_root().join(".kanban").join(relative);
            std::fs::read_to_string(&in_board).map_err(|_| err.into())
        }
        Err(err) => Err(err.into()),
    }
}

fn env_session(explicit: Option<String>) -> String {
    explicit
        .or_else(|| std::env::var("KANBAN_SESSION").ok())
        .unwrap_or_else(|| "default".to_string())
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn launch_tui(selector: Option<&str>) -> Result<ExitCode> {
    match resolve::resolve_tui(selector)? {
        Some(resolve::Resolved::Project(project)) => crate::tui::run_project(project)?,
        Some(resolve::Resolved::InPlace(path)) => crate::tui::run_in_place(path)?,
        None => crate::tui::run(crate::tui::TuiStart::Projects { return_to: None })?,
    }
    Ok(ExitCode::SUCCESS)
}

/// `kanban limits`: remaining subscription capacity per provider. Reuses the
/// snapshot the TUI caches unless it is stale (or `--refresh` is given), so
/// repeated calls do not re-poll the providers.
fn print_limits(output_format: &str, refresh: bool) -> Result<()> {
    let snapshot = match limits::cached() {
        Some(snapshot)
            if !refresh
                && snapshot.age(chrono::Utc::now().timestamp())
                    < limits::DEFAULT_REFRESH_INTERVAL =>
        {
            snapshot
        }
        _ => limits::refresh_blocking(refresh),
    };
    if output_format == "json" {
        let json = serde_json::to_string_pretty(snapshot.as_ref())
            .map_err(|err| KanbanError::Invalid(format!("cannot serialize limits: {err}")))?;
        println!("{json}");
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    for provider in limits::PROVIDERS {
        let Some(entry) = snapshot.get(provider) else {
            continue;
        };
        let note = match &entry.state {
            limits::ProviderState::Ready => None,
            limits::ProviderState::NotConfigured => Some("not configured".to_string()),
            limits::ProviderState::SignedOut => Some("signed out".to_string()),
            limits::ProviderState::Unavailable(reason) => Some(reason.clone()),
        };
        if let Some(note) = note {
            println!("{provider:<8} {note}");
            continue;
        }
        let age = entry
            .data_age(now)
            .filter(|age| *age >= 60)
            .map(|age| format!("  ({} old)", limits::format_span(age)))
            .unwrap_or_default();
        // A window that has already rolled over reports a period that is gone;
        // say so rather than print the number it froze at.
        let windows = entry.live_windows(now);
        if windows.is_empty() {
            println!("{provider:<8} stale{age}");
            continue;
        }
        for (index, window) in windows.into_iter().enumerate() {
            let name = if index == 0 { provider } else { "" };
            let reset = window
                .resets_in(now)
                .map(|seconds| format!("resets in {}", limits::format_span(seconds)))
                .unwrap_or_else(|| "reset unknown".to_string());
            println!(
                "{name:<8} {:<4}{:>4.0}% left  {reset}{}",
                window.label,
                window.remaining_percent,
                if index == 0 { age.as_str() } else { "" }
            );
        }
    }
    Ok(())
}

/// `kanban update [--check]`: report (or install) the newest GitHub release.
/// Deliberately project-independent — updating the binary has nothing to do
/// with any board, so it runs from any directory, with no project at all.
fn run_update(check_only: bool) -> Result<ExitCode> {
    // The cache-or-blocking split print_limits uses: a status inside the
    // check interval answers from the cache, anything else pays one
    // blocking check. A failed check is a missing answer, not a crash.
    let now = chrono::Utc::now().timestamp();
    let fresh = match update::cached() {
        Some(status)
            if !update::status_expired(&status, update::configured_interval_hours(), now) =>
        {
            Some(status)
        }
        _ => None,
    };
    let Some(status) = fresh.or_else(|| update::check_latest(false).ok()) else {
        if check_only {
            println!("No cached update status yet, and the check could not run just now.");
            return Ok(ExitCode::SUCCESS);
        }
        eprintln!("Error: the update check failed; kanban4ai was not changed");
        return Ok(ExitCode::FAILURE);
    };
    if check_only {
        print_update_report(&status);
        return Ok(ExitCode::SUCCESS);
    }
    if !update::is_update_available(&status) {
        print_update_report(&status);
        return Ok(ExitCode::SUCCESS);
    }
    match update::apply_update(&status) {
        Ok(applied) => {
            println!(
                "Updated kanban4ai to {} ({}).",
                applied.version,
                applied.binary.display()
            );
            println!(
                "Restart kanban4ai (or open a new terminal) — the running process \
                 still executes the old version."
            );
            Ok(ExitCode::SUCCESS)
        }
        // The package manager owns the binary, so its upgrade command *is*
        // the update: pointing at it did what was asked (SUCCESS, not FAILURE).
        Err(err @ update::ApplyError::PackageManaged { .. }) => {
            println!("{err}");
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("Error: {err}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn print_update_report(status: &update::UpdateStatus) {
    let installed = update::installed_version();
    if update::is_update_available(status) {
        println!(
            "Update available: kanban4ai {installed} → {}",
            status.latest_version
        );
        if status.asset_url.is_some() {
            println!("Run `kanban update` to install it.");
        }
    } else {
        println!(
            "kanban4ai {installed} is up to date (latest release {}).",
            status.latest_version
        );
    }
    if !status.notes_url.is_empty() {
        println!("Release notes: {}", status.notes_url);
    }
}

fn dispatch(cli: Cli) -> Result<ExitCode> {
    let command = cli.command.unwrap_or(Command::Tui);
    match command {
        Command::Init { path, copy, force } => return init::init(&path, copy, force),
        Command::Project(command) => return project::run(command),
        Command::ResolveAgent { requested, command } => {
            println!("{}", resolve_opencode_agent(&command, &requested));
            return Ok(ExitCode::SUCCESS);
        }
        Command::FormatStream => {
            format_stream(std::io::stdin().lock(), &mut std::io::stdout().lock())?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::StatuslineBridge => {
            bridge::statusline_bridge(&mut std::io::stdin().lock());
            return Ok(ExitCode::SUCCESS);
        }
        Command::Limits {
            output_format,
            refresh,
            bridge: bridge_command,
        } => {
            match bridge_command {
                Some(bridge::LimitsBridge::Bridge { action }) => match action {
                    bridge::BridgeAction::Install => bridge::bridge_install()?,
                    bridge::BridgeAction::Remove => bridge::bridge_remove()?,
                },
                None => print_limits(&output_format, refresh)?,
            }
            return Ok(ExitCode::SUCCESS);
        }
        Command::Update { check } => return run_update(check),
        Command::Tui => return launch_tui(cli.project.as_deref()),
        Command::Daemon { interval, once } => {
            return daemon::run(once, interval, cli.project.as_deref());
        }
        _ => {}
    }
    let ops = resolve::resolve_project(cli.project.as_deref())?.operations();
    // Settings that loaded but will not do what they look like they do. On
    // stderr so it never contaminates a command's parseable stdout.
    if ops.config.load().is_ok() {
        for warning in ops.config.warnings() {
            eprintln!("Warning: {warning}");
        }
    }
    match command {
        Command::Create {
            title,
            description,
            ai_model,
            ai_effort,
            agent_backend,
            agent_name,
            interactive,
            designer,
            reviewer,
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
                use_designer: designer,
                use_reviewer: reviewer,
                chained_to: chained_to.filter(|c| !c.is_empty()),
            })?;
            println!("Created task {}: {}", task.id, task.title);
            if let Some(chained_to) = &task.chained_to {
                println!("Chained to {chained_to} (auto-runs when it reaches Review)");
            }
        }
        Command::Init { .. } | Command::Project(_) => unreachable!("handled before resolve"),
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
                Some(path) => read_agent_file(&ops, &path)?,
                None => text,
            };
            let session_id = std::env::var("KANBAN_SESSION").ok();
            ContextManager::new(ops.data_root()).append_context_with_session(
                &task_id,
                &text,
                &source,
                session_id.as_deref(),
                &ops.storage,
            )?;
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
            let session_id = agent
                .then(|| session.or_else(|| std::env::var("KANBAN_SESSION").ok()))
                .flatten();
            match ops.ask_question_for_session(
                &task_id,
                &question,
                source,
                session_id.as_deref(),
                variants,
            )? {
                Some(task) => {
                    println!("Question added to {task_id}");
                    if task.has_questions {
                        println!("Task has pending questions.");
                    }
                }
                None => eprintln!("Failed to add question to {task_id}"),
            }
        }
        Command::AskForm {
            task_id,
            file,
            agent,
            session,
        } => {
            let text = read_agent_file(&ops, &file)?;
            let form = AskForm::parse(&text)?;
            let source = if agent { "agent" } else { "user" };
            let session_id = agent
                .then(|| session.or_else(|| std::env::var("KANBAN_SESSION").ok()))
                .flatten();
            match ops.ask_form(&task_id, &form, source, session_id.as_deref())? {
                Some((task, count)) => {
                    println!("Posted {count} question(s) from form to {task_id}");
                    if task.has_questions {
                        println!("Task has pending questions.");
                    }
                }
                None => eprintln!("Failed to add questions to {task_id}"),
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
                Some(outcome) => {
                    println!("Answer added to {task_id}");
                    if outcome.remaining > 0 {
                        println!("{} question(s) still open", outcome.remaining);
                    } else if outcome.queued {
                        println!("All questions answered; agent queued for a free agent slot");
                    } else if let Some(session) = outcome.resumed_session {
                        println!("All questions answered; agent resumed on {session}");
                    } else if outcome.task.status == TaskStatus::InProgress {
                        println!(
                            "All questions answered; agent was not running and auto-resume is off"
                        );
                    }
                }
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
        Command::Reject { task_id, msg_id } => match ops.reject_message(&task_id, &msg_id)? {
            Some(_) => println!("Message {msg_id} rejected on {task_id}"),
            None => eprintln!("Message {msg_id} not found on {task_id}"),
        },
        Command::Unreject { task_id, msg_id } => match ops.unreject_message(&task_id, &msg_id)? {
            Some(_) => println!("Message {msg_id} restored on {task_id}"),
            None => eprintln!("Message {msg_id} not found on {task_id}"),
        },
        Command::Verdict {
            task_id,
            approve,
            changes,
            file,
            session,
            agent,
        } => {
            let session_id = env_session(session);
            let decision = if approve {
                Verdict::Approve
            } else {
                let mut text = changes.unwrap_or_default();
                if let Some(path) = file {
                    let from_file = read_agent_file(&ops, &path)?;
                    if text.trim().is_empty() {
                        text = from_file;
                    } else {
                        text.push('\n');
                        text.push_str(&from_file);
                    }
                }
                if text.trim().is_empty() {
                    return Err(KanbanError::Invalid(
                        "kanban verdict requires --approve or --changes <text> (or --file)"
                            .to_string(),
                    ));
                }
                Verdict::Changes(text)
            };
            match ops.submit_verdict(&task_id, &session_id, agent, decision)? {
                Some(task) => {
                    println!("Task {task_id} verdict recorded ({})", task.status.as_str())
                }
                None => eprintln!("Failed to record verdict on {task_id}"),
            }
        }
        Command::Edits { task_id, text } => match ops.set_review_edits(&task_id, &text)? {
            Some(_) => println!("Review edits saved on {task_id}"),
            None => eprintln!("Task {task_id} not found"),
        },
        Command::Rerun {
            task_id,
            session,
            now,
        } => {
            // Never park a re-run in a queue nothing can drain: with the
            // queue or auto-launch off, `rerun` starts the agent directly.
            let mode = if now || !ops.queue_can_dispatch()? {
                RunMode::Immediate
            } else {
                RunMode::Queued
            };
            match ops.rerun_review_task(&task_id, session.as_deref(), mode)? {
                Some(task) if mode == RunMode::Immediate => println!(
                    "Task {task_id} re-running ({})",
                    task.session.as_deref().unwrap_or("none")
                ),
                Some(_) => println!("Task {task_id} queued for re-run"),
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
                let context =
                    ContextManager::new(ops.data_root()).get_context(&task_id, &ops.storage)?;
                if !context.is_empty() {
                    println!("\nContext:\n{context}");
                }
            }
        }
        Command::Compact { task_id, force } => {
            let config = Config::new(ops.data_root());
            let threshold = config.get_threshold("context_auto_compact")? as usize;
            let result = CompactionManager::new(ops.data_root())
                .compact_context(&task_id, threshold, force)?;
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
            SessionManager::new(ops.data_root()).heartbeat(&session_id)?;
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
            for wake in ops.wake_expired_waits()? {
                match wake {
                    WaitWake::Queued { task_id } => {
                        println!("Wait deadline passed: {task_id} queued for a free agent slot");
                    }
                    WaitWake::Resumed {
                        task_id,
                        session_id,
                    } => {
                        println!("Resumed {task_id} after wait deadline → {session_id}");
                    }
                }
            }
            let timeout =
                Config::new(ops.data_root()).get_threshold("session_heartbeat_timeout")?;
            let crashed = SessionManager::new(ops.data_root()).check_sessions(timeout)?;
            if crashed.is_empty() {
                println!("No crashed sessions found.");
            } else {
                println!("Found {} crashed sessions:", crashed.len());
                for session in crashed {
                    println!("  {} (task: {})", session.id, session.task_id);
                }
            }
            let restarted = ops.due_restarts()?;
            for task_id in &restarted {
                println!("Crash-restart due: {task_id} handed to the queue");
            }
            let dispatched = ops.dispatch_queue()?;
            for item in &dispatched {
                println!(
                    "Dispatched {} → {} ({})",
                    item.task_id, item.session_id, item.backend
                );
            }
            for overlap in ops.detect_write_overlaps() {
                println!(
                    "Provenance overlap: {} ({}) and {} ({}) both wrote {} while running concurrently",
                    overlap.task_a,
                    overlap.session_a,
                    overlap.task_b,
                    overlap.session_b,
                    overlap.path
                );
            }
            let availability = crate::core::vcs::availability(ops.work_path());
            if availability.is_available() {
                println!("Isolation: available");
            } else {
                println!("Isolation: unavailable — {availability}");
            }
        }
        Command::Recover { task_id } => match ops.recover_task(&task_id)? {
            Some(_) => {
                println!("Task {task_id} recovered and moved to To Do");
            }
            None => eprintln!("Task {task_id} not found"),
        },
        Command::Integrate { task_id } => match ops.integrate_task(&task_id)? {
            Some((task, LandOutcome::Landed { changed })) => {
                println!(
                    "Landed {} into {} ({} path(s), unstaged):",
                    task.branch.as_deref().unwrap_or("branch"),
                    ops.work_path().display(),
                    changed.len()
                );
                for path in changed {
                    println!("  {}", path.display());
                }
                println!("Commit manually after review; the integration ref has advanced.");
            }
            Some((task, LandOutcome::Conflict { paths })) => {
                println!(
                    "Merge conflicts landing {} — nothing was written to the work folder:",
                    task.id
                );
                for path in paths {
                    println!("  {path}");
                }
                println!(
                    "Resolve in the work folder or the worktree, then run \
                     \"kanban integrate {}\" again.",
                    task.id
                );
            }
            Some((_, LandOutcome::Deferred(reason))) => {
                eprintln!("Landing deferred: {reason}");
            }
            Some((task, LandOutcome::NotIsolated)) => {
                eprintln!("Task {} has no isolated branch to integrate", task.id);
                return Ok(ExitCode::FAILURE);
            }
            None => eprintln!("Task {task_id} not found"),
        },
        Command::Stop { task_id } => match ops.stop_task(&task_id)? {
            Some(task) => {
                let session = task.session.as_deref().unwrap_or("unknown");
                println!("Stopped {task_id} session {session}");
            }
            None => {
                eprintln!("Task {task_id} not found");
                return Ok(ExitCode::FAILURE);
            }
        },
        Command::Sessions => {
            let session_mgr = SessionManager::new(ops.data_root());
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
                let tokens = estimate_session_tokens(ops.data_root(), &session.id);
                let tokens_label = tokens.map_or("unknown".to_string(), format_thousands);
                println!(
                    "▶ {:<26} {:<36} {:<12} {:<16} {}",
                    session.id, task_label, tokens_label, started, last_seen
                );
            }
        }
        Command::Tui
        | Command::Limits { .. }
        | Command::Update { .. }
        | Command::StatuslineBridge
        | Command::Daemon { .. } => {
            unreachable!("handled before resolve")
        }
        Command::Attach { task_id } => {
            let Some(task) = ops.storage.load_task(&task_id)? else {
                eprintln!("Task {task_id} not found");
                return Ok(ExitCode::FAILURE);
            };
            // A task keeps its last session id after that session ends, so
            // presence alone does not mean there is anything to attach to.
            let session_mgr = SessionManager::new(ops.data_root());
            let Some(session_id) = task
                .session
                .as_deref()
                .filter(|session_id| session_mgr.is_session_active(session_id))
            else {
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
        Command::ResolveAgent { .. } | Command::FormatStream => {
            unreachable!("handled before resolve")
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Reformat a claude stream-json transcript (read line by line) into
/// human-readable text. Recognized events are rendered (assistant text, tool
/// one-liners, final result); other JSON events are dropped; non-JSON lines
/// (backend stderr, banners) pass through untouched so nothing is lost.
fn format_stream(input: impl std::io::BufRead, output: &mut impl std::io::Write) -> Result<()> {
    for line in input.lines() {
        let line = line?;
        match serde_json::from_str::<serde_json::Value>(line.trim()) {
            Ok(value) => {
                if let Some(rendered) = crate::core::provenance::render_stream_event(&value) {
                    writeln!(output, "{rendered}")?;
                }
            }
            Err(_) => writeln!(output, "{line}")?,
        }
    }
    Ok(())
}

fn wait_resume_monitor(ops: &Operations, task_id: &str, session_id: &str) -> Result<()> {
    let session_mgr = SessionManager::new(ops.data_root());
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
        let woken = ops.wake_expired_waits()?;
        if !woken.is_empty() {
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
        "use_designer": task.use_designer,
        "use_reviewer": task.use_reviewer,
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
