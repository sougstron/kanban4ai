# kanban4ai 0.4.2

## Highlights

- Claude remaining-capacity now prefers the Claude Code statusline bridge
  (`kanban limits bridge install` / `remove`). Claude Code (>= 2.1.80) pipes
  `rate_limits` to the statusline on every turn; a generated shim tees that
  into `<store>/claude-rate-limits.json` while the original command still
  renders the line. The OAuth usage endpoint is only a fallback, and a 429
  no longer replaces last-good windows with `n/a` — the snapshot TTL doubles
  after each consecutive 429 (capped at 64×). Claude windows carry their
  true observation time so the row can show age the way codex does.
- The limits row and `kanban limits` now include z.ai (`◆`) and Synthetic
  (`✦`). z.ai reads the GLM Coding Plan quota from opencode's
  `zai-coding-plan` key; Synthetic reads `$SYNTHETIC_API_KEY` or opencode's
  `synthetic.key`. Both segments are display-only (long-lived keys; the
  background poll keeps them fresh).
- Machine-wide settings live at `<store>/config.yaml`. The Projects screen
  opens Global Settings with `s`; the Esc-from-board toggle moved there out
  of per-project Project Settings. A stale per-project
  `tui.escape_to_projects` key is ignored — boards that had it on need the
  toggle re-enabled once globally.
- The projects list is a labelled two-line table: the board's Project
  Settings name (or the registry name), the work path, right-aligned column
  counts, and Agents / Last opened that drop on a narrow terminal. A yellow
  `?` marks boards with open questions; `●` still marks unseen Review work.
  `kanban project rename` and the TUI rename write `tui.name` so the list
  shows the new name.

## Verification coverage

- Unit tests in `src/core/limits.rs` cover Claude statusline and OAuth
  payload parsing, bridge-vs-HTTP source preference, 429 TTL backoff, z.ai
  quota mapping (live-shaped payload, fallbacks, monthly-MCP sentinel), and
  Synthetic quota mapping (live-shaped and docs-example fallbacks).
- CLI tests cover `kanban limits bridge install` / `remove`, the hidden
  `statusline-bridge` recorder, and `kanban project rename` writing
  `tui.name`.
- `core::global` tests cover the store-root config round trip; TUI tests
  cover the Global Settings dialog, projects-table layout snapshots, the
  question-mark flag, and display-name preference.
- Release checks for this version include rustfmt, clippy with warnings
  denied, locked tests, a release build, and installer packaging smoke tests.
