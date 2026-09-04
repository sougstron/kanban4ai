# Configuration reference

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when you are reading or adding a setting in `.kanban/config.yaml` or the store `config.yaml`.

## Configurable Thresholds (per-project .kanban/config.yaml)
- `context_embed_max_size`: 5120 (5KB) - inline vs separate file
- `context_warning`: 51200 (50KB) - warn about large context
- `context_auto_compact`: 102400 (100KB) - auto-compress
- `session_heartbeat_timeout`: 1800 (30 min) - mark crashed
- `context_summary_max_length`: 5000 chars
- `tui_refresh_interval`: 1 (sec) - TUI refresh fallback (primary refresh is inotify)
- `question_poll_interval`: 3 (sec) - poll interval for `kanban ask --wait`
- `question_wait_timeout`: 600 (sec) - default timeout for `kanban ask --wait`
- `max_auto_resumes`: 3 - cap consecutive automatic relaunches after stranded exits or expired waits
- `waiting_min_eta`: 10 (sec) - lower bound for `kanban waiting --eta`
- `waiting_max_eta`: 604800 (sec) - upper bound for `kanban waiting --eta`
- `waiting_default_eta`: 900 (sec) - default expected wait for `kanban waiting`
- `waiting_eta_multiplier`: 2 - safety multiplier applied to the ETA before relaunch
- `waiting_note_max_chars`: 1000 - maximum stored wait note length
- `agent_reply_max_chars`: 32768 - maximum length of the agent's session answer (every assistant text of the run, in order) recorded on the thread at exit (`0` disables recording it); the budget is spent from the last message backwards
- `agent_reply_message_max_chars`: 8192 - maximum length kept from any single *earlier* message of that answer, so one long mid-run message cannot eat the whole budget (`0` disables the per-message cap)
- `limits_refresh_interval`: 120 (sec) - how long a provider-limits snapshot stays fresh before the TUI refreshes it in the background

## TUI Settings (.kanban/config.yaml `tui:`)
- `card_height_lines`: 4 - task card height
- `card_line_max_symbols`: 40 - fixed one-line preview length before adding `...`
- `max_tasks_per_column`: 100 - cap rendered per column
- `name`: project name shown in Project Settings
- `theme`: theme name (quick-toggle/persist via `Ctrl+T`, or edit in Project Settings)
- `task_sort`: `task_number` (default, ascending TASK id), `task_number_desc`
  (descending TASK id — highest task number first; doubles as the
  queue-priority control for the dispatcher), `updated_at_asc` (least recently
  modified first), or `updated_at_desc` (most recently modified first).
  Legacy `completion_date` values are read as `updated_at_desc`; unknown
  values read as `task_number`.
- `show_limits`: true - draw the provider subscription-limits row above the
  status bar on the Board and Projects screens
- `hide_kanban_messages`: false - when true, the task-detail thread hides
  messages authored by kanban (audit notes). They stay on the sidecar; this
  is a display filter only. Opening a task pins the first line of the last
  visible message as high as possible without blank rows under the thread;
  a thread that already fits is left at scroll 0.

## Global Settings (<store>/config.yaml)
Machine-wide settings shared by every board, stored at the store root (the
KANBAN_HOME/XDG directory, next to `limits.json`), not in any PROJECT's
`.kanban`. Because the Projects screen has no board context, this is where
they are edited: press `s` on the Projects screen. Saved under the store
`.lock`; unknown keys survive load/save.
- `tui.escape_to_projects`: false - when true, Esc on the Board (with no
  active search filter) opens the projects list. Moved out of the per-project
  `tui.escape_to_projects` (TASK-178): the stale per-project key is now
  ignored, so boards that had it enabled need the toggle re-enabled globally.
- `tui.project_sort`: `name` (default, alphabetical by the name the list
  displays), `newest` (most recently created first), `smart` (unread work
  first — unseen Review or open questions — then rows with running agents,
  then most recently opened), or `smart_name` (the same tiers, but
  alphabetical by display name within each). Unknown values read as `name`.
  Edited from Global Settings (`s` on the Projects screen).
