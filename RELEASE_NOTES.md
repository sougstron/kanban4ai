# kanban4ai 0.6.5

The board starts work on a provider that still has quota, and can wait until
a clock time: limit-aware executor pools pick a live candidate before launch
instead of dying on a 429, a planned `launch_at` enqueues a To Do card when
that local time comes due, and Project Settings split into four tabs so the
executor roster is its own page.

## Added

- **Limit-aware executor pools** (`core/executors.rs`, `core/operations.rs`,
  `core/scheduler.rs`, `core/limits.rs`, `core/config.rs`, `core/daemon.rs`,
  `docs/config.md`, `docs/orchestration.md`, `docs/limits.md`;
  `tests/config_test.rs`, `tests/operations_test.rs`). New
  `orchestration.executors` holds two ordered candidate lists — `cheap` (the
  executor default for tasks whose launch settings still match the board
  defaults) and `middle` (opt-in via `role_profile: middle`) — each at most
  three entries, in either `roles` spelling. Before a launch the dispatcher
  walks the pool against the *cached* limits snapshot only: the first
  candidate whose provider is at or above the floors
  (`thresholds.week_percent` 5, `thresholds.five_hour_percent` 15, inclusive)
  is materialized onto the task the same way `advance_role_roster` is. An
  explicit per-task assignment always wins. When every candidate is blocked
  the task parks at the earliest reset plus `ask_grace_secs` (no crash-
  restart cost, no crash notification) and the board posts one
  `kanban:executor-pool` question whose variants are the other providers that
  still pass; an answer or the deadline, whichever comes first, runs the
  task. The daemon tick now warms the limits cache so a headless pump has
  numbers to read. The post-mortem 429 ladder is unchanged.
- **Planned launches** (`core/models.rs`, `core/scheduler.rs`, `core/timefmt.rs`,
  `core/storage.rs`, `cli/mod.rs`, `tui/dialogs.rs`, `docs/orchestration.md`,
  `docs/cli.md`, `docs/tui.md`; `tests/operations_test.rs`, `tests/cli_test.rs`,
  `tests/storage_test.rs`). A To Do task may carry `launch_at` — the next
  local occurrence of an HH:MM — set from the task form's "Planned launch"
  checkbox or `kanban create --launch-at HH:MM`. Every pump, before the queue
  walk, `due_launches()` enqueues schedules that have come due through the
  normal `queue_run` path (same caps, claiming, crash-restart) and consumes
  the field, so a task launches at most once per schedule. With the queue off
  the scan is a no-op and the time stays on the task. The card shows
  `🕐 HH:MM` while pending; `kanban check-sessions` reports the step.

## Changed

- **Tabbed Project Settings** (`tui/dialogs.rs`, `tui/app.rs`, `docs/tui.md`).
  `s` on Board/Detail is four tabs — Common, Designer, Reviewer, Executor —
  with Left/Right switching (wrapping; skipped inside text inputs and
  filtered selectors), clickable labels, and one Save for the whole dialog.
  A validation error flips to the tab that owns the field. The Executor tab
  holds the six ordered pool slots (filterable `backend/model` selectors
  annotated with live quota numbers and `(out of quota)`), the resolved
  `next:` line, and the two quota floors.
- **Answer panel and form sizing** (`tui/detail.rs`, `tui/dialogs.rs`,
  `tui/app.rs`). Left/Right in the detail answer panel edit the custom
  answer; question switching is explicit previous/next buttons under the
  variants. Multiline textareas jump to line start/end at wrap boundaries.
  The task-form description cap is 15 lines and the chain selector is pinned
  to 4..=8 rows so a long chain list cannot crowd out the description.
- **Role-colored live cards** (`tui/card.rs`, `docs/tui.md`). A live design
  or review session gets a bold `▶ running` row under the badges — blue for
  designer, purple for reviewer — and the token/cost stats line uses the
  same color; executor cards keep the green badge-only look.
- **Text-pager scroll speedups** (`tui/app.rs`, `tui/board.rs`, `docs/tui.md`).
  Shift+Up/Down moves the stats report and every other text pager by 3 rows,
  Ctrl+Up/Down by 10; plain arrows stay at 1.

## Verification coverage

