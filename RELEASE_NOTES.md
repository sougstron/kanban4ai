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
