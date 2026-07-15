# kanban4ai 0.2.3

## Highlights

- Deferred opencode persona resolution out of the launch path. Starting a task no longer blocks the TUI or CLI while `opencode agent list` starts; the spawned wrapper resolves the requested persona after the session heartbeat loop is running and falls back to the requested name if resolution fails.
- Added a hidden `kanban resolve-agent` callback used by runtime wrappers to map friendly opencode persona keys to the exact registered `--agent` name without slowing the launching process.
- Improved TUI onboarding and version visibility: empty To Do columns now show `press n to create task`, and the help popup header includes the current `kanban4ai` version from Cargo metadata.

## Verification coverage

- Added regression coverage for deferred opencode persona resolution in wrapper scripts, literal `--agent` preservation when deferral is not requested, fallback behavior when the resolve callback fails, and launch-plan storage of the requested persona.
- Added regression coverage for the versioned TUI help header and refreshed empty-board snapshots for the To Do hint.
- Release checks for this version include rustfmt, clippy with warnings denied, locked tests, a release build, and installer packaging smoke tests.