- executor-pool parse, size cap, unknown-backend warning, threshold bounds
  (`tests/config_test.rs`: `executor_pools_parse_both_candidate_spellings`,
  `executor_pools_reject_more_than_three_entries`,
  `executor_pools_warn_on_unknown_backend`,
  `executor_pool_thresholds_must_be_percentages`)
- pool walk materializes the next candidate, never overrides an explicit
  assignment, parks with a question and no crash cost, and lets answer or
  deadline win (`tests/operations_test.rs`:
  `executor_pool_skips_blocked_candidate_and_materializes_next`,
  `executor_pool_never_overrides_an_explicit_assignment`,
  `executor_pool_parks_blocked_task_with_question_and_no_crash_cost`,
  `executor_pool_answer_materializes_provider_and_clears_the_park`,
  `executor_pool_deadline_wins_and_withdraws_the_question`)
- planned launch queues a due To Do, leaves the future alone, stays inert
  when the queue cannot dispatch, and round-trips off the frontmatter while
  unset (`tests/operations_test.rs`:
  `due_launches_queues_a_due_todo_task_and_consumes_the_schedule`;
  `tests/storage_test.rs`:
  `launch_at_round_trips_and_stays_off_the_frontmatter_while_unset`)
- inclusive quota floors (`src/core/limits.rs`:
  `has_headroom_boundaries_are_inclusive`)
