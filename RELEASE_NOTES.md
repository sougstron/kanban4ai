# kanban4ai 0.3.4

## Highlights

- The pi agent family (`pi` and `omp`) is no longer launched blind. Both now run
  with `--mode json`, which streams the same NDJSON event log they persist in
  their session files, so kanban4ai captures a real transcript for them at
  `.kanban/logs/<session>.transcript.jsonl` instead of only a raw log tee.
- Running `pi`/`omp` cards get the same live telemetry as claude and opencode:
  token count, cost, last tool activity, and — for omp, which has a `todo` tool —
  a real todo progress bar replayed from its `init`/`append`/`done` calls. Tokens
  use the same live accounting as claude (`last_input + Σ output`) and cost sums
  each turn's reported total; the placeholder `message_start` and the duplicate
  `turn_end` events are skipped so nothing is double counted. pi has no todo
  tool and reports no todos.
- Input-provenance manifests are now harvested for `pi`/`omp` runs too. Their
  tool calls are classified into reads, writes, globs/greps, and URLs, with
  unrecognized tools recorded as external capabilities and paths canonicalized
  to repo-relative form — the same treatment opencode runs already got. The
  session-info panel therefore shows a backend conversation id and provenance
  for these backends instead of nothing.
- `kanban format-stream` renders pi/omp turns, so a live pane shows assistant
  text and `→ edit src/auth/mod.rs` activity lines rather than raw JSON.
- Fixed: `pi`/`omp` runs could hang forever. Both probe stdin even in
  non-interactive `-p` mode, and inheriting the tmux pane's TTY left them
  blocked with no output. The launch wrapper now closes their stdin.

## Verification coverage

- Telemetry tests: an omp transcript yields 2/3 todo progress, correct live
  token totals with the `message_start`/`turn_end` decoy events ignored, summed
  cost, and the last tool activity; a pi transcript yields tokens, cost, and
  activity with no todos.
- Agent launch-plan tests assert `--mode json` and a captured transcript file
  for both omp and pi.
- Release checks for this version include rustfmt, clippy with warnings denied,
  locked tests, a release build, and installer packaging smoke tests.
