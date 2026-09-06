# Run phases, scheduling and the daemon

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when you are touching run phases, the queue dispatcher, the headless daemon, crash restart or chaining.

## Run Phases (In Progress sub-states)

The board columns are unchanged. What is new is a sub-state on In Progress:
`Task.run_phase`, one of `queued`, `orchestrate`, `design`, `execute`,
`review`. `None` means
"In Progress the old way" and reads as `execute` everywhere a phase is needed,
so legacy boards keep working untouched.

```
To Do            manual start only, or a graph node whose dependencies are all done
In Progress      queued → [orchestrate] → [design] → execute → [bot review]
Review           human review
Done             human only (unchanged)
```

| phase | badge | meaning |
|---|---|---|
| `queued` | `⏸ queued` | waiting for a free agent slot; the dispatcher starts it. No session runs, so a queued task occupies no slot. A paused task whose wait deadline passed or that was revoked while paused is parked here too |
| `orchestrate` | `◧ plan` | the orchestrator bot is decomposing the task into a subtask graph (only when the task has `use_orchestrator`) |
| `design` | `✎ design` | the designer bot is planning (only when `orchestration.designer.enabled`) |
| `execute` | `▶ running` | the task's own assigned bot is doing the work |
| `review` | `⚖ review` | the reviewer bot is checking the result; the task is still In Progress |

The badge still derives live/waiting/crashed from the session record; the phase
only overrides the *label*, and a pending crash-restart shows `↻ retry HH:MM`
from `restart_at` instead. Worktree isolation adds two badges of its own:
`⑂ worktree` while the task holds an isolated checkout, and `⚠ conflict`
(error color) when `integration: conflict` — the one blocking state, so it
leads the card and displaces the plain worktree badge it implies.

**To Do stays manual-start-only.** The dispatcher never pulls from To Do. A
task reaches In Progress only through an explicit start (`r` Run, `Q` queue, a
move, `take --agent`) or the chaining rule. **Done stays human-only** — rule 2
is unchanged and every role prompt repeats it.

How a task enters each phase:

- **`r` Run** (`queue_run` + one immediate `dispatch_queue` pump) — the normal
  human entry into the queue: the task lands In Progress with phase `queued`
  and the pump starts it on the spot when a slot is free; a full board parks it
  with the `⏸ queued` badge. When the queue could never drain
  (`queue_enabled: false` or auto-launch off) `r` falls back to the old direct
  `start_task` (`take_task_inner(immediate: true)`), which always launches,
  bypassing the queue and clearing any queued marker — the same path the `F`
  run-now hotkey uses. The one thing neither bypasses is an enabled designer:
  the planning pass still runs first (phase `design`). `kanban rerun` (no
  `--now`) and the TUI `Ctrl+R` re-run go through the same queue entry, folding
  the review edits first; `--now` / a disabled queue launch directly.
- **`take --agent`** — queue-aware. When `auto_launch_on_delegate` would launch
  but every applicable cap is full (`queue_is_full`), the task lands In Progress
  with phase `queued` instead of launching, and the dispatcher starts it later.
- **`Q`** — explicit enqueue. A To Do task moves to In Progress, an idle In
  Progress task stays put; either way the phase becomes `queued`, the
  human-restart counters reset, and nothing launches. `Q` on an already-queued
  task unqueues it (phase back to `None`, run it manually). A task with a live
  session cannot be queued.
- **`orchestrate` → the graph** — the orchestrator's `kanban done` closes the
  planning session, and the task leaves In Progress: it goes back to **To Do**
  as the join node of the graph it planned, carrying `depends_on` for every
  subtask. The graph's root nodes are queued immediately; the join node is
  re-queued by the readiness sweep once every subtask has reached Review or
  Done, and runs then as an ordinary executor (the integration pass). Finishing
  without an accepted plan is refused. See **Orchestrator mode**.
- **`design` → `execute`** — the designer's `kanban done` closes the design
  session, sets `designed: true`, flips the phase to `execute`, and launches the task's own bot
  directly on the same slot (re-queueing would stall a task whose slot is
  already paid for). The plan reaches the executor through the thread.
