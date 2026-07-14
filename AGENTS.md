# AGENTS.md

## Project: kanban4ai

A local kanban board application for task management within projects, driven by
AI coding agents (opencode, Claude Code) via CLI commands. Native Rust rewrite
of the Python `kanban-cli`; the on-disk format and the CLI contract are fully
compatible with boards created by the original.

### Architecture
- **Type**: Standalone CLI tool + TUI (NOT an opencode plugin)
- **Language**: Rust (stable, edition 2024)
- **TUI Framework**: ratatui + crossterm
- **Storage**: File-based (Markdown + YAML frontmatter), no database
- **Integration**: Shell command calls from agents; binary `kanban4ai` with
  `kanban` / `kb` symlinks

### Rewrite status
Порт на Rust завершён: реализованы ядро данных, полный CLI, business logic,
запуск агентов, нативный TUI и release/AUR packaging. Исходники прежней
Python-версии удалены; `tests/fixtures/` сохранены для проверки совместимости
формата существующих досок.

### Directory Structure
```
src/
├── main.rs              # Binary entry point (SIGPIPE reset + cli::run)
├── lib.rs
├── cli.rs               # clap CLI: every `kanban` command, Python-compatible output
└── core/
    ├── mod.rs
    ├── error.rs         # KanbanError (Io/Yaml/Invalid/Permission) / Result
    ├── timefmt.rs       # Python-isoformat timestamps (parse/format/serde)
    ├── models.rs        # Task, Session, Thread, Message, enums
    ├── config.rs        # BoardConfig + per-project .kanban/config.yaml loader
    ├── storage.rs       # Task file I/O, atomic writes, board lock, fingerprint
    ├── thread.rs        # ThreadManager: sidecar threads, merge-on-save
    ├── operations.rs    # Business-logic hub: CRUD, rules, questions, chaining,
    │                    #   review edits; AgentLauncher seam
    ├── session.rs       # SessionManager: heartbeats, crash detection, token estimate
    ├── context.rs       # ContextManager: thread-based context + legacy back-compat
    ├── compaction.rs    # Rule-based context compaction (no LLM)
    └── notifier.rs      # Desktop notifications (notify-send)
Additional modules:
    agent/               # process manager, tmux wrapper, backends, prompts
    tui/                 # ratatui board, detail, dialogs, search, sessions
.github/workflows/       # CI and tagged Linux release automation
packaging/aur/           # stable and VCS Arch source packages
scripts/                 # POSIX installer and packaging smoke test
tests/
├── fixtures/            # golden files written by the Python version
├── golden_compat.rs     # lossless load/round-trip of Python-written files
├── storage_test.rs, thread_test.rs, config_test.rs
├── operations_test.rs   # agent rules, questions, chaining, review edits
├── cli_test.rs          # end-to-end binary tests (assert_cmd)
```

### Data Model
- **Task**: id (TASK-NNN), title, description, status (todo/in_progress/review/done/archive), session, has_questions, interactive, ai_model, agent_backend, agent_name, chained_to, review_edits. `description` is the **user-authored task only** — agent work-context lives in the thread (see "Context, questions & review edits"). `interactive: true` enables the thread-based blocking question loop for delegated agents. `chained_to` is an optional target task id: when that target enters Review, this task auto-runs (see "Task Chaining"). `review_edits` is the single editable buffer for the human's review feedback; it is folded into the thread and cleared on the next re-run from Review.
- **Session**: id, task_id, started_at, status (active/closed/crashed), last_seen
- **MessageRole** / **MessageKind** / **MessageStatus**: enums for thread message author, type, and lifecycle state. `MessageKind` is one of `system`, `task`, `question`, `suggestion`, `context`, or `review_edit`.
- New tasks initialize their sidecar thread with `system` and `task` messages: `MSG-001` records creation metadata, `MSG-002` stores the initial user-authored task body so the TUI can render the whole conversation from the thread.
- **Message**: thread entry with `id` (MSG-NNN), role, kind, status, body, `parent_id`, `variants`, author, timestamps, and resolution metadata. Answered questions also store `answer` and `answered_by_role`.
- **Thread**: sidecar per-task conversation state with `task_id`, `rev`, and ordered `messages`.
- **BoardConfig**: columns, rules, thresholds (all configurable per-project).

