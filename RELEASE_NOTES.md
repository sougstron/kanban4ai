# kanban4ai 0.2.2

## Highlights

- Added `kanban detach`: agents can start long-running commands in their own detached process session, keep output and exit status under `.kanban/detached/`, and declare the relaunch wait in one safe step.
- Hardened agent prompts for external waits: prompts now prefer `kanban detach`, warn that plain shell background jobs die with the agent process group, and document the manual `setsid`/`nohup` plus `kanban waiting` fallback.
- Changed the bulk Review shortcut: `b` and `R` now confirm moving all Review tasks to Done, while destructive confirmations default to No and Backspace no longer deletes tasks.
- Improved TUI detail and answering flows with multiline thread rendering, task-description context in the meta panel, Escape-to-close from thread focus, inactive Enter on task detail, scrollable answer variants, and a visible custom-answer cursor.
- Improved TUI polish across cards, search, status bars, and mouse handling: width-aware truncation, description and id filter highlighting, clickable filter clearing, contextual Tab hints, truncated long messages and column titles, safer drag detection, and expiring double-click state.

## Verification coverage

- Added regression coverage for detached command execution, wait declaration, session ownership checks, empty-command rejection, detached artifact cleanup, and CLI detach behavior.
- Added regression coverage for Review-to-Done bulk action confirmation, stale-source rechecks, Escape-closes-detail behavior, inactive detail Enter handling, multiline thread rendering, detail metadata, TextArea search rendering, answer scrolling, card-click expiry, drag-move guarding, and clickable filter clearing.
- Release checks for this version include rustfmt, clippy with warnings denied, locked tests, a release build, and installer packaging smoke tests.
