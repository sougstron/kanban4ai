# Agent reply capture, telemetry and attachments

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when you are touching `core/reply.rs`, `core/telemetry.rs`, or image attachments.

## Agent Reply Capture (`core/reply.rs`)
An agent's answer used to reach only `.kanban/logs/<session>.log`, so the task
thread showed the audit trail (launch, agent-written context, exit) but never
what the agent actually said. At exit `reconcile_agent_exit` extracts the
run's **entire assistant text** from the backend's machine transcript and
posts it as a `context` message (role `agent`, author `agent-reply`) just
before the `■ exit` audit line, so it is thread content like any other
context entry and feeds the next prompt.

The capture is deliberately the whole session, not the closing message:
delegated agents finish with `kanban` tool calls (`done`, `context`, …), so
their final message is a short wrap-up ("Task done, moved to Review") while
the substantive answer is the text printed earlier in the run. Extracting
only the last message demonstrably posted just that wrap-up and lost the
answer. Every backend therefore gathers all assistant text in order, exactly
as the session rendered it:

- claude: every `assistant` event's `text` blocks, grouped by message `id`;
  the closing `result` event repeats the last message and is only a fallback
  for runs with no recorded assistant text at all.
- codex: every completed `agent_message` item from an `item.completed`
  event; streamed `item.updated` partials are skipped.
- opencode: every `text` event, grouped by `part.messageID`.
- pi / omp: every assistant `message_end` carrying text (`turn_end`
  duplicates it and is skipped).
- Backends with no parseable transcript, and runs that ended without printing
  text, record nothing. Text identical to an existing `context` message is not
  posted again (agents commonly repeat their summary through `kanban context`).
- Unlike `core/provenance.rs` (telemetry, deliberately kept out of the thread)
  this is the agent's own prose and belongs in the thread.

The messages are kept as a list and assembled by `compose_reply` within
`agent_reply_max_chars` (`0` disables the capture entirely). The budget is
spent **from the tail**, not the head: the run's last message is laid down
first and earlier ones are prepended while they still fit, so a long run loses
its opening planning chatter rather than the answer it finished on — the
head-first clamp used to cut the conclusion off mid-sentence. Every earlier
message is additionally clamped to `agent_reply_message_max_chars` so one
mid-run wall of text cannot crowd out the rest. Cuts land on a line boundary
where there is one (agent answers are markdown; slicing a table row mid-cell
reads as corruption), and both markers —
`... (agent reply truncated, full text in <log>)` and
`... (N earlier agent messages omitted, full text in <log>)` — name
`.kanban/logs/<session>.log` so the dropped text can still be read in full.

## Live Agent Telemetry (`core/telemetry.rs`)
`read_session_progress` answers *how a run is going right now* by re-reading the
backend's machine transcript (`.kanban/logs/<session>.transcript.jsonl`) on the
TUI tick: todo progress, tokens, cost, and the last tool invoked. Where
`core/provenance.rs` harvests what a run consumed once at exit, this is
recomputed live and never persisted — the transcript stays the single source of
truth, so no new on-disk record or fixture surface is introduced.

- claude (`--output-format stream-json`): mid-run there is no cumulative total,
  so tokens are approximated as `last_input + Σ output`; the final `result`
  event's cumulative `usage` supersedes it and carries `total_cost_usd`.
  `TodoWrite` inputs give todo counts (last write wins).
- codex (`exec --json`): cumulative `input_tokens`, `output_tokens`, and
  `total_tokens` from `turn.completed` are read, with completed command/file
  change items providing last activity.
- opencode (`run --format json`): a `tokens` object on the event `part` is read
  best-effort (placement is not stable across versions), `todowrite` gives todo
  counts.
- pi / omp (`--mode json`): each assistant turn is finalized in one
  `message_end` carrying that turn's `usage` (`input`/`output` and
  `cost.total`) and tool calls, so tokens follow claude's live accounting
  (`last_input + Σ output`) and cost is summed per turn. `message_start` is a
  zeroed placeholder and `turn_end` duplicates the last message; both are
  skipped so nothing is double counted. omp's `todo` tool is replayed
  (`init`/`append`/`done`) into the progress counts; pi has no todo tool and
  reports none.
- Tool summaries reuse the provenance harvesters' helpers so both stay in
  lock-step on backend event shapes. Invalid session ids are rejected before any
  filesystem access.
- A backend with no parseable transcript, or a run whose transcript reported no
  usage, falls back to the log-scraping token estimate parsed from
  `.kanban/logs/<session>.log`.

On a running card the two telemetry rows (`▓▓▓░░ 2/3  12.4k tok  $0.42`, then
`→ Edit src/auth/mod.rs`) replace the static description. Cards stay uniform
within a column, but a column grows to its tallest card, so telemetry and badges
are never clipped while columns of plain cards keep the configured
`card_height_lines`; the description is the one row still allowed to clip.

## Image Attachments
Paste an image from the clipboard (`wl-paste`/`xclip`, or a file path in clipboard text), sniff the type by magic bytes (png/jpg/gif/webp), write it atomically under `.kanban/assets/images/`, and embed Markdown (`![pasted image](...)`) in the task description.