### Storage Format
```markdown
---
id: TASK-001
title: Fix login bug
status: todo
session: null
created_at: '2026-06-01T10:00:00'
updated_at: '2026-06-01T10:00:00'
has_questions: false
interactive: false
ai_model: openai/gpt-5.5
review_edits: ''
---
User-authored task description only — agent context is NOT embedded here.
```

Timestamps use the Python `datetime.isoformat()` dialect (naive local time,
microseconds omitted when zero) — `src/core/timefmt.rs` is the single
implementation. Legacy boards that embedded a `## Context` heading in the
description (or spilled to `.kanban/context/<task>.md`) are still read for
back-compat, but new context is always written to the thread.

### Context, questions & review edits
The task `description` holds **only** what the human wrote. Everything the agent
adds, and the human's review feedback, lives in the sidecar thread:
- **Agent context** — `kanban context <id> <text>` posts a `context` message.
- **Questions** — `kanban ask` posts a `question`; once answered the reply is
  stored on the same message (`answer` + `answered_by_role`).
- **Review edits** — while a task sits in Review the human types feedback into
  the single `review_edits` buffer (`kanban edits`, or the TUI Review-edits
  field). On the next re-run (`kanban rerun` / TUI "Save & Re-run") the buffer
  is folded into the thread as a permanent `review_edit` message and cleared.

### Thread Storage Format
Sidecar YAML at `.kanban/threads/TASK-NNN.yaml`: `task_id`, `rev`, ordered
`messages`. Saves are optimistic read-modify-write: the manager re-reads the
file, merges concurrent changes by message id (additions from both sides
survive; a message is only overwritten by a writer that actually changed it
relative to its loaded base), and writes `rev + 1` atomically.

```yaml
task_id: TASK-001
rev: 3
messages:
- id: MSG-001
  role: agent
  kind: question
  body: Should I use JWT?
  status: answered
  parent_id: null
  variants:
  - JWT
  - Session cookie
  author: opencode
  answer: Use JWT.
  created_at: '2026-06-01T10:00:00'
  updated_at: '2026-06-01T10:02:00'
  answered_by_role: human
  resolved_at: '2026-06-01T10:02:00'
```

### CLI Commands (implemented)
- `kanban init` - Initialize .kanban/ board
- `kanban create <title> [--backend opencode|claude] [--model M] [--agent-name P] [--interactive] [--chain-to TASK-NNN]` - Create task
- `kanban chain <id> [<target_id>] [--clear]` - Show, set, or clear chaining
- `kanban list` - List tasks
- `kanban show <id>` - Show task details
- `kanban take <id> --session <id> --agent` - Take task for an agent
- `kanban done <id> --session <id> --agent` - Complete task
- `kanban move <id> <column>` - Move task
- `kanban context <id> <text>` - Add a `context` message to the thread
- `kanban ask <id> <question> [--wait] [--variants TEXT ...] [--timeout SECONDS] [--session <id>]` - Add question, optionally block until answered
- `kanban answer <id> <index> <answer>` - Answer question
- `kanban questions <id>` - List open thread messages
- `kanban suggest <id> <suggestion>` - Add suggestion
- `kanban edits <id> <text>` - Set the review-edits buffer
- `kanban rerun <id> [--session <id>]` - Fold review edits into the thread and re-run the agent
- `kanban compact <id>` - Compact context (rule-based, no LLM)
- `kanban heartbeat --session <id>` - Update session heartbeat
- `kanban check-sessions` - Find crashed sessions
- `kanban recover <id>` - Recover crashed task
- `kanban sessions` - List active sessions
- `kanban archive` - List archived tasks
- `kanban archive-done` - Move all Done tasks to Archive
- `kanban tui` - Launch interactive board
- `kanban attach <id>` - Attach to the task's running agent tmux session

### Agent Rules (Enforced with --agent flag)
1. `one_task_per_instance`: Block an agent from taking multiple tasks
2. `user_only_review_to_done`: Only user can move Review -> Done
3. `auto_move_on_assign`: Move to In Progress on take
4. `auto_move_on_complete`: Move to Review on agent done
5. `questions_go_to_review`: If true, questions move task to Review; if false, keep in In Progress
6. `auto_launch_on_delegate`: On agent `take`, auto-launch the backend for the task (gated by `auto_launch.enabled`)
7. `auto_launch_chained`: When a task enters Review, auto-launch every To Do task whose `chained_to` points at it (gated by `auto_launch.enabled`)