- full `cargo test --locked`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build --release --locked`, `sh scripts/test-packaging.sh`,
  `sh scripts/token-budget.sh`

# kanban4ai 0.6.4

The board learns to plan in graphs: a task can wait on its dependencies (an
AND-join over Review-or-Done), an orchestrator mode decomposes one task into a
planned DAG of subtasks, nodes fail over across a named model roster when a
provider caps out, and a crashed run raises a desktop alarm instead of failing
silently.

## Added

- **Task dependencies — the DAG** (`core/graph.rs`, `core/operations.rs`,
  `core/scheduler.rs`, `core/models.rs`, `core/storage.rs`,
  `docs/orchestration.md`, `docs/cli.md`; `tests/graph_test.rs`).
  `kanban depends TASK-310 [--on TASK-NNN ...] [--clear]` and the repeatable
  `kanban create --depends-on TASK-NNN` maintain `Task.depends_on`. A
  dependency is an AND-join: the node cannot start until every listed task has
  reached Review or Done. The edge is pulled, not pushed —
  `dispatch_ready_dependents()` runs in every daemon / TUI / `check-sessions`
  pump before the queue dispatch and hands ready To Do tasks to the normal
  cap-checked queue, so a wide fan-out cannot bypass the concurrency caps. A
  dependency whose task no longer exists counts as satisfied and is reported
  in the thread note (`missing: TASK-nnn`). Cycles are refused at write time
  by a DFS over the whole board plus the proposed edges, naming the path —
  acyclicity is the termination guarantee. The edge also carries context: the
  node's prompt opens with an *Upstream results* section built from each
  dependency's recorded context and harvested final reply, compacted by the
  existing rule-based compaction and capped by
  `orchestration.orchestrator.upstream_budget_chars` (split across the
  dependencies), with the task's `needs` contract printed above it. Legacy
  boards load untouched — the new task fields default and the golden fixtures
  still round-trip (`tests/golden_compat.rs`).
- **Orchestrator mode** (`agent/prompt.rs`, `core/operations.rs`,
  `docs/orchestration.md`). A per-task opt-in (`use_orchestrator` — the
  Orchestrator checkbox in the task form, `kanban create --orchestrator`);
  there is deliberately no board-wide switch. The task's first run enters a
  new `orchestrate` phase before any design pass, with a role-scoped
  orchestrator prompt carrying the DAG rules, the plan schema, the configured
  model rosters and the `max_subtasks` cap. The plan is submitted with
  `kanban plan <task> --file <plan.yaml>` and validated whole — unknown
  references, duplicate or colliding keys, cycles, unknown role profiles,
  size — before anything is created. Accepted, each node becomes a To Do task
  with its `depends_on` wired, its `needs` contract, its role profile and
  `parent_task`; the planner itself becomes the join node (`orchestrated:
  true`, `depends_on` every node it created), returns to To Do, and the
  graph's root nodes are queued immediately. Finishing without an accepted
  plan is refused; moving an orchestrated task back to To Do by hand drops the
  join so the next run plans again (subtask edges are never cleared this way).
  New config: `orchestration.orchestrator {max_subtasks: 12,
  upstream_budget_chars: 4000}`, and `orchestration.max_running_per_role`
  gains `orchestrator: 1`.
- **Role model rosters** (`orchestration.roles`, `docs/config.md`). Named,
  ordered backend/model candidates — plain `claude/haiku` strings or
  `{backend, model, effort}` maps — the orchestrator may assign to a node with
  `role:`. A node starts on the first candidate, materialized onto its own
  backend/model/effort/agent fields; when a run dies on a provider limit,
  `advance_role_roster` moves it to the next candidate and re-queues it
  immediately instead of parking it until the quota window rolls over. A
  failover spends no crash-restart step and `roster_index` is never reset
  automatically; with no candidate left, the normal crash-restart backoff
  takes over.
- **Role-scoped instructions** (`agent/prompt.rs`, `docs/orchestration.md`).
  `<board>/.kanban/instructions/<role>.md` (`orchestrator`, `designer`,
  `reviewer`, `executor`) is appended to that role's prompt only, when that
  role is actually launched — the opposite of AGENTS.md/CLAUDE.md, which every
  session pays for. Missing or empty files are skipped.
- **Crash notifications** (`core/notifier.rs`, `core/scheduler.rs`,
  `core/daemon.rs`, `docs/config.md`; `tests/scheduler_test.rs`). New
  `notifications.crash` (default true, urgency critical): every failure on the
  auto-restart path — non-zero exit, heartbeat timeout, failed launch — fires
  a desktop alert that names the scheduled retry and attempt, or says plainly
  that no automatic retry is configured, so a crashed task is never silent. A
  spent schedule keeps the stronger stranded notification.

## Changed

- **TUI** (`tui/card.rs`, `tui/detail.rs`, `tui/dialogs.rs`, `tui/app.rs`).
  To Do cards wearing `depends_on` show a graph-node badge, the detail view
  renders `Depends: …`, `Orchestrator: on` / `Orchestrator: planned` and the
  `needs` contract, the task form gains the Orchestrator checkbox, and the new
  phase shows as `◧ plan`.
- **Docs**: `docs/orchestration.md` documents the `orchestrate` phase, Task
  Dependencies, Orchestrator Mode and role rosters; `docs/cli.md`,
  `docs/config.md`, `docs/data-model.md` follow; `docs/research/dag-orchestration.md`
  records the design investigation.
- **Housekeeping**: the `update-app` maintainer skill lands under
  `.agents/skills/` with `docs/releasing.md` aligned, AGENTS.md/CLAUDE.md are
  trimmed, and the tracked AUR packaging metadata catches up to the published
  0.6.3-1.

# kanban4ai 0.6.3

Codex CLI joins the board as a first-class agent backend — non-interactive
launch, reply capture, token telemetry, and native conversation resume — and
the board gets sharper about what it remembers: usage stats split by model
provider, tasks saved as "Default" pin the launch settings they resolved to,
and the TUI keeps every registered board moving, not just the one on screen.

## Added

- **Codex CLI agent backend** (`agent/backends.rs`, `agent/prompt.rs`,
  `agent/tmux.rs`, `core/provenance.rs`, `core/reply.rs`, `core/telemetry.rs`,
  `core/operations.rs`, `docs/agent-io.md`, `docs/config.md`). Codex runs
  non-interactively as `codex exec --json` with the prompt file as the last
  argument, `-c model_reasoning_effort=E` carrying the task's reasoning
  effort and `--model` its model. Replies are captured from completed
  `agent_message` items (`item.completed` events; streamed `item.updated`
  partials are skipped), and token telemetry reads the cumulative
  `input_tokens`/`output_tokens`/`total_tokens` of `turn.completed`, with
  completed command executions providing last activity. Provenance
  harvesting records the Codex thread id, so automatic relaunches reopen the
  native conversation with `codex exec resume <thread-id> --json` instead of
  re-briefing a fresh one. `auto_launch.max_running_per_backend` gains a
  `codex` slot, the launch wrapper closes stdin for it (Codex may read extra
  prompt text from stdin even with a positional prompt), and the limits row,
  CLI docs, and README name the backend everywhere.
- **Provider breakdown in usage stats** (`core/stats.rs`, `docs/stats.md`).
  The Tokens, Time, and Tasks sections aggregate by provider next to
  backend, model, and project. Providers are derived at report time from the
  model id — the segment before its first slash (`openai/gpt-5.5` →
  `openai`, `zai/glm-4.7` → `zai`); a bare model id has no provider and
  lands in `unknown`. Nothing new is stored in the events file — existing
  logs report providers without re-recording.

## Changed

- **Saving "Default" pins the resolved launch settings** (`agent/mod.rs`,
  `core/operations.rs`, `docs/config.md`, `docs/tui.md`). Creating or saving
  a task with Default backend/model/effort/agent selected snapshots the
  board's current defaults onto the task — including the selected backend's
  configured model, effort, and agent — so a later change to the board
  defaults never silently rewrites an existing task's launch.
- **The TUI pumps every registered board** (`tui/app.rs`,
  `docs/orchestration.md`). `App::tick` now drives the daemon's store-wide
  tick — queue dispatch, crash-restart deadlines — across all registered
  projects from any screen, not only the board in view; an unregistered
  in-place board keeps the local throttled dispatch fallback. With no TUI
  open, the `kanban daemon` looping form remains the headless clock.

## Verification coverage

- codex launch plan, reasoning-effort wiring, and native-thread relaunch
  (`tests/agent_test.rs`: `codex_launch_plan_uses_exec_json_and_reasoning_effort`,
  `codex_auto_relaunch_resumes_native_thread`)
- default snapshot on create and update, including the selected backend's
  configured model/effort/agent (`tests/operations_test.rs`:
  `create_and_update_snapshot_default_launch_settings`,
  `default_snapshot_uses_the_selected_backend_config`,
  `default_snapshot_picks_up_configured_effort_and_agent`)
- store-wide tick advances retry deadlines on registered boards from the
  Projects screen (`src/tui/tests.rs`:
  `projects_screen_ticks_retry_deadlines_on_registered_boards`)
- provider derivation from model ids (`src/core/stats.rs`:
  `model_provider_is_the_first_slash_segment`)
- full `cargo test --locked`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build --release --locked`, `sh scripts/test-packaging.sh`,
  `sh scripts/token-budget.sh`

