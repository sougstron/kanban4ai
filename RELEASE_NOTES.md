# kanban4ai 0.5.5

Review-column cards keep the yellow completed-task highlight after you open
them.

## Fixed

- **Completed tasks lost their yellow highlight once Review was opened.**
  The card border used `review_unseen` as the only completed-work signal.
  Opening a Review task clears that marker, so the border dropped back to
  the default and the column no longer read as finished work. The border
  now stays `theme.warn` for every card in Review (and still for unseen
  Review). Crash, retry, and open-question coloring are unchanged.

## Verification coverage

- 821 tests green, including `seen_review_cards_keep_the_yellow_completed_highlight`
  and updated board snapshots for the Review-column border color.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --release --locked`, and
  `sh scripts/test-packaging.sh` all clean.