When `interactive: true`, delegated agents are instructed to use `kanban ask --wait` for blocking questions and `kanban suggest` for non-blocking ideas.

### Configurable Thresholds (per-project .kanban/config.yaml)
- `context_embed_max_size`: 5120 (5KB) - inline vs separate file
- `context_warning`: 51200 (50KB) - warn about large context
- `context_auto_compact`: 102400 (100KB) - auto-compress
- `session_heartbeat_timeout`: 300 (5 min) - mark crashed
- `context_summary_max_length`: 5000 chars
- `tui_refresh_interval`: 1 (sec) - TUI refresh fallback (primary refresh is inotify)
- `question_poll_interval`: 3 (sec) - poll interval for `kanban ask --wait`
- `question_wait_timeout`: 600 (sec) - default timeout for `kanban ask --wait`

### TUI Settings (.kanban/config.yaml `tui:`)
- `card_height_lines`: 4 - task card height
- `card_line_max_symbols`: 40 - fixed one-line preview length before adding `...`
- `max_tasks_per_column`: 100 - cap rendered per column
- `theme`: theme name (toggle/persist via `Ctrl+T`)

### Notification Settings (.kanban/config.yaml `notifications:`)
- `enabled`: true - master switch for desktop notifications
- `questions`: true - notify when a task raises a question
- `completion`: true - notify when a task is completed or ready for review
- `chained_start`: true - notify when a chained task auto-starts
- `command`: `notify-send` - notification command
- `timeout`: 3 - command timeout in seconds
- `max_body_chars`: 240 - truncate notification body beyond this length

### Auto-Launch Settings (.kanban/config.yaml `auto_launch:`)
Controls how delegating a task spawns a background agent job (shared across all backends):
- `enabled`: true - master switch for auto-launching
- `use_tmux`: true - host the agent in a tmux session (falls back to a direct background process if tmux is missing)
- `terminal_fallback`: true
- `auto_complete_on_exit`: false - whether agent exit auto-completes the task
- `default_agent`: opencode - backend used when a task has no `agent_backend`
- `model` / `models` / `agent`: opencode back-compat mirrors of `agents.opencode.*`

### Agent Backends (.kanban/config.yaml `agents:`)
Each task carries an `agent_backend` field selecting which CLI runs it. When unset, `auto_launch.default_agent` is used; an unknown backend falls back to `opencode`. The `agents:` map defines one entry per backend:
- `command`: executable resolved via PATH (e.g. `opencode`, `claude`)
- `model`: default model when a task has no `ai_model`
- `models`: list offered in the TUI create/edit dialog for this backend
- `agent`: optional default `--agent` persona (overridden per task by `task.agent_name`)
- `agent_options` (opencode only): personas offered in the TUI and via `kanban create --agent-name` (e.g. `sisyphus`, `prometheus`, `hephaestus`, `atlas`)
- `extra_args`: extra CLI flags inserted before `--model`

Per-task persona: `task.agent_name` is passed to opencode as `--agent`, overriding the backend default. opencode matches `--agent` against an agent's *exact* registered name (oh-my-openagent personas are decorated strings); at launch the friendly key is resolved via `opencode agent list` (cached). If opencode is unavailable the key is passed through unchanged. The claude backend ignores `agent_name`.

Built-in backends:
- **opencode**: `opencode run --title "<id>: <title>" [extra_args] [--model M] [--agent A] <prompt>`
- **claude** (Claude Code): `claude --print [extra_args] [--model M] [--agent A] <prompt>`. Default `extra_args` is `["--dangerously-skip-permissions"]` — tighten in config for stricter permissions. Default models are the `sonnet`/`opus`/`haiku` aliases.

### TUI Keyboard Shortcuts
- `↑/↓/←/→`: Move focus between tasks/columns
- `Tab` / `Shift+Tab`: Next/previous column
- `Enter`: Show task detail
- `s`: Start/Delegate to agent (with confirmation dialog)
- `n`: New task
- `e`: Edit task
- `d` / `Ctrl+d` / `Delete` / `Backspace`: Delete task
- `m`: Move task
- `w`: Open the answer-question dialog
- `r`: Recover crashed task
- `a`: Show archived tasks
- `l`: Show running sessions
- `Ctrl+t`: Change theme (persisted to config)
- `/`: Search
- `?`: Help overlay
- `q`: Quit