- **`execute` → `review`** — when `orchestration.reviewer.enabled` or this task has `use_reviewer`, the point
  where `auto_move_on_complete` would move the task to Review instead keeps it
  In Progress, sets phase `review`, increments `review_rounds`, and launches the
  reviewer bot with the reviewer's own backend/model.
- **`review` → out** — only `kanban verdict`. `--approve` clears the phase and
  moves to human Review. `--changes` folds the text into the thread and then
  routes on `reviewer.on_changes_requested`: `todo` returns the task to To Do
  with a cleared phase (manual restart), `in_progress` sets phase `queued` so
  the dispatcher restarts **the task's own bot**, never the reviewer, with the
  edits already in the thread. Once `review_rounds` reaches
  `reviewer.max_rounds` the bounce falls through to human Review and notifies.

Every phase change posts a one-line audit note on the task thread
(`▶ dispatcher started session …`, `↻ crash restart scheduled at …`,
`⚖ reviewer approved — handing to human Review`, …).

Whether a run starts at `design` or `execute` is decided by the task's
`designed` flag, never by a counter. A crash restart, a `Q` re-queue or a
review bounce of a task that already has its plan resumes the **executor**;
only a task with no plan yet goes to the designer, and only when the designer is on for the project or this task (`use_designer`). `designed` is cleared when a
human sends the task back to To Do (`recover`, or a human move whose target is
To Do) — that is a fresh attempt, so the next run plans again. A reviewer
bounce routed by `on_changes_requested: todo` does **not** clear it: the plan
is still the plan, and the requested edits are already on the thread.

## Integration Model
Agents call kanban via shell commands. NOT a plugin. An agent must:
1. Use the session the launcher exported (`KANBAN_SESSION`, `KANBAN_TASK_ID`,
   and when registered `KANBAN_PROJECT` / `KANBAN_DATA_DIR`)
2. Use `--agent` flag for all commands
3. Call `kanban heartbeat` periodically while working
4. Add context via `kanban context`
5. Ask questions via `kanban ask`, or `kanban ask --wait --session <id>` when the task is interactive and the question is blocking
6. For long detached external work, prefer `kanban detach <id> --session <id> --eta SECONDS --note TEXT -- <command>` (starts the command so it survives the session and declares the wait in one step). A plain shell background job dies with the session's process group; when detaching manually (`setsid` + `nohup`, output to a file), declare the wait with `kanban waiting <id> --session <id> --eta SECONDS --note TEXT`. Either way the pause releases the agent slot and at the deadline the task re-enters the queue (or, with the queue off, the agent is relaunched directly) to check the result
7. Finish according to role — never `kanban move` a task to Done:
   - executor: `kanban done` (Review, or bot review when the reviewer is on)
   - designer: do not implement and do not move the task; record the plan with `kanban context` and finish the design phase with `kanban done`
   - reviewer: do not implement fixes and do not move the task; the only exit is `kanban verdict`

Closure invariant for non-interactive **executor** jobs: after implementation and verification are complete, do not stop at a progress update, green test report, or pending specialist review. Record final context and run `kanban done <id> --session <id> --agent` in the same execution unless a blocking ambiguity requires `kanban ask --agent`, or a long-running detached result requires `kanban waiting --session <id>` or `kanban detach --session <id>`, and an immediate stop. A designer finishes its phase with the same `done` command after recording the plan (no implementation). A reviewer finishes only with `kanban verdict`.

