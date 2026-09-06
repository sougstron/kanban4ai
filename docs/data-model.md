# Data model and on-disk formats

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when the task/thread file format, the board directory layout, or the project store.

## Data Model
- **Task**: id (TASK-NNN), title, description, status (todo/in_progress/review/done/archive), session, has_questions, interactive, use_designer, use_reviewer, use_orchestrator, depends_on, needs, parent_task, role_profile, roster_index, orchestrated, ai_model, ai_effort, agent_backend, agent_name, chained_to, launch_at, review_edits, auto_resumes, completed_at, run_phase, crash_restarts, restart_at, review_rounds, designed, worktree, branch, base_commit, integration. `description` is the **user-authored task only** — agent work-context lives in the thread (see "Context, questions & review edits"). `interactive: true` selects the blocking-question guidance for delegated agents (`kanban ask --wait`); resume-after-answer now applies to every task regardless of this flag (rule `resume_after_last_answer`). `use_designer` / `use_reviewer` opt this task into the project designer or reviewer bot even when that bot is off board-wide; models and agents still come from `orchestration.designer` / `orchestration.reviewer`. Either flag ORs with the matching project `enabled` switch. Omitted from frontmatter while false. `chained_to` is an optional target task id: when that target enters Review, this task auto-runs (see "Task Chaining"). `launch_at` is an optional local timestamp — the next occurrence of an HH:MM chosen in the TUI or via `kanban create --launch-at` — that makes the queue dispatcher enqueue the To Do task when it comes due; the enqueue consumes it (see "Planned launches" in `docs/orchestration.md`). Omitted from frontmatter while unset. `use_orchestrator` opts the task into the orchestrator bot — a plan-only first pass that decomposes it into a subtask graph — and has no board-wide counterpart on purpose; `orchestrated` records that a plan was accepted, gating that pass exactly like `designed` gates the design pass. `depends_on` is the DAG edge set: task ids that must reach Review or Done before this task becomes ready, and whose results are prepended to its prompt. It is deliberately not `chained_to` — a chain is one parent, pushed, carrying no context, while a dependency is an AND-join that is *pulled* by a readiness sweep and carries the upstream results across (see "Task Dependencies"). `needs` is the orchestrator's one-line contract for what this node takes from upstream, shown above those results; `parent_task` names the orchestrated task that planned this one; `role_profile` and `roster_index` name the `orchestration.roles` roster the node runs on and which candidate is in use (advanced on a provider-limit failure, never reset automatically). `review_edits` is the single editable buffer for the human's review feedback; it is folded into the thread and cleared on the next re-run from Review. `auto_resumes` counts consecutive automatic relaunches after clean exits or expired waits and resets on human starts/recoveries. `completed_at` records the most recent transition that completed work into Review or Done; a rerun keeps the previous value while active and replaces it when the agent completes again. `session` names the **last** session that worked the task, not only a running one: it survives the session's end (done, stop, recover, unarchive, failed launch) so the task keeps a record of who ran it, and is overwritten by the next session. Whether that session is alive is decided by its session record — never by this field being set. `agent_backend`/`ai_model`/`ai_effort`/`agent_name` record what the task will run with: creating or saving with those fields left as default snapshots the resolved board defaults (the default backend and that backend's configured model/effort/agent) onto the task, and each executor launch pins the values it resolved. Designer/reviewer launches must not overwrite the task's assigned executor settings. `run_phase` is the In Progress sub-state (`queued`/`orchestrate`/`design`/`execute`/`review`, see "Run Phases"); it is `None` on every other column and on legacy boards, where it reads as `execute`. `crash_restarts` counts consumed crash auto-restarts and `restart_at` is the pending backoff deadline (both distinct from `auto_resumes`); `review_rounds` counts consumed bot-review bounces. `designed` records that a designer pass already finished and its plan is on the thread. `worktree` / `branch` / `base_commit` / `integration` carry worktree-isolation state (see "Worktree Isolation"): the isolated checkout's path relative to `.kanban/worktrees/`, its branch (`<branch_prefix><TASK-ID>`), the snapshot oid the branch was cut from, and the landing state (`none`/`pending`/`landed`/`conflict`). All four are omitted from frontmatter while unset, so legacy task files round-trip byte-identically. The relaunch bookkeeping is cleared in two grades: `Task::reset_auto_restart()` clears `auto_resumes`, `crash_restarts` and `restart_at`, and `Task::reset_human_restart()` clears those **and** `review_rounds`. A human restart of the *work* (run, re-run from Review, recover, take, queue) uses the second; a human nudge to a run that is still the same run (wake/revoke, re-run of a stranded session) uses the first, so a task woken mid-review does not re-arm `reviewer.max_rounds` from zero and reopen the bounce loop the cap exists to stop.
- **Session**: id, task_id, started_at, status (active/closed/crashed), last_seen, wait_until, wait_note, wait_exited. `wait_until`/`wait_note` are set by `kanban waiting`; `wait_exited` means the agent process ended during the declared wait — at the deadline the pause is handed back to the queue (or, with the queue off, the agent is relaunched directly) to check the result.
- **MessageRole** / **MessageKind** / **MessageStatus**: enums for thread message author, type, and lifecycle state. `MessageKind` is one of `system`, `task`, `question`, `suggestion`, `context`, or `review_edit`.
- New tasks initialize their sidecar thread with `system` and `task` messages: `MSG-001` records creation metadata, `MSG-002` stores the initial user-authored task body so the TUI can render the whole conversation from the thread.
- **Message**: thread entry with `id` (MSG-NNN), role, kind, status, body, `parent_id`, `variants`, author, timestamps, and resolution metadata. Answered questions also store `answer` and `answered_by_role`.
- **Thread**: sidecar per-task conversation state with `task_id`, `rev`, and ordered `messages`.
- **BoardConfig**: columns, rules, thresholds (all configurable per-project).