- `daemon.interval`: 60 - seconds between `kanban daemon` ticks. `--interval` on the command line overrides. This is the only orchestration cadence that lives in the store config, because the daemon spans projects.
- `tui.file_manager`: unset - command the Projects screen's `o folder` button
  hands the work folder to (the folder is appended as the last argument;
  the value is split like a shell word list, e.g. `nautilus --new-window`).
  Unset means the first of `xdg-open`, `gio open`, `nautilus`, `dolphin`,
  `thunar`, `nemo`, `pcmanfm`, `caja` found on PATH (`open` on macOS). Set it
  when that chain picks the wrong application. There is no dialog field for
  this key; it is edited in the file.
- `updates.check_on_open`: true - whether the TUI kicks off a background
  update check when it opens (`core::update::warm_check`; no-ops inside the
  `check_interval_hours` TTL). When a newer release is seen, the status line
  shows a one-time banner (`↑ kanban4ai X.Y.Z available - open Settings to
  update`); showing persists `dismissed_version` into
  `<store>/update-status.json`, so the same version never nags again but a
  newer tag reopens the banner. The Global Settings dialog's Updates section
  shows the status row, a `Check now` button (one deliberate blocking check),
  the checkbox for this key, and — only on unmanaged installs — an
  `Update now` button running the same apply path as `kanban update`;
  package-managed installs see the upgrade command instead of the button.
- `updates.check_interval_hours`: 24 - how long a cached update check stays
  fresh before the next on-open check runs.
- `updates.notify`: false - reserved for firing a desktop notification on a
  newly seen version; the status-line banner is the only surface for now.

## Notification Settings (.kanban/config.yaml `notifications:`)
- `enabled`: true - master switch for desktop notifications
- `questions`: true - notify when a task raises a question
- `completion`: true - notify when a task is completed or ready for review
- `chained_start`: true - notify when a chained task auto-starts
- `waiting`: true - notify when an agent declares a wait
- `command`: `notify-send` - notification command
- `timeout`: 3 - command timeout in seconds
- `max_body_chars`: 240 - truncate notification body beyond this length

