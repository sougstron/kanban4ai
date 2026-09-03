# CLI command reference

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when you need the exact flags of a `kanban` subcommand (`kanban <cmd> --help` is authoritative).

## CLI Commands (implemented)
- Global `--project <id|name|path>` on every subcommand (overrides cwd / `$KANBAN_PROJECT`)
- `kanban init [--path P] [--copy] [--force]` - Register the folder in the store and create the board there (never a local `.kanban/`). Migrates an existing `<P>/.kanban` into the store. Repeat init is a no-op exit 0
- `kanban project list [--format table|json]` - List registered projects
- `kanban project add [PATH] [--name NAME] [--copy] [--force]` - Register a folder (migrating a local `.kanban` if present)
- `kanban project show <id|name|path>` - Show one project (id, name, work path, data root, timestamps)
- `kanban project rename <id|name> <new-name>` - Change the display name (id stays put) and write `tui.name` so the projects list shows it
- `kanban project set-path <id|name> <path>` - Repoint the work folder
- `kanban project path [id|name|path]` - Print the work path (defaults to the current project)
- `kanban project remove <id|name> [--purge] [--yes]` - Unregister; `--purge` also deletes board data. Interactive confirm unless `--yes`
- `kanban project open <id|name|path>` - Open the TUI on that project
- `kanban create <title> [--backend opencode|claude|omp|pi] [--model M] [--effort E] [--agent-name P] [--interactive] [--designer] [--reviewer] [--chain-to TASK-NNN]` - Create task. `--designer` / `--reviewer` opt this task into the project designer or reviewer bot without turning that bot on for the whole board.
- `kanban chain <id> [<target_id>] [--clear]` - Show, set, or clear chaining
- `kanban list` - List tasks
- `kanban show <id>` - Show task details
- `kanban take <id> --session <id> --agent` - Take task for an agent
- `kanban done <id> --session <id> --agent` - Complete task
- `kanban move <id> <column>` - Move task
- `kanban context <id> <text>` - Add a `context` message to the thread
- `kanban ask <id> <question> [--wait] [--variants TEXT ...] [--timeout SECONDS] [--session <id>]` - Add question, optionally block until answered
- `kanban ask-form <id> --file <path> [--agent] [--session <id>]` - Post one or more questions from a strict YAML form (each entry's `options` become answer variants)
- `kanban answer <id> <index> <answer>` - Answer question
- `kanban waiting <id> [--session <id>] [--eta SECONDS] [--note TEXT]` - Declare a long-running wait; records a thread note and keeps the session alive until `eta × waiting_eta_multiplier`. A pause releases the agent slot: when the deadline passes the task re-enters the queue (or, with the queue off, the agent is relaunched directly) to check the result
- `kanban detach <id> [--session <id>] [--eta SECONDS] [--note TEXT] -- <command> [args...]` - Run a command fully detached from the agent session (own `setsid` session, so it survives the tmux host being killed when the reply ends), append output to `.kanban/detached/<task>-<stamp>.log`, write the exit code to the matching `.status` file, and declare the wait in one step; the wait note carries both paths into the relaunch prompt
- `kanban questions <id>` - List open thread messages
- `kanban suggest <id> <suggestion>` - Add suggestion
- `kanban edits <id> <text>` - Set the review-edits buffer
- `kanban verdict <id> (--approve | --changes <text> [--file <path>]) --session <id> --agent` - The bot reviewer's only exit (see "Run Phases"). `--agent` is required and the session must be the task's current reviewer session on a task that is In Progress with phase `review`. `--approve` clears the phase and moves the task to human Review (chained tasks and the completion notification fire as usual); `--changes` writes the text into the `review_edits` buffer, folds it into the thread, and routes per `orchestration.reviewer.on_changes_requested`. `--file` reads the text from a file for longer write-ups; empty change text is rejected
- `kanban rerun <id> [--session <id>] [--now]` - Fold review edits into the thread and re-queue the run (the dispatcher starts it; the CLI does not pump the queue). `--now` bypasses the queue and launches immediately, as does the automatic fallback when the queue could never drain
- `kanban compact <id>` - Compact context (rule-based, no LLM)
- `kanban heartbeat --session <id>` - Update session heartbeat
- `kanban check-sessions` - The manual headless pump: resume expired waits, reap crashed sessions, hand due crash-restarts back to the queue (`due_restarts`), then `dispatch_queue` and print what each step did. Ends with an `Isolation:` line — `available`, or `unavailable — <reason>` (project not registered, git not found, git too old for `merge-tree`, not a git repository, unborn HEAD, detached HEAD)
- `kanban daemon [--interval SECONDS] [--once] [--project <p>]` - Foreground loop (does not fork) that ticks every registered project: resume expired waits, reap crashed sessions, `due_restarts()`, then `dispatch_queue()`. `--once` is one tick for cron or a systemd timer; the plain loop is what the user unit runs. Default interval is 60s from the store `daemon.interval` (`--interval` overrides). `flock`s `<store>/daemon.lock` and refuses a second daemon; a TUI pumping at the same time is fine. Projects with `orchestration.queue_enabled: false` or a missing work folder are skipped (one warning for a gone folder). Logs one line per resume/reap/restart/dispatch to `<store>/logs/daemon.log` and stdout. Cron fallback: `* * * * * kanban daemon --once`. Opt-in user unit: `scripts/install.sh --with-daemon` (never enabled).
- `kanban recover <id>` - Recover crashed task
- `kanban stop <id>` - Stop the task's running agent session; the task stays In Progress (idle)
- `kanban sessions` - List active sessions
- `kanban archive` - List archived tasks
- `kanban archive-done` - Move all Done tasks to Archive
- `kanban limits [--format table|json] [--refresh]` - Remaining subscription capacity per provider (claude, grok, zai, synthetic, yolo); serves the cached snapshot unless it aged out or `--refresh` is given
- `kanban limits bridge install` / `kanban limits bridge remove` - Wrap / unwrap Claude Code's statusline command with the bridge feeding the claude segment of the limits row
- `kanban stats` - Print the application-collected usage report (tokens and time, by backend/model/project, all time / this month / this week) across every registered project — see `docs/stats.md`
- `kanban update [--check]` - Report (or install) the newest GitHub release; see "Updater". Project-independent: runs from any directory with no board. A status cached within `updates.check_interval_hours` answers from the cache, otherwise one blocking check runs; `--check` only prints the report. Without `--check` a newer release is downloaded, verified, and installed — refused with the upgrade command when pacman owns the binary
- `kanban tui` - Launch the interactive board; with no resolved project, open the projects list
- `kanban attach <id>` - Attach to the task's running agent tmux session
- `kanban integrate <id>` - Land an isolated task branch into the work folder by hand — the manual counterpart of automatic landing (`land: manual`, or a deferred landing); refuses non-isolated tasks and re-integrating an already-landed one, prints landed paths, conflicting paths, or the deferral reason (see "Worktree Isolation")