## Storage Format
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

The orchestration fields are written only when they carry a value, so a board
that never used the queue round-trips byte-identically against the golden
fixtures: `run_phase` and `restart_at` are omitted while `None`,
`crash_restarts` and `review_rounds` while `0`, `designed` while `false`. A task mid-run looks like this:

```yaml
status: in_progress
run_phase: queued
crash_restarts: 1
restart_at: '2026-06-01T14:32:00'
review_rounds: 1
```

`restart_at` uses the same timestamp dialect as every other datetime field.

Timestamps use the Python `datetime.isoformat()` dialect (naive local time,
microseconds omitted when zero) — `src/core/timefmt.rs` is the single
implementation. Legacy boards that embedded a `## Context` heading in the
description (or spilled to `.kanban/context/<task>.md`) are still read for
back-compat, but new context is always written to the thread.

## Context, questions & review edits
The task `description` holds **only** what the human wrote. Everything the agent
adds, and the human's review feedback, lives in the sidecar thread:
- **Agent context** — `kanban context <id> <text>` posts a `context` message.
- **Questions** — `kanban ask` posts a single `question`; once answered the
  reply is stored on the same message (`answer` + `answered_by_role`). For one
  or more structured questions at once, `kanban ask-form <id> --file <path>`
  reads a strict YAML form and posts one `question` per entry, mapping each
  entry's `options` onto the message `variants` (the selectable answers in the
  TUI answer panel). The form schema (agents are instructed to write this):

  ```yaml
  questions:
    - id: q1                 # optional, agent-facing label
      prompt: Which backend? # required, non-empty
      options: [OAuth2, API key]   # optional → answer variants
      allow_custom: true     # optional, default true; false + options appends
                             # a "pick one of the listed options" hint (advisory)
    - prompt: Any constraints?     # add as many entries as needed
  ```

  Empty `questions` or a blank `prompt` is rejected; malformed YAML is a YAML
  error. Delegated agents are prompted to prefer `ask-form` and to proactively
  file non-blocking ideas via `kanban suggest`.

  Answering the task's **last** open question wakes the agent (rule
  `resume_after_last_answer`, gated by `auto_launch.enabled`),
  for every task — `interactive` only selects the `ask --wait` guidance. A live
  `ask --wait` poller is left alone because it wakes itself on the answer; a
  session whose heartbeat went stale is marked crashed and replaced. A woken
  pause re-enters the queue instead of launching past the caps (only the
  `wake_expired_waits` / revoke paths hold that fence; see "Run Phases"). With
  `questions_go_to_review: true` the ask moves the task to Review, so the
  In-Progress fence means answering there does not auto-resume.
- **Suggestions** — `kanban suggest <id> <text>` posts a non-blocking
  `suggestion` message. Every delegated-agent prompt now nudges agents to record
  ideas, risks, and better alternatives this way without stopping their work.
- **Agent reply** — when an agent session exits, its closing answer (the
  summary it printed as its last words) is posted as a `context` message
  authored by `agent-reply`. See "Agent Reply Capture".
- **Review edits** — while a task sits in Review the human types feedback into
  the single `review_edits` buffer (`kanban edits`, or the TUI Review-edits
  field). On the next re-run (`kanban rerun` / TUI "Save & Re-run") the buffer
  is folded into the thread as a permanent `review_edit` message and cleared.

## Thread Storage Format
Sidecar YAML at `.kanban/threads/TASK-NNN.yaml`: `task_id`, `rev`, ordered
`messages`. Saves are optimistic read-modify-write: the manager re-reads the
file, merges concurrent changes by message id (additions from both sides
survive; a message is only overwritten by a writer that actually changed it
relative to its loaded base), and writes `rev + 1` atomically.

Task ids are recycled (`Storage::get_next_id` hands out `max + 1`), so a thread
must never outlive its task: `abandon_task` deletes the sidecar with the task,
and task creation drops any thread already sitting on the fresh id. Otherwise a
new task adopts the deleted task's messages — `initialize_task_thread` keeps an
existing thread as-is — and shows them in the detail view and in the agent's
prompt.

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

