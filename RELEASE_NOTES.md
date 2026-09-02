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
