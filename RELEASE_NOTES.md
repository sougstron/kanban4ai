# kanban4ai 0.4.8

## Highlights

- Provider limits: background TTL refreshes no longer clobber a fresher
  claude/codex observation with older file-source numbers. Click-refresh
  stores live Claude usage-endpoint and Codex RPC data in `limits.json`;
  `retain_fresher_providers` now keeps the later `observed_at` for claude
  (window merge) and codex (ready observation) whether the snapshot comes
  from `fetch_all` or a background `store`.
- TUI: pressing `n` always opens the New Task dialog targeting To Do,
  regardless of the focused column or open task's status, so new tasks are
  visible to other board users before an agent or human picks them up.
- Agent backends: the `pi` model catalog now merges custom providers from
  `models.json` with the builtin/remote `models-store.json` cache, so
  custom-provider models (e.g. Yolo) show up in the TUI model selector
  instead of only the builtin catalog.

## Verification coverage

- `core::limits`: 52 tests covering the fresher-observation merge for claude
  and codex across `fetch_all` and `store`.
- TUI: covering test renamed to
  `phase_three_headers_new_task_always_targets_todo_and_bulk_confirmation_work`,
  asserting `n` always targets To Do regardless of focus.
- `agent_test`: `pi_models`/`pi_catalog` cover merging `models.json` custom
  providers with `models-store.json`, store entries winning on a duplicate
  selector.
- Release checks for this version include rustfmt, clippy with warnings
  denied, locked tests, a release build, and installer packaging smoke tests.
