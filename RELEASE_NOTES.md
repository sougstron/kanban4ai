# kanban4ai 0.2.6

## Highlights

- Tasks now retain the session, backend, model, effort, and persona from their latest agent launch after completion or recovery, while attach actions separately verify that the recorded session is still active.
- Project Settings now offers task-number, oldest-updated-first, and newest-updated-first sorting across every board column, with deterministic numeric task-id tie breaking and compatibility for the legacy completion-date setting.
- The New/Edit task description editor now soft-wraps Unicode text and grows between five and ten rows after upgrading to Ratatui 0.30 and the maintained ratatui-textarea editor.
- Mouse-dragged TUI text can now be selected and copied through OSC 52, without breaking card clicks or cross-column drag-and-drop; Shift forces selection over interactive controls.
- Existing board configurations automatically gain newly shipped backend model aliases, including Claude's `fable`, while preserving configured ordering and custom entries.
- Tasks now record `completed_at` whenever work enters Review or Done, retaining the previous completion time during a rerun and refreshing it after the next completion.

## Verification coverage

- Added regression coverage for retained session identifiers, completion timestamps, and updated-time sorting in both directions; launch metadata persistence and inactive-session attach handling were also verified end to end.
- Added regression coverage for wrapped description editing, cursor and selection behavior, resize preservation, constrained layouts, and the Ratatui 0.30 rendering changes.
- Added regression coverage for text extraction, wide Unicode cells, OSC 52 encoding, selection notices, and coexistence with card drag behavior.
- Added regression coverage for merging newly shipped model defaults into existing customized backend catalogs.
- Release checks for this version include rustfmt, clippy with warnings denied, locked tests, a release build, and installer packaging smoke tests.