# kanban4ai 0.6.2

The board starts counting what the agents cost — tokens and time — and the
Projects list gets sharper about what its rows mean: paused work is visible,
the smart ordering follows what you see, and the status bar stops pretending
to be buttons.

## Added

- **Usage statistics** (`core/stats.rs`, `core/session.rs`, `core/scheduler.rs`,
  `core/operations.rs`, `cli/mod.rs`, `tui/projects.rs`, new `docs/stats.md`).
  The board appends one small JSON line per project to
  `.kanban/stats/events.jsonl` at state transitions it already drives — a
  session starting or closing (which also tallies the session's final tokens
  from its transcript), a declared wait, a queue entry, a crash-restart
  backoff. No agent ever writes to it. The report — `kanban stats`, or `S` on
  the Projects screen — shows tokens and running time by backend, model, and
  project for all time, this month, and this week. The grand running-time
  total is a wall-clock union of concurrent spans, so two agents running at
  once don't double the clock; the per-backend/model/project breakdowns are
  honest plain sums. An all-time Tasks section adds task counts and per-task
  averages by backend/model.
- **`smart_name` project sort** (`core/global.rs`, `tui/projects.rs`,
  `docs/config.md`). The smart tiers — unread work first, then projects with
  live agents — now support alphabetical order by display name within each
  tier. `smart` itself changed slightly: its final tier follows most recently
  opened (the visible "Last opened" column) instead of creation date.
- **Paused-agent indicator** (`tui/projects.rs`, `docs/tui.md`). The Projects
  list's Agents column shows `⏸N` for tasks that hold no running slot —
  queued, retrying after a crash, or parked in a declared wait — while `▶N`
  counts only agents actually running.

## Changed

- **The status bar is no longer clickable** (`tui/board.rs`, `tui/app.rs`,
  `docs/tui.md`). It is an informational hotkey panel: nothing in it reads as
  a button, and it registers no hitboxes. Global Settings remains on `s`.

## Verification coverage

