# kanban4ai 0.2.4

## Highlights

- Warm the opencode model catalog when the TUI starts so task and project settings dialogs can switch from configured fallback models to the live catalog without blocking dialog open.
- Added hover highlighting across the TUI hitbox registry, including board cards, detail actions, answer choices, modal fields, modal options, and modal buttons.
- Detail-screen `Enter` now starts the open task only when it is still in To Do, while keeping `Enter` inactive for non-To Do tasks and answer/review text panels.
- Improved modal Save/Cancel helper hints by placing `(Ctrl + S)` on the left edge and the Tab navigation hint on the right edge of the button box.

## Verification coverage

- Added regression coverage for non-blocking opencode catalog warming and refreshing open forms after the warmed catalog becomes available.
- Added regression coverage for hover rendering on board cards, detail buttons, answer choices, modal fields, modal options, and modal buttons.
- Added regression coverage for detail-view To Do launch, non-To Do inactivity, and answer-panel safety.
- Refreshed affected TUI snapshots for the modal hint and hover rendering updates.
- Release checks for this version include rustfmt, clippy with warnings denied, locked tests, a release build, and installer packaging smoke tests.
