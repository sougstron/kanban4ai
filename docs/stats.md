# Usage statistics (`core/stats.rs`)

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when you are touching
`core/stats.rs` or the hooks that call into it.

## What it collects

Two numbers, per task/session, with the same "обвес" (backend, model, effort,
agent name) on each: tokens spent and time spent. Purely programmatic — the
board itself appends one small JSON line at state transitions it already
drives (a session starting/ending, a declared wait, a queue entry, a
crash-restart backoff); no agent ever writes to it, unlike `core/provenance`
(agent-driven, harvested from the backend's own transcript) or the live
`core/telemetry` progress reader (never persisted).

## Storage

`.kanban/stats/events.jsonl`, per project (alongside `sessions/`, `logs/`).
One JSON object per line, one of two shapes:

- **Phase edge**: `{"kind":"phase","ts":...,"task_id":...,"phase":"running|queued|waiting|retry","edge":"enter|exit", backend?, model?, effort?, agent?}`.
  Tags are only ever present on a `running` `enter` — the other phases carry
  no backend/model breakdown in the report, so recording them there would be
  dead weight.
- **Usage**: `{"kind":"usage","ts":...,"task_id":...,"session_id":...,"tokens":N, backend?, model?, effort?, agent?}` —
  one session's final token tally, recorded once when the session closes.

A call site never has to know when the *previous* edge happened; it only
records the edge in front of it. Pairing `enter`/`exit` per `(task_id, phase)`
into closed `[start, end)` spans happens once, lazily, when a report is
rendered (`pair_records`). An edge whose partner never arrives — the process
died before writing it, or the events file starts mid-history — is simply
dropped rather than guessed at: an `exit` with no open `enter` is discarded,
and a still-open `enter` (a session that is still running right now) is not
counted until it actually closes.

## Phases and where they are recorded

`Running` covers Design/Execute/Review alike — the report does not split
sub-phases, only "an agent session is live and not in a declared wait":

- **Enter**: `SessionManager::link_session` / `link_named_session` — every
  session start, whichever role or launch path claimed it (dispatcher, direct
  launch, revoke, designer→executor handoff). Tags come off the task's
  current launch fields (`Tags::from_task`) at that exact moment.
- **Exit**: `SessionManager::close_session` / `crash_session`, which also
  tally the session's final tokens (`telemetry::read_session_progress` against
  the full transcript) into a `Usage` record. Which phase actually closes —
  `Running` or `Waiting` — is decided by whether the session still carried a
  `wait_until` at that instant, mirroring the `was_paused` check already used
  elsewhere for the same purpose. A no-op re-close (already non-`Active`) is a
  no-op here too, so calling `close_session` twice never double-records.

`Queued` — parked in the dispatcher queue (`run_phase == Queued`):

- **Enter**: every place a task is freshly queued — `queue_run` (`Q`/`r`
  fallback), the auto-queue branch of `take_task_inner` (slots full on
  take), a bot-reviewer `VerdictRoute::Requeue`, the queued branch of
  rerun-from-In-Progress, `revoke_in_progress_task`'s wake-a-paused-task
  branch, and `queue_expired_wait` (a declared wait's deadline passing).
  `due_restarts` also re-enters here once a crash-restart backoff is due.
- **Exit**: `Operations::claim_run_phase` (every direct-launch path that
  claims a session outside the dispatcher) and `Scheduler::claim_queued_task`
  (the dispatcher's own launch) — the two and only places a task's phase
  actually leaves `Queued`.

`Waiting` — the agent declared a wait (`kanban waiting`):

- **Enter**/**Exit(Running)**: `SessionManager::set_wait`, but only on a
  *fresh* wait (`wait_until` was previously unset) — renewing an
  already-declared wait with a new ETA must not re-close a `Running` span
  that already ended at the first call.
- **Exit(Waiting)**: whichever `close_session`/`crash_session` call ends the
  session next (see `Running` above — the same check decides which phase it
  was in).

`Retry` — crashed, with a restart scheduled:

- **Enter**: `Scheduler::schedule_crash_restart_at`, the moment the backoff
  deadline is actually set. Note the on-disk `run_phase` field is set to
  `Queued` at the very same call (existing behaviour, unrelated to this
  module) even though the task is not really eligible to dispatch until
  `restart_at` passes — `dispatch_queue`/`claim_queued_task` both re-check
  `restart_at` before treating it as genuinely queued, and this module's
  `Retry` phase follows that real eligibility, not the field's literal name.
- **Exit**: `Scheduler::due_restarts`, when the deadline has actually passed
  and the task is hers back to the queue (paired with a `Queued` `Enter` at
  the same instant).

## Report

`render_report` (called by `collect_store_report`, which loads every
registered project via `ProjectStore::list()`) produces three sections as
plain text, read by both `kanban stats` and the TUI Stats window
(`Screen::TextView`, opened with `S` on the Projects screen — the only screen
with no single project in context, matching where the report aggregates
from):

1. **Tokens** — total, top backends, top providers, top models (max 10), top
   projects; for all time, this month (resets the 1st), and this week
   (resets Monday).
2. **Time** — same shape, but only the `Running` phase. The single grand
   total is a wall-clock **union** of every running span (`union_seconds`):
   two tasks running at the same moment must not double the clock. The
   per-backend/provider/model/project breakdowns are plain sums instead —
   different backends running concurrently really are separate work, so
   summing them is correct and does not have the grand-total's
   double-counting problem.
3. **Tasks** (all time only) — task counts and per-task averages (tokens,
   time) by backend/provider/model, plus four *cumulative* totals (concurrent
   tasks summed, not deduplicated — the opposite of the Time section's single
   wall-clock total): running, waiting/pause, retry-wait, queue-wait.

Model breakdowns are capped at 10 entries everywhere; backends, providers and
projects are shown in full.

**Providers** are derived at report time from the model id — the segment
before its first slash (`openai/gpt-5.5` → `openai`, `zai/glm-4.7` → `zai`);
a bare model id has no provider and lands in `unknown`. Nothing new is stored
in the events file — existing logs report providers without re-recording.

## Two accepted approximations

Both fine for a for-fun feature, neither for anything load-bearing:

- **Task id recycling**: ids are reused after a task is abandoned
  (`docs/data-model.md`), so an all-time count keyed by task id can in
  principle conflate two different tasks that happened to reuse the same id
  months apart. Cross-project grouping (which must merge many projects' ids
  into one set) qualifies every task id by its project first
  (`project_task_key`) to rule out the far more likely collision — the same
  `TASK-005` existing in two different projects.
- **"Completed task"** is approximated as *a task id with at least one
  recorded `Usage` entry* (at least one of its sessions closed with tokens
  accounted for), not "reached the Done column" — Done status is not stable
  history (tasks get reopened, archived, or deleted) while the event log is
  append-only.
