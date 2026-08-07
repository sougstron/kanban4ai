# kanban4ai 0.3.5

## Highlights

- The agent's closing answer is now recorded on the task thread. A session's
  final reply — the summary the backend prints as its last words — used to live
  only in `.kanban/logs/<session>.log`, so the thread showed the audit trail
  (launch, agent-written context, exit) but never what the agent actually said.
  At exit `reconcile_agent_exit` now extracts the final assistant message from
  the backend's machine transcript and posts it as a `context` message
  (role `agent`, author `agent-reply`) just before the `■ exit` audit line, so
  the reply is thread content like any other context entry and feeds the next
  prompt.
  - claude: the `result` event's `result` is the finished answer; without one
    (interrupted run) the last `assistant` message's `text` blocks are used,
    grouped by `message.id` so earlier turns are dropped.
  - opencode: `text` events carry `part.messageID`, so the final message is the
    last group of text parts sharing one id.
  - pi / omp: the last assistant `message_end` carrying text (`turn_end`
    duplicates it and is skipped).
  - Backends with no parseable transcript, and runs that ended without printing
    text, record nothing. Text identical to an existing `context` message is
    not posted again (agents commonly repeat their summary through
    `kanban context`), and the body is clamped to `agent_reply_max_chars` with a
    `... (agent reply truncated)` marker; `0` disables the capture entirely.
- New configurable threshold `agent_reply_max_chars` (default 4000) controls the
  maximum length of the recorded reply; `0` disables agent reply capture. No
  hardcoded budget in business logic.

## Verification coverage

- Unit tests in `src/core/reply.rs`: transcript parsing and truncation for each
  backend, deduplication against existing context, and the disabled
  (`agent_reply_max_chars = 0`) case.
- Integration tests in `tests/operations_test.rs`: `reconcile_agent_exit` posts
  the reply as `context` / `agent-reply` immediately before the `■ exit` step,
  and a repeated/stale `agent-exit` callback cannot duplicate it.
- Verified against real on-disk transcripts: the opencode transcript of an
  earlier task yields exactly the answer that was missing from its thread;
  claude transcripts yield their `result` text.
- Release checks for this version include rustfmt, clippy with warnings
  denied, locked tests, a release build, and installer packaging smoke tests.
