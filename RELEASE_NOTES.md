# kanban4ai 0.5.7

A quieter status line and a cleaner thread: codex/yolo leave the limits row,
kanban's own audit notes can be hidden, and two `.kanban`-directory bugs are
fixed.

## Added

- **Hide kanban's own thread messages** (`tui.hide_kanban_messages`, default
  `false`). A Project Settings checkbox (and the config key) filters the
  task-detail thread's kanban-authored audit notes from the display — they
  stay on the sidecar thread, so nothing is deleted. Opening a task now pins
  the first line of the last visible message as high as possible without
  blank rows under the thread; a thread that already fits stays at scroll 0.

## Changed

- **codex and yolo are gone from the limits row** and from `kanban limits`.
  The codex subscription is paused, so its readers and app-server RPC client
  stay compiled and tested but it is neither fetched nor displayed; yolo is
  removed from the board's provider set entirely. Remaining providers:
  claude, grok, zai, synthetic.

## Fixed

- **Agent launches could litter the work folder with `.kanban/logs`.** The
  tmux wrapper's log directory fell back to a relative `.kanban/logs` path,
  so the `mkdir -p` in the wrapper ran after `cd` into the work folder.
  The wrapper now always creates the absolute log directory under the data
  root.
- **Read-only kanban commands created `.kanban/threads`.** `ThreadManager`
  created the threads directory as a constructor side effect, so merely
  resolving or reading a project could create board directories (and, for
  operations on other folders, a local `.kanban`). The directory is now
  created only when a thread is actually saved.

## Verification coverage

- `hide_kanban_messages_filters_thread_but_keeps_sidecar`
- `settings_save_persists_hide_kanban_messages`
- `opening_long_thread_pins_last_message_without_blank_tail`
- `opening_short_thread_does_not_scroll`
- `opening_tall_last_message_puts_header_at_top`
- `opening_with_filter_pins_last_visible_message`
- `new_does_not_create_kanban_directories`
- `create_context_and_heartbeat_do_not_create_local_kanban`
- `for_project_ops_do_not_create_a_local_kanban`
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --release --locked`, and
  `sh scripts/test-packaging.sh`

---

# kanban4ai 0.5.6

Unread Review work is yellow again; reading a card clears it.

## Fixed

- **Review cards were always yellow after 0.5.5.** Completed-by-agent work
  should only highlight while it is unread. A new Review task
  (`review_unseen`) gets a yellow border, a `●` on the card, and a `●` on
  the projects list. Opening the card is the human-read signal: both the
  yellow and the projects notifier go out. Focus and hover on an unread
  card stay yellow so selection does not hide the notifier.

## Verification coverage

- `unseen_review_cards_use_the_yellow_notifier_border`
- `seen_review_cards_drop_the_yellow_notifier_border`
- `focused_unseen_review_card_stays_yellow`
- `opening_review_detail_clears_the_unseen_notifier`
- `appending_agent_reply_context_keeps_review_unseen`
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --release --locked`, and
  `sh scripts/test-packaging.sh`
