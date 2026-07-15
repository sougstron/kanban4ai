# kanban4ai 0.2.1

## Highlights

- Added declared agent waits with `kanban waiting`: agents can record bounded long-running waits, keep sessions alive until a relaunch deadline, and be resumed automatically to check the result.
- Hardened agent exit reconciliation with bounded auto-resume, explicit launch-failure reporting, safe session-id validation, atomic session writes, and clearer stranded-session recovery.
- Improved TUI visibility and controls for waiting, crashed, and stuck work: waiting deadlines are shown on cards/details/session rows, stuck tasks expose Recover, detail Run closes back to the board, and `b` is the discoverable bulk-review hotkey.
- Improved agent callback robustness when a running executable has been replaced on disk, documented the long-wait workflow for agent operators, and raised the default heartbeat timeout from 5 minutes to 30 minutes.

## Verification coverage

- Added regression coverage for declared waits, expired-wait relaunches, expired-wait launch failures/no-op recovery, auto-resume budget exhaustion, unsafe session IDs, waiting/crashed TUI rendering, log-tail session sanitization, detail Run behavior, and bulk-review shortcut routing.
- Release checks for this version include rustfmt, clippy with warnings denied, locked tests, a release build, and installer packaging smoke tests.
