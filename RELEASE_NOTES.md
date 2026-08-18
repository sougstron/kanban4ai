# kanban4ai 0.4.5

## Highlights

- After a Review re-run (`Ctrl+R` / Re-run), the TUI closes the Review detail
  view and moves board focus to the same task in In Progress. The task had
  already left Review; the previous view left the user staring at an empty
  Review column. In-progress re-runs stay put.
- Status-bar notices no longer stick. One-shot messages (focus hints, "Created
  TASK-001", and similar) return to the idle string (`TUI ready`, or `Projects`
  on the projects list) after 3 seconds. Continuous strings stay: a limits
  refresh still in flight, copy / Ctrl+C notices (their own timers), and
  screen-mode titles (Archive / log / text view).
- Clicking a provider on the limits row still shows `Refreshing {provider}
  limits…`. Once the background refresh finishes the bar settles to
  `{provider} limits updated`, then returns to the previous status after 3
  seconds.

## Verification coverage

- Review re-run tests now assert Board + In Progress focus, including a
  re-run started from the board rather than the detail view.
- Status tests cover a focus notice expiring while the editor stays focused,
  action notices expiring, Projects idle remaining, and an in-flight limits
  refresh staying on `Refreshing…` until the provider CLI finishes.
- Release checks for this version include rustfmt, clippy with warnings denied,
  locked tests, a release build, and installer packaging smoke tests.
