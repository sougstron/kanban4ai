# kanban4ai 0.4.9

## Highlights

- TUI dialogs: Backend, Model, and "Chain to" selectors now start with a
  type-to-filter row. Typing narrows the list case-insensitively (the leading
  "Default …" / "No chain" entry included); Enter on a single match picks it
  and advances; Enter on no matches paints the section in the theme error
  colour and stays put. Effort, agent, status, theme, and sort stay unfiltered
  — those lists are short and fixed.
- Dialog navigation: Enter commits the focused field and moves to the next
  one; it only submits from Save. Checkboxes toggle on Space only. Multi-line
  fields take Shift+Enter (or Alt+Enter) for a newline. A filter lasts only
  for the visit that typed it — leaving the field clears it.
- TUI: the open project is named on Board, Detail, Sessions, and Archive as a
  ` ▸ <name> ` badge in the top border of the rightmost block, and in the
  terminal window title (`<name> — kanban4ai`). The badge degrades
  full → truncated → dropped, and a click opens the Projects list.
- Agent backends: the `pi` model catalog now also merges bundled `pi-ai`
  provider catalogs (`providers/data/<provider>.json`) for every provider
  listed in `auth.json`. Authenticated built-ins that never land in
  `models-store.json` (OpenRouter) show up in the TUI model selector.

## Verification coverage

- TUI: filter narrow/restore, Enter on one match vs no matches, empty option
  lists, filtered click indices, form-wide Enter vs Shift+Enter, Space-only
  checkbox toggle, live catalog refresh under a filter, Backend filter
  commit, and effort/agent swallowing typed characters. Snapshots cover the
  filter row, "no matches", and error colouring.
- TUI: project badge on Board/Detail/Sessions/Archive, degradation across
  160/96/60 columns, badge hitbox precedence, no badge on Projects, window
  title naming / control-character strip / 64-column clip.
- `agent_test`: `pi` catalog merges `auth.json` providers with the bundled
  `pi-ai` data dir; OpenRouter selectors appear when that provider is
  authenticated. Live `installed_pi_openrouter` covers a real `pi` catalog.
- Release checks for this version include rustfmt, clippy with warnings
  denied, locked tests, a release build, and installer packaging smoke tests.
