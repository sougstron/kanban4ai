# kanban4ai 0.5.3

A correctness release for worktree isolation. It also carries everything
from 0.5.2, which reached GitHub but never reached the AUR packages — AUR
users upgrading from 0.5.1 get both.

## Fixed

- **A merge conflict could never be resolved.** The conflict report tells you
  to resolve the markers in the task's own isolated checkout and finish with
  `kanban done` (or `kanban integrate`). That never worked: a conflicted
  landing left the integration ref where it was, so every later landing
  snapshotted the work folder onto the *pre-conflict* tip. The merge base
  never reached the snapshot your resolution had absorbed, your still-
  uncommitted edit was re-diffed against the original base every time, and
  the same conflict came back for ever — the only way out was copying the
  resolution into the work folder by hand.

  A conflicted landing now advances the integration ref to the snapshot it
  merged into the worktree. The next landing snapshots on top of that, so
  once the resolution commit has absorbed it the merge base *is* that
  snapshot, your edit reads as unchanged against it, and the resolution
  merges cleanly. The advance is a fast-forward carrying only the work
  folder's own state: no task's landed work moves, and no unlanded branch
  becomes an ancestor of the ref. Every landing invariant is unchanged —
  nothing is silently overwritten, and landing still never commits or stages
  on your branch.

- **The TUI could sit on a stale board.** Change detection keyed on file
  count and newest mtime. Filesystems differ in timestamp granularity and
  some carry whole seconds only, so a write landing inside the same tick as
  the previous read left the signature unchanged and the board did not
  refresh until something else moved. The signature now includes the total
  size of the files it already stats, so those writes are caught.

## Verification coverage

- 820 tests green, including two new regression tests: a conflict resolved
  *only* in the isolated checkout must land on the next integrate (verified
  failing before the fix), and a fingerprint change for a write whose mtime
  was pinned back to simulate a coarse clock.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --release --locked`,
  `sh -n scripts/install.sh scripts/test-packaging.sh` and
  `sh scripts/test-packaging.sh` all clean.
- Live end-to-end run of the release binary on a scratch board and a real
  git repo: the full conflict loop — agent and human editing the same line,
  a conflicted `done` that leaves the work folder untouched, a resolution
  committed only in the worktree, and an `integrate` that lands it with HEAD
  unmoved, nothing staged, unrelated human work intact, and the worktree and
  branch cleaned up. TUI board, live refresh from an external change, detail
  view and Project Settings verified in a real terminal.
