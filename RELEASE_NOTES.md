# kanban4ai 0.3.2

## Highlights

- Deleting a task now removes its sidecar thread. Task ids are recycled
  (`max + 1`), so a leftover `.kanban/threads/TASK-NNN.yaml` was being adopted
  by the next task on that id — the detail view and the agent prompt both saw
  the deleted task's messages. Creation also discards any thread already sitting
  on a freshly allocated id, healing boards cleaned up by hand.
- Bracketed paste is enabled in the TUI. Clipboard text lands as one edit in the
  focused field (dialog Title/Description/Answer, search, detail answer box,
  review-edits editor) instead of being replayed as keystrokes. Tabs no longer
  hop between dialog fields, newlines no longer submit the focused button, and a
  paste on the board is dropped with a status hint rather than firing shortcuts.
- One-line fields flatten pasted newlines; control sequences are sanitized.
  `Ctrl+V` image paste is unchanged.

## Verification coverage

- Storage/operations tests: create after a bare delete does not inherit the old
  thread; abandon removes the sidecar.
- TUI tests: whole-block paste into Description, single-line flatten, control
  sanitization, board paste ignored, detail answer box accepts paste.
- Delete-modal snapshots updated for the new wording.
- Release checks for this version include rustfmt, clippy with warnings denied,
  locked tests, a release build, and installer packaging smoke tests.
