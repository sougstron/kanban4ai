# kanban4ai 0.3.3

## Highlights

- Running cards now show live agent telemetry instead of their static
  description: a todo progress bar, tokens spent, cost, and a
  `→ Edit src/auth/mod.rs` activity line for the last tool the agent invoked.
  It is derived from the backend's transcript on every refresh tick and never
  persisted, so the transcript stays the single source of truth. claude reports
  todos, live tokens and (at exit) cost; opencode reports todos and tokens
  best-effort; backends without a parseable transcript keep the old
  log-scraped token estimate.
- Columns grow to their tallest card, so telemetry rows and badges are no
  longer clipped while columns of plain cards keep the configured card height.
  The `☑ interactive` badge is emitted first so a long session-state badge
  (`✖ crashed · u recover`) can't push it off a narrow card.
- `Enter` in the Sessions view and `t` on a task now *open* a session rather
  than only attaching: a live tmux session attaches as before, a background
  agent with no terminal follows its log, and a stopped session whose backend
  conversation id was recorded reopens with `<backend> --resume` (claude
  today). Previously anything but a live tmux session reported "no running
  session" and left you with nothing.
- New session-info panel: `i` in the Sessions view shows elapsed time, tokens,
  cost, todo progress, last activity, and the input provenance harvested so
  far. The Sessions list itself gained todo progress and an activity column.

## Verification coverage

- Telemetry tests: claude live-token estimate, the final `result` event
  superseding it with cost, opencode part parsing, unparseable and missing
  transcripts producing no data without panicking, and rejection of an invalid
  session id.
- Card tests: progress-bar fill and rounding (including an empty todo list) and
  compact token formatting; TUI tests and snapshots updated for the new badge
  order and grown card rows.
- Release checks for this version include rustfmt, clippy with warnings denied,
  locked tests, a release build, and installer packaging smoke tests.
