# kanban4ai 0.4.3

## Highlights

- Claude's remaining-capacity row no longer freezes on a spent statusline
  bridge. The bridge short-circuit now requires *every* bridge window to still
  be running: a file whose `five_hour` window rolled over hours ago used to
  keep the row at "1% left, reset in the past" because `seven_day` resets days
  out. The OAuth usage endpoint became a second source instead of a fallback,
  polled at most once every 15 minutes (the last poll time lives in
  `<store>/claude-usage-poll`, so a run of CLI processes shares one interval);
  `kanban limits --refresh` is a user asking now and skips the interval.
- The two Claude sources are merged window by window: for each label the
  fresher observation wins, except that a window which has already reset never
  displaces one that is still running, and the reported observation time is the
  oldest reading that survived, so the row never claims to be fresher than the
  stalest number on it.
- Stored Claude access tokens renew themselves. An expired token (`expiresAt`,
  5-minute skew) or a 401 from the usage endpoint trades the refresh token at
  `POST https://platform.claude.com/v1/oauth/token` and writes the rotated pair
  back into `~/.claude/.credentials.json`, preserving the file's other fields
  and its `0600` mode so Claude Code is not left holding a retired token.
- For every provider, a window whose reset time has passed is dropped from the
  limits row and from `kanban limits` — its percentage describes a period that
  is over. A provider left with no live window reads `stale` (`n/a` at the
  narrowest row detail) instead of showing a frozen percentage.
- The Projects screen gained `o` (status-bar `o folder`): it opens the selected
  board's work folder in the desktop's own file manager, and on the pinned
  `+ Create project for <cwd>` row it opens the folder that row offers to
  register. The opener is spawned detached with its streams closed so it cannot
  write over the frame, and a folder that no longer exists is reported in the
  status bar instead of being launched. The new global setting
  `tui.file_manager` (edited in `<store>/config.yaml`) overrides the default
  chain of `xdg-open`, `gio open`, `nautilus`, `dolphin`, `thunar`, `nemo`,
  `pcmanfm`, `caja` (`open` on macOS).
- The projects list preselects the row under the mouse with a new fainter
  `theme.hover` background, so the pointer target is visible without moving the
  keyboard selection; the selected row keeps the stronger border colour.

## Verification coverage

- New unit tests in `src/core/limits.rs` cover bridge currency with a spent
  `5h` window beside a live `7d`, per-window source merging, running-beats-
  newer-but-reset, `live_windows`/`is_expired`, credential parsing with refresh
  token and expiry skew, refresh-response parsing (rotated and non-rotated),
  and credential write-back preserving other fields and the `0600` mode.
- Five `core::opener` unit tests cover opener resolution and folder rejection;
  TUI tests cover the `o folder` button handing the work path to a recording
  stub file manager, a missing folder being reported instead of launched, the
  hover preselection background, and reset windows dropping off the limits row.
- The claude limits change was verified live against this machine's own
  credentials: the row read `5h 1% left resets in -22h` from a 21.7-hour-old
  bridge file before, and `5h 79% left resets in 4h49m · 7d 82%` after, with a
  following non-forced run leaving the poll marker untouched.
- Release checks for this version include rustfmt, clippy with warnings denied,
  locked tests, a release build, and installer packaging smoke tests.
