# kanban4ai 0.5.4

The task thread now receives the agent's **whole session answer**, verbatim —
no summary, no shortening.

## Fixed

- **The thread only ever saw the agent's closing wrap-up, never its answer.**
  Agent-reply capture (`core/reply.rs`) kept only the *final* assistant
  message of the run. But a delegated agent finishes with `kanban` tool calls
  (`done`, `context`), and after tool calls its last message is always a
  30–200 char wrap-up ("Task done, moved to Review") while the substantive
  answer was printed earlier in the session. So the thread silently lost the
  answer and kept the wrap-up. Reproduced live on all three backend families
  with the same sentinel payload: claude/haiku (4230-char payload → 124-char
  wrap-up on the thread), opencode/mimo-free (4232 → 37), pi/minimax-free
  (4231 → 198).

  The capture now gathers **every assistant text of the run, in order** —
  byte-equal to what the session rendered: claude `assistant` events grouped
  by message id (the `result` event demoted to a fallback for runs with no
  recorded assistant text at all), opencode `text` events grouped by
  `part.messageID`, pi/omp every assistant `message_end` carrying text.
  Re-verified live after the change: the thread's agent-reply message is
  exactly the whole session text on every backend family.

- **`agent_reply_max_chars` default raised 4000 → 32768.** 4000 was smaller
  than a typical full answer, so even a correct capture would have been
  truncated. The cap and its truncation marker remain; `0` still disables
  recording the reply entirely.

## Verification coverage

- 820 tests green, including the updated unit tests in `core/reply.rs` and
  the two integration tests in `tests/operations_test.rs` asserting the
  whole-answer contract for every backend family.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --release --locked`,
  `sh -n scripts/install.sh scripts/test-packaging.sh` and
  `sh scripts/test-packaging.sh` all clean.
- Live end-to-end runs of the rebuilt binary on three backends — claude
  haiku, opencode `mimo-v2.5-free`, pi `openrouter/minimax/minimax-m2.7:free`
  — the thread's agent-reply message is byte-equal to the session's whole
  assistant text (4351 / 4283 / 4263 chars), no truncation marker.
