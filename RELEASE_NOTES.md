# kanban4ai 0.5.2

## Highlights

- **Worktree isolation.** With `max_running_per_role.executor` at 3, several
  agents used to edit the same folder and silently clobber each other. Each
  task's agent now runs in its own git worktree under
  `.kanban/worktrees/<TASK-ID>`, on a `kanban/<TASK-ID>` branch cut from a
  live snapshot of the work folder — modified *and* untracked files
  included, so the human's uncommitted work is there from the start. Two
  invariants hold end to end: nothing is ever silently overwritten (every
  landing path is re-checked against a fresh snapshot and the whole landing
  aborts on a race), and landing never commits on the user's branch (the
  merge happens in the object database and the result is written as plain
  unstaged changes; HEAD never moves, nothing is staged). Conflicts reuse
  the existing review-edits / re-run plumbing instead of new commands: the
  markers land in the task's own checkout, a structured report goes into
  `review_edits`, and `Ctrl+R` re-dispatches. Configured under
  `orchestration.isolation` (`mode`, `seed`, `land`, `on_conflict`,
  `cleanup`); `mode: auto` falls back to the shared folder with an audit
  note whenever git cannot support it. New `kanban integrate <id>` lands a
  branch by hand; worktrees and branches are cleaned up on land, Done and
  abandon, with a GC pass for orphans.
- **Self-updater.** `kanban update [--check]` checks GitHub Releases and, on
  an unmanaged install, downloads, SHA-256-verifies and atomically replaces
  the binary. A pacman-owned binary is never self-replaced: the command
  prints the right `yay -S` / `pacman -Syu` line instead. The TUI shows a
  one-time status-line banner per newly seen version, and Global Settings
  gains an Updates section with the status row, a `Check now` button, the
  `check on open` switch, and an `Update now` button on unmanaged installs.
- **Run means queue.** `r` now puts a task into the orchestration queue and
  pumps it once — an idle board starts it on the spot, a full board parks it
  with `⏸ queued`. Re-runs (`Ctrl+R`, `kanban rerun`) go the same way. The
  new `F` / `⚡ Now F` keeps the old direct launch for debugging, and every
  entry falls back to a direct launch when the queue could never drain.
- **A pause releases its slot.** A declared wait no longer occupies a
  dispatcher slot for the whole wait window; when the wait ends the task
  re-enters the queue instead of launching past the caps. Revoking a paused
  task queues it too.
- **The agent resumes after the last answer.** Answering the last open
  question wakes the agent on every task, not just `interactive: true` ones
  (rule `resume_after_last_answer`). A live `ask --wait` poller is left
  alone; a session whose heartbeat went stale is replaced instead of
  refusing with "Cannot revoke active session".
- **Card badges follow the column.** `☑ interactive` and `↪ chain` show only
  where they still mean something, the chain badge names its target
  (`↪ chain -> 154`), and pending `✎ design` / `⚖ review` stage marks show
  from the moment a stage is scheduled until it completes.
- **A dead agent is no longer disguised as a retry.** `format-stream` renders
  `type: error` events, the failure is posted on the thread as
  `✖ agent error: …`, and a backend error marked `isRetryable: false`
  (credits, 401) skips crash-restart so the task stays visibly crashed. A
  crash on an already-`queued` task now gets a backoff instead of
  hot-looping through the dispatcher.
- **Concurrent-write warning.** Sessions from different tasks that ran at the
  same time and wrote the same path get a `⚠ provenance overlap` note on
  both threads and a line in `kanban check-sessions` — the safety net
  wherever isolation does not apply.
- **Fixed:** a leftover `queued` phase no longer sticks to an already-running
  agent after a crash-restart, revoke, or stranded re-run.

## Fixes found in release review

- The landing race guard probed repo-relative paths against the kanban
  process's working directory instead of the work folder. A file the human
  created by hand at a path the task branch also adds escaped the check and
  was overwritten. It is now resolved against the repo root, with a
  regression test.
- `kanban answer` accepted any `MSG-id`, stamping an answer onto a task or
  context message while the real question stayed open. Only questions are
  answerable now.
- Post-land cleanup sweeps any directory left under
  `.kanban/worktrees/<TASK-ID>` after `git worktree remove`. A stray file
  written by an exiting agent process used to make `git worktree add` refuse
  that task id forever, silently dropping it back to the shared folder.

## Verification coverage

- 818 tests green, including a new `tests/isolation_test.rs` suite (19 cases:
  live-snapshot launch, snapshot chaining, seed modes, availability
  fallbacks, worktree reuse, clean landing with the no-commit invariant,
  conflict → report → resolution → land, `land: manual`, cleanup on
  Done/abandon, the GC pass and integration-ref re-baseline, and a conflict
  worktree surviving every cleanup path).
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --release --locked`,
  `sh -n scripts/install.sh scripts/test-packaging.sh` and
  `sh scripts/test-packaging.sh` all clean.
- Manual end-to-end run of the release binary on a scratch board and a real
  git repo: isolation cut, landed and cleaned up a task branch, leaving the
  work folder with only its own uncommitted file; TUI board, detail view,
  Project Settings isolation row and the new action bar verified in a live
  terminal.
