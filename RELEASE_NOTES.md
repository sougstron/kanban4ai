# kanban4ai 0.2.5

## Highlights

- Task cards now open their detail view on the first mouse click, while drag-and-drop still moves cards between columns without accidentally opening the detail view.
- The detail view's review-edits panel is now mouse-focusable, with hover/focus highlighting and an explicit click-to-focus hint alongside the existing keyboard navigation.
- Background TUI refreshes now preserve unsaved review-edit text, the active detail focus, and the detail scroll position instead of clearing in-progress review feedback.

## Verification coverage

- Added regression coverage for first-click card opening and the drag-release path that keeps column moves separate from detail opening.
- Added regression coverage for mouse focusing and highlighting the review-edits panel.
- Added regression coverage for preserving dirty review edits across thread-triggered filesystem reloads.
- Release checks for this version include rustfmt, clippy with warnings denied, locked tests, a release build, and installer packaging smoke tests.