## Auto-Launch Settings (.kanban/config.yaml `auto_launch:`)
Controls how delegating a task spawns a background agent job (shared across all backends):
- `enabled`: true - master switch for auto-launching
- `use_tmux`: true - host the agent in a tmux session (falls back to a direct background process if tmux is missing or `new-session` fails)
- `terminal_fallback`: true
- `auto_complete_on_exit`: false - whether agent exit auto-completes the task
- `default_agent`: opencode - backend used when a task has no `agent_backend`. Creating or saving a task with Default selected writes this (and that backend's model/effort/agent) onto the task
- `model` / `models` / `agent`: opencode back-compat mirrors of `agents.opencode.*`

## Orchestration Settings (.kanban/config.yaml `orchestration:`)

Per-project — nothing here lives in the global store config. Edited in Project
Settings (`s`) or in the file. Unlike every other section, `orchestration` is
merged with `merge_missing_deep`, so a board that sets only
`orchestration.designer.enabled` still gets all the sibling defaults; the other
sections keep their long-standing shallow `merge_missing` semantics.

```yaml
orchestration:
  queue_enabled: true
  max_running_total: 3
  max_running_per_backend: {claude: 2, codex: 2, opencode: 2, omp: 2, pi: 2}
  max_running_per_backend_model: {}
  max_running_per_role: {designer: 1, reviewer: 1, executor: 3}
  auto_restart: {enabled: true, delays_minutes: [1, 30, 270]}
  designer: {enabled: false, backend: claude, model: sonnet, effort: null, agent: null}
  reviewer: {enabled: false, backend: claude, model: sonnet, effort: null, agent: null,
             on_changes_requested: in_progress, max_rounds: 3}
```

- `queue_enabled`: true - master switch for the dispatcher. Off means nothing
  is ever queued and `dispatch_queue` returns immediately. `auto_launch.enabled`
  gates it too: with auto-launch off the dispatcher starts nothing
- `max_running_total`: 3 - concurrently running agents across the board
- `max_running_per_backend`: 2 each for claude/codex/opencode/omp/pi - cap per
  resolved backend. A key naming a backend the board does not know (a typo, or
  an agent since removed from `agents:`) caps nothing, so `Config::load`
  reports it as a **warning** rather than rejecting it — a hard error would run
  on every command and lock the user out of their own board. The warning
  surfaces on stderr for CLI commands, in the daemon log (once per project per
  daemon run), and in the TUI status line at startup
- `max_running_per_backend_model`: `{}` - cap per resolved `<backend>/<model>`
  pair. **The key must be `<backend>/<model>`, parsed on the first slash only**,
  because model ids contain slashes themselves: `opencode/openai/gpt-5.5` is
  backend `opencode`, model `openai/gpt-5.5`. A bare model id (`opus`) is
  rejected by `Config::validate` rather than silently never matching, as is an
  empty model id or an unknown backend. Model ids are only unique inside a
  backend, so this is the one format used by the config key, the census key
  (`OrchestrationSettings::backend_model_key`), the settings label
  ("Max tasks per backend/model") and these docs
- `max_running_per_role`: designer 1, reviewer 1, executor 3 - cap per role.
  Unlike the backend maps this is a closed set: a key that is not `executor`,
  `designer` or `reviewer` is rejected by `Config::validate`, since there is no
  user-extensible role to be wrong about
- `auto_restart.enabled`: true - master switch for crash auto-restart
- `auto_restart.delays_minutes`: `[1, 30, 270]` - backoff schedule; entry *n*
  is the wait before attempt *n+1*, and its length is the attempt budget.
  Entries must be positive integers
- `designer.enabled`: false - run a planning pass before execution on every task. A single task can still opt in with `use_designer` / the create-dialog Designer checkbox / `kanban create --designer`
- `designer.backend` / `model` / `effort` / `agent`: `claude` / `sonnet` /
  unset / unset - the designer bot's own launch settings, used instead of the
  task's assignment. An unset (or blank) backend falls back to
  `auto_launch.default_agent`, and a name with no `agents:` entry falls back to
  `opencode`, matching the task path; unset model/effort/agent inherit that
  backend's configured defaults
- `reviewer.enabled`: false - run a bot review before human Review on every task. A single task can still opt in with `use_reviewer` / the create-dialog Reviewer checkbox / `kanban create --reviewer`
- `reviewer.backend` / `model` / `effort` / `agent`: same defaults and same
  fallback chain as the designer
- `reviewer.on_changes_requested`: `in_progress` - where `kanban verdict
  --changes` sends the task: `in_progress` re-queues it for its own bot,
  `todo` returns it to To Do for a manual restart. Any other value is a config
  error
- `reviewer.max_rounds`: 3 - consecutive bot-review bounces before falling
  through to human Review

Every cap is an integer where **`0` means unlimited**, and so does an absent
map entry; a negative or unparseable value is a config error. `enabled` flags
are coerced like the other boolean settings (`true`/`yes`/`1`).

## Agent Backends (.kanban/config.yaml `agents:`)
Each task carries an `agent_backend` field selecting which CLI runs it. When unset, `auto_launch.default_agent` is used; an unknown backend falls back to `opencode`. Create/save with Default selected snapshots those resolved values onto the task so the detail view and stats see a concrete backend/model/effort/agent (provider is the model id's slash prefix). The `agents:` map defines one entry per backend:
- `command`: executable resolved via PATH (e.g. `opencode`, `claude`, `codex`)
- `model`: default model when a task has no `ai_model`
- `models`: list offered in the TUI create/edit dialog for this backend. For the catalog backends (opencode, omp, pi) this is only a fallback: when the backend's catalog is available the dialog lists the live catalog instead, ordered default model first, then up to three most recently launched models (`.kanban/recent_models`, newest first), then the rest alphabetically. Catalog sources: opencode → `opencode models --verbose`; omp → `omp models --json`; pi → on-disk `models-store.json` (builtin/remote cache) merged with custom providers from `models.json` and, for every provider listed in `auth.json`, the matching bundled catalog from the installed `pi-ai` package (`providers/data/<provider>.json`, e.g. OpenRouter). Agent dir is `PI_CODING_AGENT_DIR` (default `~/.pi/agent`). Catalogs are warmed in the background at TUI startup and cached per backend+command for the process lifetime
- `effort`: default reasoning effort when a task has no `ai_effort`
- `efforts` (claude, codex, omp, pi): effort levels offered in the TUI dialog as a fallback (defaults `low`/`medium`/`high`/`xhigh`/`max`, matching the backend's supported reasoning levels; omp/pi also expose `off`). For opencode/omp/pi the dialog instead offers the selected model's variants reported by the live catalog when available (opencode exposes them as `variants`, omp as each model's `thinking` list, pi as each model's `thinkingLevelMap` keys)
- `agent`: optional default `--agent` persona (overridden per task by `task.agent_name`; opencode only)
- `agent_options` (opencode only): personas offered in the TUI and via `kanban create --agent-name` (e.g. `sisyphus`, `prometheus`, `atlas`). omp/pi have no launch-time persona selector, so they expose no personas
- `extra_args`: extra CLI flags inserted before `--model`

Per-task persona: `task.agent_name` is passed to opencode as `--agent`, overriding the backend default. opencode matches `--agent` against an agent's *exact* registered name (oh-my-openagent personas are decorated strings), so the friendly key is resolved via `opencode agent list`. Because starting the opencode CLI takes seconds, resolution is deferred into the launch wrapper script: the spawned session calls the hidden `kanban resolve-agent` command and substitutes the result into `--agent`, so the launching process (TUI or CLI) never blocks on it. If opencode is unavailable or lists no match the key is passed through unchanged. The claude backend ignores `agent_name`.

Built-in backends:
- **opencode**: `opencode run --title "<id>: <title>" [extra_args] [--model M] [--variant E] [--agent A]` plus the prompt file as the last argument. A task's `ai_effort` (or the backend `effort` default) is passed as `--variant`, opencode's per-model reasoning-effort selector.
- **claude** (Claude Code): `claude --print [extra_args] [--model M] [--effort E]` plus the prompt file as the last argument. Default `extra_args` is `["--dangerously-skip-permissions"]` — tighten in config for stricter permissions. Default models are the `fable`/`opus`/`sonnet`/`haiku` aliases; `ai_effort` is passed as `--effort` (`low`/`medium`/`high`/`xhigh`/`max`).
- **codex**: `<command> exec --json [extra_args] [--model M] [-c model_reasoning_effort=E]` plus the prompt file as the last argument. `--json` emits Codex's JSONL event stream, which is captured for telemetry, replies, and input provenance. `ai_effort` is passed through Codex's `model_reasoning_effort` config override (`low`/`medium`/`high`/`xhigh`). The default extra args also include `--dangerously-bypass-approvals-and-sandbox` and `--skip-git-repo-check`, matching autonomous launches and kanban's support for non-git folders. Codex has no launch-time persona flag, so `agent_name` is ignored. Codex stdin is closed by the wrapper because `exec` may read additional prompt input from a pane TTY.
- **omp** / **pi** (the "pi" agent family): `<command> -p --mode json [extra_args] [--model M] [--thinking E]` plus the prompt file as the last argument. Run non-interactively with `-p`; `ai_effort` is passed as `--thinking` (`off`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`). Model uses fuzzy `provider/id` selectors from the live catalog. Neither has a launch-time persona flag, so `agent_name` is ignored. `--mode json` makes them emit the same NDJSON event stream on stdout as their session files, so their runs are harvested for telemetry and input provenance exactly like claude/opencode. Both probe stdin even under `-p` and hang forever on an inherited pane TTY, so the wrapper closes their stdin (`< /dev/null`).