- stats: a closed running span tagged with the backend, a closed queued span
  across dispatch, a waiting span that closes the running one, and a closed
  retry span after crash backoff (`tests/scheduler_test.rs`:
  `dispatch_and_stop_record_a_closed_running_span_tagged_with_the_backend`,
  `queue_run_then_dispatch_records_a_closed_queued_span`,
  `declare_waiting_then_stop_records_a_waiting_span_and_closes_the_running_one`,
  `crash_restart_backoff_records_a_closed_retry_span`)
- status bar renders as plain info with no hitboxes
  (`src/tui/tests.rs`: `wide_board_status_bar_is_not_clickable`,
  `phase_seven_status_bar_is_contextual_and_not_clickable`)
- Projects rows show paused agents beside running ones, and both smart
  orderings sort their tiers correctly (`src/tui/tests.rs`:
  `project_row_shows_paused_tasks_next_to_running_agents`,
  `projects_screen_smart_sort_orders_tiers_by_last_opened`,
  `projects_screen_smart_name_sort_orders_tiers_by_display_name`)
- `smart_name` survives a config round-trip (`src/core/global.rs` tests)
- full `cargo test --locked`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build --release --locked`, `sh scripts/test-packaging.sh`,
  `sh scripts/token-budget.sh`

# kanban4ai 0.6.1

Relaunches stop paying twice: automatic relaunches of pi/omp agents now reopen
the backend's own conversation instead of re-briefing a fresh one, and the
fixed prompt text that every delegated session carries got smaller.

## Added

- **Native conversation resume for pi/omp relaunches** (`agent/backends.rs`,
  `agent/prompt.rs`, `core/operations.rs`). Crash restarts, queue resumes, and
  wake-after-answer relaunches look up the task's most recent completed
  session with a recorded backend conversation id (provenance manifest, same
  backend) and reopen it — pi via `--session <id>`, omp via `--resume <id>`
  — instead of starting a fresh conversation. The relaunch sends a small
  follow-up prompt (`build_resume_prompt`) that carries only the new board
  session identity, the finish command for the role, and the thread delta
  since the previous session; the backend already holds the original task,
  rules, and tool history. Provenance is harvested before exit
  reconciliation, so the just-finished conversation id is available when the
  successor launch is built. Human-started resets are never resumed.
- **`kanban ask-form --help` documents the schema**
  (`cli/mod.rs`). The strict YAML form schema moved into the command's help
  text, so agents are pointed at it instead of carrying the example in every
  prompt.

## Changed

- **Slimmer delegated-agent prompts** (`agent/prompt.rs`). The detach/waiting
  guidance and the ask-form example were condensed to a couple of lines that
  reference `--help`; every session prompt (executor, designer, reviewer)
  shrinks accordingly.
- **No double replay of a run's reply** (`agent/prompt.rs`). When a captured
  whole-session reply repeats context that the same run already posted
  explicitly, the reply is skipped and only the concise context record is
  replayed into the next prompt.

## Docs and tooling

- **`AGENTS.md` cut from ~1.6k lines to a project shape**
  (`AGENTS.md`, `docs/*`, `scripts/token-budget.sh`). Long-form reference
  moved into `docs/` (data model, CLI, config, orchestration, worktrees, TUI,
  limits, agent I/O, releasing, token profile), loaded on demand; the
  auto-loaded file keeps only architecture, rules, and a doc map.
  `scripts/token-budget.sh` keeps the auto-loaded files small.
- **Token profiler** (`scripts/profile-tokens.py`,
  `docs/token-profile.md`). Measures what the board and `AGENTS.md` actually
  cost an agent from the run history in `.kanban/logs/`, per stage and
  cross-project.

## Verification coverage

- relaunch reopens the native conversation with the delta prompt
  (`tests/agent_test.rs`: `pi_family_auto_relaunch_resumes_native_conversation_with_delta_prompt`)
- captured reply not replayed when the run posted explicit context
  (`tests/agent_test.rs`: `full_prompt_does_not_replay_agent_reply_when_same_run_posted_context`)
- full `cargo test --locked`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build --release --locked`, `sh scripts/test-packaging.sh`,
  `sh scripts/token-budget.sh`

# kanban4ai 0.6.0

Three board-workflow features: duplicate a task with a keystroke, reach
backend/model/effort/persona settings through one nested popup, and a tighter
default persona list.

## Added

- **Copy task** (`core/operations.rs`, `tui/app.rs`). `Ctrl+C` on a selected
  board or detail task duplicates it under a fresh ID in the same column —
  title, description, assignment, and the sidecar thread come along; run
  bookkeeping does not (`session`, run phase, restart counters, and
  worktree/branch/integration state are cleared, so a copy never inherits a
  live agent or an isolated checkout). `Ctrl+C` twice within 3 seconds still
  quits when no task is selected, and the help overlay says which of the two
  the key does.

## Changed

- **Nested `Agent settings` popup** (`tui/dialogs.rs`). The flat
  Backend/Model/Effort/Agent rows in the task create/edit dialogs and Project
  Settings became one launcher row that opens a nested popup (per slot: the
  task's own executor settings, the designer bot, the reviewer bot). The popup
  opens on Enter or a click on the row, saves or restores the exact opening
  state, and the parent form keeps Designer, Reviewer, and Chain-to controls.
  The legacy `Interactive` checkbox left the TUI dialogs — TUI-created tasks
  are non-interactive and edits leave an existing value untouched; the CLI
  `--interactive` option and the stored YAML field remain supported.
- **Agent prompts ask sooner** (`agent/prompt.rs`). Delegated agents are now
  told to ask whenever clarification is needed instead of only "if blocked",
  and an `interactive` task's prompt states the `kanban ask --wait` guidance
  explicitly.

## Removed

- **`hephaestus` persona** (`core/config.rs`): dropped from the default
  opencode `agent_options`.

## Verification coverage

- `copy_task` round-trip, run-state stripping, and thread copy
  (`tests/operations_test.rs`)
- agent-popup open/save/cancel, launcher click, snapshot updates for the
  create/edit/settings dialogs (`src/tui/tests.rs` and snapshots)
- interactive prompt paragraph in `tests/agent_test.rs`
- full `cargo test --locked` and `cargo clippy --all-targets -- -D warnings`

# kanban4ai 0.5.9

opencode's `openai/*` models spend the same ChatGPT subscription as the codex
CLI, but the board had no visibility into that quota and no way to react to it
being spent. This release makes codex a visible provider again and teaches
crash-restart to wait for the quota window instead of retrying blind.

## Added

- **codex is a visible provider again** (`core/limits.rs`). It reports the
  OpenAI subscription behind both the codex CLI and opencode's `openai/*`
  models, since they spend the same quota. Live numbers come from the codex
  app-server JSON-RPC (`account/rateLimits/read`), polled on its own 300s
  interval since the exchange costs no usage; the newest `rollout-*.jsonl`
  under `~/.codex/sessions/` is the offline fallback. A run that 429s with a
  `usage_limit_reached` error hands its `x-codex-*` response headers to the
  cache directly, so a machine that only ever drives OpenAI through opencode
  still gets numbers on the row.

## Changed

- **Crash-restart waits for a spent quota instead of retrying blind**
  (`core/operations.rs`, `core/scheduler.rs`). A crash whose transcript names
  a `retry_at` — decoded from the 429 body, codex reset headers, or
  `retry-after` — is rescheduled for that moment (floored a minute out, capped
  at 24h) instead of stepping through the usual backoff ladder, so an openai
  task no longer relaunches every minute into the same exhausted window.

## Verification coverage

- `codex_usage_headers_read_both_windows` and related `parse_codex_usage_headers`
  cases (both windows, string-typed header values, absolute vs. relative reset)
- `crash_restart_plan` returning `Backoff`/`Skip`/`After` for the corresponding
  transcript error shapes
- `schedule_crash_restart_at` honoring a named retry time, floor, and 24h cap

# kanban4ai 0.5.8

The yolo subscription is back on the limits row, a captured agent reply keeps
its conclusion instead of its opening, and the thread no longer pre-highlights
its last message.

## Added

- **yolo returns to the limits row** (`core/limits.rs`). The endpoint publishes
  counters but no quota — `limits.requests` and `remaining.requests` are `null`
  on the current plans — so the ceiling comes from the plan itself: the
  40,000,000-token rolling day of Standard pressure (`YOLO_DAILY_TOKEN_LIMIT`).
  Only that single `24h` window is drawn; spend is the larger of the key's own
  rolling-24h total and the project's UTC-day total, each a lower bound on the
  real rolling day, so the row never promises capacity that is already gone.
  The window is `rolling` with no reset countdown, and the key never expires,
  so the segment needs no click refresh.

## Changed

- **Agent replies keep their ending.** The thread budget for a run's captured
  answer is now spent from the *last* message backwards instead of clamping the
  head: a long run loses its opening planning chatter rather than the answer it
  finished on. Earlier messages are additionally capped by the new
  `agent_reply_message_max_chars` threshold (default 8192) so one mid-run wall
  of text cannot crowd out the rest, cuts land on a line boundary where there is
  one, and both truncation markers name `.kanban/logs/<session>.log` where the
  full text still lives.
- **The thread no longer pre-highlights its last message.** Opening a task still
  pins the first line of the last visible message as high as possible without
  blank rows under the thread, but that message is no longer painted in the
  selection colour, so the conversation reads as history instead of a fresh
  selection.

## Verification coverage

- `parse_yolo_usage`, `find_yolo_key`, `is_yolo_provider` (yolo limits row)
- `budget_is_spent_from_the_last_message_backwards`,
  `agent_reply_budget_keeps_the_last_message_and_drops_early_chatter`
- TUI detail test asserting `thread_selected.is_none()` on open
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --release --locked`, and
  `sh scripts/test-packaging.sh`

---

# kanban4ai 0.5.7

A quieter status line and a cleaner thread: codex/yolo leave the limits row,
kanban's own audit notes can be hidden, and two `.kanban`-directory bugs are
fixed.

## Added

- **Hide kanban's own thread messages** (`tui.hide_kanban_messages`, default
  `false`). A Project Settings checkbox (and the config key) filters the
  task-detail thread's kanban-authored audit notes from the display — they
  stay on the sidecar thread, so nothing is deleted. Opening a task now pins
  the first line of the last visible message as high as possible without
  blank rows under the thread; a thread that already fits stays at scroll 0.

## Changed

- **codex and yolo are gone from the limits row** and from `kanban limits`.
  The codex subscription is paused, so its readers and app-server RPC client
  stay compiled and tested but it is neither fetched nor displayed; yolo is
  removed from the board's provider set entirely. Remaining providers:
  claude, grok, zai, synthetic.

## Fixed

- **Agent launches could litter the work folder with `.kanban/logs`.** The
  tmux wrapper's log directory fell back to a relative `.kanban/logs` path,
  so the `mkdir -p` in the wrapper ran after `cd` into the work folder.
  The wrapper now always creates the absolute log directory under the data
  root.
- **Read-only kanban commands created `.kanban/threads`.** `ThreadManager`
  created the threads directory as a constructor side effect, so merely
  resolving or reading a project could create board directories (and, for
  operations on other folders, a local `.kanban`). The directory is now
  created only when a thread is actually saved.

## Verification coverage

- `hide_kanban_messages_filters_thread_but_keeps_sidecar`
- `settings_save_persists_hide_kanban_messages`
- `opening_long_thread_pins_last_message_without_blank_tail`
- `opening_short_thread_does_not_scroll`
- `opening_tall_last_message_puts_header_at_top`
- `opening_with_filter_pins_last_visible_message`
- `new_does_not_create_kanban_directories`
- `create_context_and_heartbeat_do_not_create_local_kanban`
- `for_project_ops_do_not_create_a_local_kanban`
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --release --locked`, and
  `sh scripts/test-packaging.sh`

---

# kanban4ai 0.5.6

Unread Review work is yellow again; reading a card clears it.

## Fixed

- **Review cards were always yellow after 0.5.5.** Completed-by-agent work
  should only highlight while it is unread. A new Review task
  (`review_unseen`) gets a yellow border, a `●` on the card, and a `●` on
  the projects list. Opening the card is the human-read signal: both the
  yellow and the projects notifier go out. Focus and hover on an unread
  card stay yellow so selection does not hide the notifier.

## Verification coverage

- `unseen_review_cards_use_the_yellow_notifier_border`
- `seen_review_cards_drop_the_yellow_notifier_border`
- `focused_unseen_review_card_stays_yellow`
- `opening_review_detail_clears_the_unseen_notifier`
- `appending_agent_reply_context_keeps_review_unseen`
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --release --locked`, and
  `sh scripts/test-packaging.sh`
