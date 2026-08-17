# kanban4ai 0.4.4

## Highlights

- The New/Edit task dialog's Backend selector now leads with **Default backend
  (`<agent>`)**, matching Default model / Default agent. New tasks no longer
  silently pin the first configured backend; leaving the option selected keeps
  `agent_backend` unset so launches follow `auto_launch.default_agent` from
  Project Settings. The detail meta line shows `default` until a launch pins a
  concrete backend. Project Settings itself is unchanged: it still lists only
  concrete backends, because it *sets* the default.
- `q` on the Projects screen quits the TUI, matching the `q quit` title-bar
  hint. Previously it reopened the board the list was opened from. `Esc` still
  goes back to that board, or quits when the list is the entry screen.
- The Projects list can be ordered from Global Settings (`s` on the Projects
  screen). The new machine-wide `tui.project_sort` in `<store>/config.yaml` is
  `name` (alphabetical by the displayed name, the previous and default
  behaviour), `newest` (most recently created first), or `smart` (unread work
  first — unseen Review or open questions — then rows with running agents, then
  newest). Unknown values read as `name`.
- Tapping the claude segment of the limits row now refreshes it, the same way
  as codex and grok. A user tap (or `kanban limits --refresh`) force-polls the
  OAuth usage endpoint even when the statusline bridge file is still current,
  so a tap hours after the last Claude Code turn is not stuck replaying that
  file. Background polls still skip the endpoint while the bridge covers every
  window and still honor the 15-minute interval. zai and synthetic stay
  display-only.

## Verification coverage

- TUI tests cover Default-backend create/edit (unset pin, keep pin, clear a
  pinned claude task back to Default), `q` vs `Esc` on Projects (with a board
  behind the list and as the entry screen), project-sort orders including
  smart-tier unread/running, filter + pinned create-cwd, preload from the store,
  and persist through Global Settings.
- Limits tests now treat the claude segment as a `RefreshLimits` hitbox and
  cover a click reporting `Refreshing claude limits…`. The existing
  bridge-currency and per-window merge suite is unchanged.
- Release checks for this version include rustfmt, clippy with warnings denied,
  locked tests, a release build, and installer packaging smoke tests.