## Agent Auto-Launch
When a task is handed to an agent (`take --agent`, or the TUI `r` Run action) and auto-launch is enabled, the CLI spawns the agent itself:
- Builds a non-interactive command per backend (see "Agent Backends"). Model resolves from `task.ai_model`, else the backend default; reasoning effort from `task.ai_effort`, else the backend `effort` default. Automatic Codex/pi/omp relaunches reopen the most recent native conversation (`codex exec resume <id>`, `pi --session <id>`, `omp --resume <id>`) when the completed run's provenance manifest captured one. The follow-up contains only the new board session identity and thread messages added after that run; first launches, human restarts, missing manifests, backend changes, and revert jobs keep the full fresh prompt.
- The assembled prompt is written to `.kanban/logs/<session>.prompt.txt`. The wrapper feeds it as the last argument with `"$(cat -- <file>)"` so the body is never placed on the tmux/`bash -c` argv (ARG_MAX / `ps`).
- The prompt is role-scoped (`Role` from the task's run phase): an executor backs up touched files, records progress via `kanban context`, and finishes with `kanban done --agent` (never a move to Done); a designer records a plan and finishes the design phase with `done` without implementing or moving the task; a reviewer checks the result and exits only via `kanban verdict`. When `interactive: true`, blocking questions go through `kanban ask --wait --session <id>`. Long detached waits go through `kanban detach --session <id> -- <command>` (preferred; survives the session and records output/exit code under `.kanban/detached/`) or a manual `setsid`/`nohup` launch plus `kanban waiting --session <id>` — the prompt warns that plain background jobs die with the session's process group. Clean exits that leave a task In Progress without `done`, `ask`, `verdict`, or `waiting` are automatically resumed up to `max_auto_resumes`. The prompt stays backend-neutral. An isolated task's prompt additionally opens with an Isolation paragraph: the checkout at `<data_root>/.kanban/worktrees/<TASK-ID>` was cut from a live snapshot of the project folder, so it already contains the human's uncommitted work; commit freely on the branch (it merges back when the task is done); never create, switch, or delete branches, and never touch the project folder's own checkout (see "Worktree Isolation").
- If `use_tmux` and tmux is available → `tmux new-session -d` with stdin/stdout/stderr detached from the TUI TTY (`-x`/`-y` size, `-c` work path; tmux stderr goes to `.kanban/logs/<session>.tmux.err`). A non-zero tmux exit takes the same background fallback as a missing tmux binary; the exact error is posted on the thread and returned to the TUI status bar instead of `eprintln`. Either way agent stdout/stderr is teed to `.kanban/logs/<session>.log`. Session ids are prefixed by backend (`ses-<backend>-...`).
- While the TUI owns the terminal, `operations` never writes to stderr (`eprintln`). After a TUI-initiated launch (run / revoke / re-run / revert, or an expired-wait relaunch) the event loop `terminal.clear()`s and fully redraws, same as after attach, so a leaked glyph cannot desync ratatui's buffer from the alternate screen.
- Agent exit is watched to reconcile task/session state. Transcript provenance and the agent reply are harvested before a possible automatic successor is spawned, so native resume can use the conversation id from the run that just ended.

## Queue Dispatcher (`core/scheduler.rs`)

`Operations::dispatch_queue()` starts queued tasks while the concurrency caps
have room. It is a plain library call with no TUI or terminal assumptions.

**Occupied slots.** The census (`Slots::measure`) counts every **In Progress**
task whose session state is `Live`. A declared wait is a pause: the agent
process is normally already gone, so the pause releases its slot, and the task
re-enters the queue when the wait ends. Tasks in phase
`queued` own no session and count for nothing. Each counted task is bucketed by
its **resolved** launch settings (task override, else the backend default), so
a task that inherits the backend model is counted under the same
`<backend>/<model>` key as one that names it explicitly, and by its role
(`Role::from_phase`). Before counting, dispatch reaps dead sessions exactly as
`check-sessions` does — otherwise a session whose process died silently would
hold its slot until the `session_heartbeat_timeout` (30 min) — and runs
`due_restarts()`.

**Candidate order is the board's own sort.** `tui.task_sort` is mapped onto the
same `sort_tasks(by, order)` call the board screen uses (`task_number` →
id/asc, `task_number_desc` → id/desc, `updated_at_asc`/`updated_at_desc` →
updated/asc|desc, legacy `completion_date` → updated/desc, anything unknown →
id/asc). Changing the board sort therefore changes the queue priority. Only
queued tasks with no pending `restart_at` in the future are candidates.

**Caps.** For each candidate the dispatcher resolves the launch settings and
the phase it would enter (`upcoming_run_plan`: `design` when the designer is
enabled and no review bounce has happened yet, else `execute`), then asks
`Slots::blocking_cap` for the first cap that refuses one more agent. Caps are
checked total → backend → backend/model → role, and a cap of `0` or an absent
entry is unlimited.

- The **global total** is the only head-of-line-blocking cap: hitting it stops
  the walk (`DispatchDecision::Stop`).
- Every other cap only **skips** its own candidate and the walk continues, so a
  full claude quota never holds back an opencode task.

**Claiming.** Because several pumps can run at once, a start is claimed under
the board lock: re-read the task, verify it is still In Progress with phase
`queued`, no live session and no future `restart_at`, re-measure the caps, mint
the session id, flip the phase and persist — all in one locked
read-modify-write. The launch itself happens outside the lock. A launch failure
hands the task to the crash-restart backoff instead of hot-looping.

**Pump points** — `dispatch_queue()` is a library call, not a process of its
own; something has to call it. Most callers need a human or agent process to
be present; the daemon is the headless clock:

1. `App::tick` → the daemon's store-wide tick, at most once every 5 s, from
   every TUI screen. It advances every registered project, not only the board
   currently on screen. An unregistered in-place board keeps the local
   `dispatch_queue_throttled` fallback. Errors land in the status line.
2. `kanban check-sessions` — the manual headless one-shot.
3. `reconcile_agent_exit` — an exit frees a slot, so the launch wrapper pumps
   the queue even with no TUI open.
4. `kanban verdict --changes` with `on_changes_requested: in_progress`, which
   re-queues the task and pumps immediately.
4b. `r` Run and the `Ctrl+R` re-run pump the queue once on the spot, so an
   idle board starts the pressed task immediately while a full board parks it
   `⏸ queued` (both fall back to the direct launch when the queue could never
   drain — see "Run Phases").
5. `kanban daemon` — the scheduled headless pump. `--once` is one tick; the
   looping form is what the systemd user unit runs. Without it and with no TUI
   open, a queued task or due crash-restart sits until something calls (2)–(4).

## Headless Dispatcher Daemon (`core/daemon.rs`, `cli/daemon.rs`)

This is the answer to "nothing starts while the TUI is closed". The TUI now
pumps every registered board while it is open, whether it shows a board or the
projects screen. The other interactive pump points still need a human or agent
to be present. When no TUI is running, `kanban daemon` is the one pump that
runs on a clock; without it, a queued task or due crash-restart can sit until
something calls another pump point.

```
kanban daemon [--interval SECONDS] [--once] [--project <id|name|path>]
```

It is a **foreground loop and never forks** — daemonizing is the supervisor's
job (systemd, or `&` in a shell). `core/daemon.rs` holds the store-wide tick
and the lock and has no terminal assumptions; `cli/daemon.rs` is the loop,
the sleep and the log append.

**Per-tick order.** For every project in the store registry
(`ProjectStore::list()`, or only the one named by the global `--project` flag),
`pump_project` runs four steps in a fixed order:

1. `wake_expired_waits()` — a declared wait whose `wait_until`
   (`eta × waiting_eta_multiplier`) has passed **and** whose process is gone
   (`wait_exited`, or silent longer than `session_heartbeat_timeout`) is ended:
   the task parks back into the queue (`run_phase = queued`, old session
   closed) and the same tick's `dispatch_queue` starts it when a slot is free;
   a still-heartbeating agent past its own ETA is left alone, and a task out
   of `max_auto_resumes` is crashed instead. With the queue or auto-launch off
   the agent is relaunched directly, as before.
2. **Reap** — `SessionManager::check_sessions(session_heartbeat_timeout)` marks
   dead sessions crashed, and each one is offered to `schedule_crash_restart`
   (which is itself a no-op when `auto_restart` is off or the backoff schedule
   is spent — see **Crash Auto-Restart**).
   Reaping comes **before** scheduling for the same reason it does inside
   `dispatch_queue`: `check_sessions` only returns sessions it *just* marked
   crashed, so a later tick would never see them again.
3. `due_restarts()` — crash-restarts whose `restart_at` has passed go back to
   phase `queued`.
4. `dispatch_ready_dependents()` — the graph's pull step: every To Do task
   whose `depends_on` are all satisfied is handed to the queue, so a node that
   became ready is started by the same tick.
5. `dispatch_queue()` — the normal cap-checked dispatch described above.

(The TUI's throttled pump and `kanban check-sessions` run steps 4 and 5 in the
same order.)

So a crash detected in tick *N* is scheduled in the same tick, and started in
whichever later tick its backoff comes due. The daemon adds no new rules: it
calls the same `Operations` methods the TUI does, and starts land under the
same locked claim, so a daemon and an open TUI pumping the same board at the
same time is safe.

**`--once` vs. the loop.** `--once` runs a single tick and exits — the cron /
systemd-*timer* entry point. Without it the process ticks, sleeps
`interval`, and repeats forever; that is what the systemd *service* runs.

**Interval.** Seconds between ticks, resolved as `--interval`, else the store
`daemon.interval`, else 60. `--interval 0` is rejected outright; a `0` or
unparseable `daemon.interval` in the config silently falls back to 60. This is
the only orchestration cadence that lives in **Global Settings**
(`<store>/config.yaml`) rather than a board's `.kanban/config.yaml`, because
the daemon spans projects — see **Global Settings**.

**Single instance.** Startup `flock`s `<store>/daemon.lock` (exclusive,
non-blocking). A second daemon exits with
`kanban daemon is already running (holds …)` rather than blocking or racing.
The lock is held for the process lifetime and released on exit. It does *not*
exclude a TUI or a `check-sessions` run — two daemons are merely pointless,
concurrent pumps are not unsafe.

**One bad project cannot kill the loop.** A project with
`orchestration.queue_enabled: false` is skipped silently. A project whose work
folder has disappeared is skipped with **one** warning (remembered in a
`warned_missing` set, so it is not repeated every tick). Any other per-project
error is printed as a `warning:` line and the loop continues to the next
project.

**Logging.** One timestamped line per `resume` / `reap` / `restart` /
`dispatch` (plus warnings), written to stdout *and* appended to
`<store>/logs/daemon.log`. Quiet otherwise, so `journalctl --user -u kanban4ai`
stays readable.

**systemd user unit.** `packaging/systemd/kanban4ai.service` — `Type=simple`,
`ExecStart=… daemon`, `Restart=on-failure`, `RestartSec=30`, `WantedBy=default.target`.
It is **never enabled automatically** by any install path:

- `scripts/install.sh --with-daemon` copies it to
  `${XDG_CONFIG_HOME:-~/.config}/systemd/user/kanban4ai.service`, rewriting
  `ExecStart` to the prefix that install used. Without the flag no unit is
  written at all (`scripts/test-packaging.sh` asserts both halves: no unit
  without the flag, and no `default.target.wants/` symlink with it).
- The AUR packages install the unit under `/usr/lib/systemd/user/` and stop
  there.

Enable it yourself:

```sh
systemctl --user enable --now kanban4ai.service
journalctl --user -u kanban4ai
```

**Non-systemd fallback.** A crontab line calling the one-shot form:

```
* * * * * kanban daemon --once
```

`kanban check-sessions` remains the manual one-shot equivalent for the current
project — same steps, no lock, no loop.

## Crash Auto-Restart

Distinct from `max_auto_resumes`, and deliberately on a separate counter —
these are different failure modes:

| | trigger | budget | field |
|---|---|---|---|
| auto-resume | **clean** exit that left the task stranded In Progress, or an expired wait (the task re-enters phase `queued` instead of launching) | `thresholds.max_auto_resumes` (3) | `auto_resumes` |
| crash restart | session **crashed**: non-zero exit or heartbeat timeout | `orchestration.auto_restart.delays_minutes` (`[1, 30, 270]`: 3 attempts, waiting 1 min after the first crash, 30 min after the second, 270 min after the third) | `crash_restarts` + `restart_at` |

On a crash, `schedule_crash_restart` sets
`restart_at = now + delays_minutes[crash_restarts]`, leaves the task In
Progress with phase `queued`, and the card shows `↻ retry HH:MM`. When the
deadline passes, `due_restarts()` — same pump points — increments
`crash_restarts`, clears `restart_at` and hands the task back to the normal
queue; the dispatcher starts it, so a retry after a subscription-limit crash is
gated by exactly the same caps as any other run. Once the schedule is spent the
task stays crashed and notifies, like an exhausted resume budget. Any human
action on the task (run, re-run, recover, take, queue, unqueue) resets
`crash_restarts` and `restart_at` via `Task::reset_human_restart()`. Setting
`auto_restart.enabled: false` disables the whole mechanism and restores the
previous behavior (crashed tasks wait for `u` recover).

Every failure on this path fires a desktop alert (`notifications.crash`,
urgency critical): with a retry pending it names the retry time and attempt;
with auto-restart off — or the queue unable to drain — it says so instead,
so a crashed task is never silent until a human happens to look at the board.
A spent schedule keeps the stronger stranded notification.

A backend transcript error with `isRetryable: false` (OpenCode credits/401,
and similar hard API failures) is posted on the thread as `✖ agent error: …`
and does **not** enter this backoff: the task stays crashed so a billing or
auth failure is not disguised as `↻ retry`. `format-stream` also renders
`type: error` events into the session log. A crash on a task that is already
`queued` but has no `restart_at` still gets a backoff — otherwise the
dispatcher immediately relaunches and a lone queued task hot-loops.

A retryable HTTP 429 that names when the exhausted window rolls over — an
`openai/*` opencode run on a spent ChatGPT plan answers
`usage_limit_reached` with `resets_at` / `resets_in_seconds`, and the
`x-codex-*` response headers carry the same instant — is scheduled *for that
moment* instead of the next ladder step, since every earlier attempt can only
429 again (a 1-minute backoff against a two-hour reset burns the whole budget
in three minutes). The deadline is floored a minute out (a reset already past
is clock skew) and ignored beyond `MAX_CRASH_RESTART_DELAY_HOURS` (24h); it
still consumes one budget step, so a backend that keeps failing cannot retry
forever, and the thread note reads `↻ crash restart scheduled at … (provider
quota resets then, attempt n/N)`. The usage headers on that error are also fed
to `limits::record_codex_usage`, so the limits row shows the spent quota
immediately rather than at its next poll.

Because a crash restart runs *through* the queue, `schedule_crash_restart` also
requires `orchestration.queue_enabled` **and** `auto_launch.enabled` to be on.
With either off it schedules nothing and the task simply stays crashed and
recoverable: promising a retry the dispatcher can never honour would leave the
card wearing a `↻ retry` badge forever.

## Task Chaining
A task may carry a `chained_to` target task id. When the **target** task enters Review — via `move` or an agent's `done` — every task whose `chained_to` equals that id and is still in **To Do** is auto-run with a fresh per-task session (its own backend/model/persona/description). Only the To-Do→Review transition fires it (re-entering Review does not). Gated by the `auto_launch_chained` rule and `auto_launch.enabled`.

## Task Dependencies (the DAG)

`chained_to` is the human's push: one target, fire-and-forget, no shared
context. `depends_on` is the orchestrator's graph and is a different mechanism
on purpose.

A task's `depends_on` is a list of task ids that must reach **Review or Done**
before it becomes ready. Two things ride on that edge:

- **ordering** — the node cannot start until every dependency has finished
  (an AND-join, unlike a chain's single parent);
- **context** — the node's prompt is opened with an *Upstream results* section
  built from each dependency's recorded context and harvested final reply,
  compacted by the existing rule-based compaction and capped by
  `orchestration.orchestrator.upstream_budget_chars` (split across the
  dependencies). Nothing is summarized by a model, so the section is
  deterministic. `Task.needs` — one or two sentences the orchestrator wrote
  about what this node takes from upstream — is printed above it.

The edge is **pulled, not pushed**. `dispatch_ready_dependents()` sweeps every
To Do task with edges and hands the ready ones to the queue in phase `queued`;
they start through the normal cap-checked dispatcher, so a wide fan-out cannot
bypass the concurrency caps. A sweep (rather than firing from the finished
task) is what makes an AND-join correct, picks up dependencies satisfied by a
*human* move, and stays idempotent enough to run on every tick.

A dependency whose task no longer exists counts as satisfied — a deleted
predecessor must not deadlock the graph — and the release is reported in the
thread note (`missing: TASK-nnn`).

**Cycles are refused at write time.** `kanban depends` and plan ingestion both
run a DFS over the whole board plus the proposed edges and reject anything that
closes a cycle, naming the path. Acyclicity is the termination guarantee: a
cyclic graph can never become ready.

```sh
kanban depends TASK-310                          # show edges, readiness, dependents
kanban depends TASK-310 --on TASK-308 --on TASK-309   # replace the set
kanban depends TASK-310 --clear
kanban create "Docs" --depends-on TASK-308
```

## Orchestrator Mode

A per-task opt-in (`use_orchestrator`, the **Orchestrator** checkbox in the
task form, `kanban create --orchestrator`). There is deliberately **no**
board-wide switch: an orchestrated run spends a whole graph of sessions, so it
is always a per-task decision. The orchestrator runs on the **task's own**
backend/model — the model is chosen on the task, not in `orchestration.*`.

The pass itself:

1. The task's first run enters phase `orchestrate` (before any design pass) and
   gets the orchestrator prompt: the DAG rules, the plan schema, the configured
   model rosters and the `max_subtasks` cap. That prompt is **role-scoped** —
   it is never added to `AGENTS.md`, which every session pays for.
2. The orchestrator writes a plan file and submits it with
   `kanban plan <task> --file <plan.yaml> --session <s> --agent`. The plan is
   validated whole (unknown references, duplicate or colliding keys, cycles,
   unknown role profiles, size) **before anything is created**; a refused plan
   costs one message, an accepted 200-node plan would cost 200 sessions.
3. Accepted, each node becomes a To Do task with its `depends_on` wired, its
   `needs` contract, its role profile and `parent_task` set to the planner. The
   planner itself becomes the **join node**: `depends_on` every node it created,
   `orchestrated: true`.
4. `kanban done` ends the phase: the planner returns to To Do, the graph's roots
   are queued, and the sweep drives the rest.

Moving an orchestrated task back to To Do by hand (a human move or `recover`)
drops its join (`orchestrated` and `depends_on` clear) so the next run plans
again — the same "start from the top" semantics `designed` has. A subtask's own
edges are never cleared this way: they are graph structure, not run state.

### Role model rosters (`orchestration.roles`)

Named, ordered lists of backend/model candidates the orchestrator may assign to
a node with `role:`:

```yaml
orchestration:
  roles:
    cheap:
      - claude/haiku
      - opencode/openai/gpt-5.5
    heavy:
      - backend: claude
        model: opus
        effort: high
```

A node starts on the first candidate (materialized onto its own
backend/model/effort/agent fields, so the census, the caps and the detail view
all describe what will actually run). When a run dies on a **provider limit**,
`advance_role_roster` moves the node to the next candidate and re-queues it
immediately instead of parking it until the quota window rolls over. A failover
does not spend a crash-restart step — the roster length already bounds it, and
`roster_index` is never reset automatically. With no candidate left, the normal
crash-restart backoff takes over.

### Role-scoped instructions

`<board>/.kanban/instructions/<role>.md` (`orchestrator`, `designer`,
`reviewer`, `executor`) is appended to that role's prompt only, when that role
is actually launched. `AGENTS.md` and `CLAUDE.md` are loaded into *every*
session, so anything role-specific written there is charged to every run on the
board; a role file is the opposite. Missing or empty files are skipped.
