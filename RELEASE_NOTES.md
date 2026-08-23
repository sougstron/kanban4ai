# kanban4ai 0.5.0

## Highlights

- Orchestration: In Progress now has run phases (`queued` → optional
  `design` → `execute` → optional bot `review`). To Do stays
  manual-start-only; Done stays human-only. The card badge shows the
  phase (`⏸ queued`, `✎ design`, `▶ running`, `⚖ review`) and a pending
  crash-restart shows `↻ retry HH:MM`.
- Queue: `Q` parks a task In Progress without launching. Caps
  (`max_running_total`, per-backend, per-`<backend>/<model>`, per-role)
  gate the dispatcher. Board sort (`tui.task_sort`, including new
  `task_number_desc`) is the queue priority. A full claude quota no
  longer holds back an opencode task.
- Optional designer and reviewer bots, configured in Project Settings.
  The designer plans (does not implement) and hands off on the same
  slot; the reviewer checks the result and exits only with
  `kanban verdict --approve` or `--changes`. Bounce budget is
  `reviewer.max_rounds`.
- Crash auto-restart is a separate counter from clean-exit
  `max_auto_resumes`. Backoff is `orchestration.auto_restart.delays_minutes`
  (`[1, 30, 270]` by default). Human run / recover / queue resets it.
- Headless daemon: `kanban daemon [--interval SECONDS] [--once]` ticks
  every registered project (resume waits → reap → due restarts →
  dispatch) so the queue keeps moving with the TUI closed. Opt-in
  systemd user unit (`scripts/install.sh --with-daemon` or the AUR
  package); never enabled automatically. Cron fallback:
  `* * * * * kanban daemon --once`.
- TUI: hovering a card *is* selecting it — one selection, whichever
  input moved last. Multi-line dialog fields (Description, Add-message,
  custom Answer) insert a newline on Enter / Shift+Enter / Alt+Enter.
  Project Settings now edits the whole `orchestration:` block.
- Limits row: yolo (`1d` / `7d` / concurrency) sits after synthetic.
  Every provider segment is click-refreshable. Tick-regenerating
  (synthetic) windows no longer vanish at a regen tick; a transient
  fetch keeps the last good numbers; Claude 429 backoff no longer
  starves the other providers.

## Verification coverage

- Config: orchestration deep-merge, cap coercion (`0` = unlimited),
  `<backend>/<model>` first-slash keys, delays and
  `on_changes_requested` validation.
- Operations / scheduler: enqueue and dispatch (skip vs head-of-line),
  locked claim, crash-restart schedule and exhaustion, designer
  hand-off, both verdict routes, role-scoped move gates, golden
  fixtures still byte-identical when the new fields are unset.
- TUI: unified hover/keyboard selection, Description Enter-newline,
  `task_number_desc` column order, queued/design/review badges, and
  settings snapshots for the orchestration sections.
- Limits: yolo parse, rolling windows, retain-on-Unavailable, every
  provider segment clickable.
- Packaging: installer `--with-daemon` copies the unit and never
  enables it; AUR recipes ship it under `/usr/lib/systemd/user/`.
- Release checks for this version include rustfmt, clippy with warnings
  denied, locked tests, a release build, and installer packaging smoke
  tests.
