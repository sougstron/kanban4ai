# kanban4ai 0.4.7

## Highlights

- `kanban stop <id>` stops the task's running agent session and leaves the
  task In Progress (idle). The last session id stays on the task so `r` can
  run it again.
- TUI Board and Detail: `k` (and a `Stop k` action-bar / status-bar hint when
  the focused task is live or waiting) opens the existing kill confirmation,
  then stops without launching a replacement. Distinct from revoke (`r`),
  which restarts immediately. Sessions view still uses `x`.
- `Operations::stop_task` looks up the task's active session. `stop_session`
  now closes the session record before `tmux kill-session`, so a racing
  wrapper `agent-exit` cannot auto-resume or mark the stopped session crashed.

## Verification coverage

- CLI: `kanban stop` closes an active session, keeps the task In Progress,
  and reports no active session when the task is idle.
- Operations: `stop_task` requires an active session; a closed session is
  ignored by `reconcile_agent_exit`.
- TUI: In Progress detail/status show `Stop k`; the board `k` hotkey closes
  the session without relaunch.
- Release checks for this version include rustfmt, clippy with warnings denied,
  locked tests, a release build, and installer packaging smoke tests.
