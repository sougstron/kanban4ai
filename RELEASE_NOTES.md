# kanban4ai 0.3.0

## Highlights

- Agents can now submit one or more structured questions with `kanban ask-form`; strict YAML forms map their choices to selectable variants in the TUI.
- Answering the final open question wakes a waiting interactive agent immediately. The new Revoke action safely replaces an In Progress task's agent session and fences stale agents from mutating the successor task.
- Thread messages now carry optional origin metadata, and rejected context can be quarantined from future prompt construction without losing its audit trail.
- Launch and exit lifecycle steps are recorded in the task thread. Per-session prompt captures and input-provenance manifests show the files, URLs, and MCP calls that an agent consumed without mixing telemetry into conversation context.
- The detail view preserves an in-progress custom answer and selected variant across live board refreshes, and adds read-only prompt/input viewers plus clearer drag-and-drop feedback.
- Boards can configure a verification command that records gate results and, by default, blocks an agent's transition to Review when verification fails.

## Verification coverage

- Added regression coverage for YAML question forms, quarantine and origin compatibility, lifecycle audit entries, provenance harvesting for both supported backends, and verification-gate outcomes.
- Added session-ownership and revoke coverage for waiting, process-exit, reused-id, stale-agent, and manual wake paths.
- Added TUI coverage for preserved answer drafts, rejected-message rendering, prompt/provenance viewers, revoke affordances, and visible drag state.
- Release checks for this version include rustfmt, clippy with warnings denied, locked tests, a release build, and installer packaging smoke tests.
