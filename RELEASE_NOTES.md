# kanban4ai 0.4.1

## Highlights

- The Board and Projects screens now show remaining AI subscription capacity
  in a row above the status bar (`✳ claude 5h 66% ↻3h30m · 7d 95% ↻6d11h │
  ✺ codex mon 75% ↻18d │ ✕ grok 7d 93% ↻4d22h`). Percentages are what remains,
  not what is spent. Providers with no credentials on the machine are omitted.
  `kanban limits [--format table|json] [--refresh]` prints the same data.
- Sources are read-only and best effort: claude via Anthropic's OAuth usage
  endpoint, grok via its billing API, both through `curl -K -` so tokens never
  appear on a command line; codex from the last local `rate_limits` payload
  (no network), so those numbers carry the age of the last run. Results are
  cached in memory and in `<store>/limits.json` with a configurable TTL
  (`limits_refresh_interval`, default 120s). `tui.show_limits` hides the row.
- Clicking the codex or grok segment refreshes that provider through its own
  CLI: codex is asked live over the app-server JSON-RPC, and `grok models`
  renews the short-lived OIDC token before the billing fetch. Claude stays
  display-only. Both CLIs run in a scratch store cwd so stray session state
  never lands in a project.
- Projects-list rows put column counts, the running-session `▶` count, and the
  unreviewed `●` marker next to the project name. The work path still fills
  the middle; last opened stays on the right.
- `P` opens the projects list from a Russian keyboard layout as well (`З`).
  New opt-in `tui.escape_to_projects` (Project Settings checkbox, default
  off) makes Esc on the Board open that list after any search filter is
  cleared.

## Verification coverage

- Unit tests in `src/core/limits.rs` cover transcript and RPC payload parsing
  for codex windows, including camelCase app-server responses.
- TUI tests cover the limits row (placement, width degradation, hide/disable,
  Projects screen), click hitboxes on codex and grok only, and the click
  status message.
- TUI tests cover the Russian `З` projects hotkey, Esc-to-projects on/off and
  search-clear ordering, persisting the setting, and the projects-row layout
  snapshots.
- Release checks for this version include rustfmt, clippy with warnings
  denied, locked tests, a release build, and installer packaging smoke tests.