Note: the opencode subscription/usage overlay (`u`) from the Python version was
dropped in the rewrite — it never worked reliably.

The detail view renders the thread (open questions, variants, suggestions,
resolved entries) plus the task's `chained_to` target. Create/edit dialogs
expose an `interactive` checkbox and a "Chain to task" selector.

### Integration Model
Agents call kanban via shell commands. NOT a plugin. An agent must:
1. Set `KANBAN_SESSION` environment variable
2. Use `--agent` flag for all commands
3. Call `kanban heartbeat` periodically while working
4. Add context via `kanban context`
5. Ask questions via `kanban ask`, or `kanban ask --wait --session <id>` when the task is interactive and the question is blocking
6. Mark done via `kanban done`

Closure invariant for non-interactive agent jobs: after implementation and verification are complete, do not stop at a progress update, green test report, or pending specialist review. Record final context and run `kanban done <id> --session <id> --agent` in the same execution unless a blocking ambiguity requires `kanban ask --agent` and an immediate stop.

### Agent Auto-Launch
When a task is delegated (`take --agent`, or the TUI `s` action) and auto-launch is enabled, the CLI spawns the agent itself:
- Builds a non-interactive command per backend (see "Agent Backends"). Model resolves from `task.ai_model`, else the backend default.
- The prompt instructs the agent to: work only on this task, use the provided `KANBAN_SESSION`/`KANBAN_TASK_ID` env vars, back up touched files, record progress via `kanban context`, and finish with `kanban done --agent`. When `interactive: true`, blocking questions go through `kanban ask --wait --session <id>`. The prompt stays backend-neutral.
- If `use_tmux` and tmux is available → runs inside a detached tmux session (reattachable via `kanban attach`); otherwise falls back to a background process. Either way stdout/stderr is teed to `.kanban/logs/<session>.log`. Session ids are prefixed by backend (`ses-<backend>-...`).
- Agent exit is watched to reconcile task/session state.

### Task Chaining
A task may carry a `chained_to` target task id. When the **target** task enters Review — via `move` or an agent's `done` — every task whose `chained_to` equals that id and is still in **To Do** is auto-run with a fresh per-task session (its own backend/model/persona/description). Only the To-Do→Review transition fires it (re-entering Review does not). Gated by the `auto_launch_chained` rule and `auto_launch.enabled`.

### Backup & Revert
- Delegated agents are told to copy each existing file they touch into `.kanban/backups/<task_id>/` preserving its repo-relative path.
- Revert spawns a second agent job whose prompt restores every file under that backup dir. Requires existing backups.
- Completing/abandoning a task clears its backups, logs, and session files.

### Image Attachments
Paste an image from the clipboard (`wl-paste`/`xclip`, or a file path in clipboard text), sniff the type by magic bytes (png/jpg/gif/webp), write it atomically under `.kanban/assets/images/`, and embed Markdown (`![pasted image](...)`) in the task description.

### Token Estimation
Session token estimates are parsed from the agent's `.kanban/logs/<session>.log` for the running-sessions view.

### Storage Directories (under .kanban/)
- `tasks/<status>/` - task Markdown files (status = subdirectory)
- `threads/` - per-task YAML threads with optimistic `rev` merge
- `context/` - legacy: large context from older boards (read-only back-compat)
- `sessions/` - per-session YAML (metadata + heartbeat)
- `logs/` - per-session agent run logs
- `backups/<task_id>/` - pre-edit file backups for revert
- `assets/images/` - pasted image attachments
- `.lock` - board-wide flock serializing read-modify-write cycles

### Development Rules
- All thresholds configurable via .kanban/config.yaml — no hardcoded values in business logic
- Atomic file writes (temp file + rename) via `storage::atomic_write_text`
- Any task read-modify-write cycle holds the board lock (`Storage::lock`)
- Context compaction is rule-based (no LLM)
- Tests required: `cargo test --locked` must stay green; golden fixtures in `tests/fixtures/` guard legacy board-format compatibility — never regenerate them from Rust output
- `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt` applied
- Release builds use the single `kanban4ai` binary; installers create relative `kanban` and `kb` symlinks
- No database dependencies
- Compatible with existing opencode plugins (doesn't modify opencode internals)
- No committing without explicit user request