## Storage Directories (under `<data_root>/.kanban/`)

Board data lives in the projects store, not in the work folder. The layout
inside `.kanban/` is unchanged (see **Projects & Store** for where
`<data_root>` is). An unregistered board still used in place keeps the same
tree at `<work>/.kanban/`.

- `tasks/<status>/` - task Markdown files (status = subdirectory)
- `threads/` - per-task YAML threads with optimistic `rev` merge
- `context/` - legacy: large context from older boards (read-only back-compat)
- `sessions/` - per-session YAML (metadata + heartbeat)
- `logs/` - per-session agent run logs
- `detached/` - `kanban detach` job artifacts: `<task_id>-<stamp>.log` (output) and `.status` (exit code); cleared with the task's logs
- `worktrees/<task_id>/` - per-task isolated git checkouts (see "Worktree Isolation"); removed on land (with `cleanup: on_land`), Done, abandon, and by the GC pass
- `recent_models` - most recently launched catalog-backend models (opencode/omp/pi), newest first (drives TUI model-selector ordering)
- `stats/events.jsonl` - append-only usage-statistics event log the board writes itself (never agents); see `docs/stats.md`
- `instructions/<role>.md` - optional per-role prompt additions (`orchestrator`, `designer`, `reviewer`, `executor`), appended only to that role's prompt when that role is launched — unlike `AGENTS.md`, which every session pays for
- `backups/<task_id>/` - pre-edit file backups for revert
- `assets/images/` - pasted image attachments
- `.lock` - board-wide flock serializing read-modify-write cycles

Provider limit snapshots are machine-wide, not per board: they live in
`<store>/limits.json`, next to the claude statusline bridge's
`claude-rate-limits.json` and the `claude-usage-poll` marker that spaces out
the OAuth usage polls (see **Projects & Store**).

The dispatcher daemon is machine-wide for the same reason — it ticks every
registered project — so its two files sit at the store root, not under any
board's `.kanban/`: `<store>/daemon.lock` (the exclusive single-instance
`flock`) and `<store>/logs/daemon.log` (one appended line per
resume/reap/restart/dispatch). See **Headless Dispatcher Daemon**.

## Projects & Store

Each registered project splits two roles that used to be the same folder:

- **work path** — the code folder; agent cwd, `kanban project path`
- **data root** — `<store>/projects/<id>`; contains `.kanban/` and `project.yaml`

Store root resolution (empty or relative values are ignored):

1. `$KANBAN_HOME` (explicit override; required in tests so the suite never
   touches the developer's real store)
2. `$XDG_DATA_HOME/kanban4ai`
3. `$HOME/.local/share/kanban4ai`

```
<store>/
├── .lock                       # flock, serializes registry mutations
├── daemon.lock                 # exclusive lock for `kanban daemon`
├── logs/daemon.log             # one line per daemon resume/reap/restart/dispatch
├── config.yaml                 # machine-wide settings (`daemon.interval`, TUI)
└── projects/
    └── <id>/                   # data_root; slug of the folder name, deduped
        ├── project.yaml        # id, name, work_path, timestamps, migrated_from
        └── .kanban/            # same board layout as before
```

There is no central index and no pointer file in the work folder. Listing is a
scan of `projects/*/project.yaml`. The `id` is stable (`rename` changes only
`name`); collisions get `-2`, `-3`, …. A missing work folder stays listed
(struck through in the TUI); agent launches fail until `project set-path`.

**Which project a command talks to** (`cli::resolve`):

1. `--project <id|name|path>`
2. `$KANBAN_PROJECT` (exported by the agent wrapper so callbacks stay
   unambiguous after `cd`)
3. The registered project whose work path is the deepest ancestor of cwd
4. Silent adoption: `<cwd>/.kanban` exists but the folder is not registered →
   register and **move** that board into the store, then continue. Lookup is
   cwd-only (never an ancestor). On active sessions or a failed move, the
   board is left in place and one warning is printed to stderr; the next
   command retries
5. Nothing resolves: `init` / `project add` create; `tui` / bare `kanban`
   open the projects list; `daemon` ticks every registered project; every
   other command errors
   (`not inside a kanban project; run kanban init or kanban project add`)

**Migration** (`init`, `project add`, TUI add, silent adoption) is one path:
rename first, verified copy on `EXDEV`, source removed last. `--copy` leaves
the source. Active sessions refuse unless `--force`. Unregister without
`--purge` renames `project.yaml` to `project.yaml.removed`; re-adding the same
folder restores that board and id.

Agent-facing paths in the prompt are **absolute** under the data root (cwd is
the work folder, so a relative `.kanban/…` would land in the repo). The
wrapper also exports `KANBAN_DATA_DIR`. `ask-form --file` and `context --file`
try cwd first, then the data root.
