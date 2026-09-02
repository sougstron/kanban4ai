# Worktree isolation, backup and revert

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when you are touching `core/vcs.rs`, isolated checkouts, landing/merge, or the backup/revert path.

## Worktree Isolation (`core/vcs.rs`)

**The problem.** `max_running_per_role.executor` defaults to 3, so several
agents run at once — and without isolation they all work in the same shared
`work_path`. Two agents editing the same file concurrently clobber each other
silently: last writer wins. The provenance overlap warning (end of this
section) only makes that visible after the fact.

**The model.** With isolation on, every task's agent runs in its own git
worktree instead of the shared folder:

- `refs/kanban/integration` (`orchestration.isolation.integration_ref`) is
  the spine: a moving ref that chains the snapshots the board cuts of the
  work folder.
- At launch (`launch_agent` → `prepare_worktree`), **under the board lock**,
  the task branch `<branch_prefix><TASK-ID>` (default `kanban/TASK-NNN`) is
  created with `git worktree add` at
  `<data_root>/.kanban/worktrees/<TASK-ID>`, and the task stores `worktree`,
  `branch`, and `base_commit`. The lock makes concurrent starts chain their
  snapshots instead of racing sibling ones.
- `seed: live` (default) cuts the branch from a **snapshot of the live dirty
  work folder** — a temp-index tree (`read-tree` + `add -A` + `commit-tree`)
  capturing modified **and untracked** files, honoring `.gitignore`, leaving
  the user's status/index/HEAD untouched — parented on the integration tip
  (on HEAD for the very first task, before the ref exists). Live matters
  because the human commits manually after moderation, so a feature can sit
  uncommitted for a long time: branching from committed HEAD would hand the
  agent a tree missing it. `seed: head` branches from HEAD and never touches
  the ref. Because each snapshot parents on the previous integration tip, two
  tasks' merge-base is the shared snapshot, not committed HEAD.
- Every launch root points at the worktree: the prompt's paths, tmux `-c`,
  the background process `current_dir`, verification gates, `kanban detach`,
  revert jobs, and provenance harvesting (whose recorded paths are
  relativized to the worktree so they stay repo-relative and comparable
  across tasks). An existing worktree is reused as-is, so re-runs continue
  the same branch.

**The two invariants.**

1. *Nothing is ever silently overwritten.* Before landing writes anything,
   every landing path is re-compared against a fresh snapshot of the work
   folder through a throwaway index; any real difference (the human edited a
   file while the agent ran) aborts the whole landing with nothing written.
2. *Landing never commits on the user's branch.* The task branch is merged
   in the object database (`git merge-tree --write-tree`, nothing written to
   any working tree), and the merged result is materialized into the work
   folder as plain **unstaged** working-tree writes — HEAD never moves,
   nothing is staged, and the user commits manually after moderation. The
   integration ref advances to a dangling merge commit (parents: previous
   integration tip + task branch tip), never onto any branch.

**Landing.** When the work completes (`kanban done` from the executor when
the reviewer bot is off, or the reviewer's verdict handing to human Review),
`land_on_review` runs: commit whatever the agent left uncommitted in the
worktree, snapshot the work folder as it is right now, preflight the merge,
and on a clean result materialize the merged tree into the work folder
(deletions included), advance the integration ref, mark the task `landed`.
Every failure defers with the reason on the task thread; landing never
blocks the move to Review.

**The conflict flow** reuses the review_edits / rerun plumbing end to end
instead of new commands. A conflicted preflight writes nothing anywhere: the
task keeps its worktree, `integration` becomes `conflict` (the one blocking
state), the human side is merged **into the task's own worktree** so markers
live only in the isolated checkout, and a structured conflict report —
conflicting paths with base/ours/theirs stage blob oids, `base_commit`, the
worktree path, and the resolve-there-and-`done` instruction — is written
into `task.review_edits`, the same buffer the human types review feedback
into. With `on_conflict: review` (default) the human edits the text and
re-dispatches through the normal rerun flow (`Ctrl+R` / `kanban rerun`); the
TUI retitles the edits panel `conflict report`, paints the Re-run button in
the alarm color, and badges the card `⚠ conflict`. With
`on_conflict: resolver` the rerun is dispatched immediately on a fresh
session: the agent resolves the markers in the worktree and finishes with
`kanban done`, which lands both sides' changes. `commit_all` in the worktree
refuses to conclude a merge that still has unmerged index entries, so
unresolved markers keep the landing re-conflicting instead of slipping the
markered tree into the work folder.

A conflicted landing also **advances the integration ref to its own snapshot
W** — the one it merged into the worktree — even though nothing landed. That
is what lets the loop terminate: the next landing snapshots the work folder
on top of W, so once the resolution commit has absorbed W the merge base of
`(new snapshot, task branch)` *is* W, the human's still-uncommitted edit
reads as unchanged against it, and the resolution merges cleanly. Without it
every snapshot is parented on the pre-conflict tip, the merge base never
reaches W, and resolving in the worktree re-reports the same conflict for
ever. The advance is safe by construction: W was snapshotted on the ref, so
it is a fast-forward, and it carries only the work folder's own state — no
task's landed work moves, and no unlanded branch becomes an ancestor of the
ref (a branch could only do so by already being an ancestor of the previous
tip). Resolving in the worktree without ever touching the work folder is
therefore the supported path, exactly as the conflict report instructs.

**Cleanup and GC.** `cleanup: on_land` (default) removes the worktree and
deletes the branch once the branch has landed. Done and abandon always clear
them regardless of `cleanup` — Done is terminal, and an abandon is an
explicit discard, so an unmerged branch goes too — except a `conflict`
task's worktree, the one place unmerged agent work lives, which survives
until resolved (or the task is deleted). A GC pass at the end of
`abandon_stalled_tasks` runs `git worktree prune`, removes every orphan
`.kanban/worktrees/<id>` directory and `<branch_prefix><id>` branch whose
task no longer exists (a leftover branch would block the recycled id's next
worktree), and — when no task holds a worktree — re-baselines the
integration ref to a fresh snapshot parented on HEAD, releasing the old
snapshot chain: the ref is a GC root, and without this every snapshot it
ever pointed at would stay alive forever.

**Configuration** — the whole `orchestration.isolation` block, validated
strictly (a value outside a closed set, a non-mapping `isolation:`, or a
non-string free-form value is a config error; unknown *keys* survive like
everywhere else):

```yaml
orchestration:
  isolation:
    mode: auto                # auto | off | required
    branch_prefix: kanban/    # namespace of the per-task branches
    integration_ref: refs/kanban/integration
    seed: live                # live | head — what a task branch starts from
    land: worktree            # worktree | manual — auto-land vs kanban integrate
    on_conflict: review       # review | resolver
    cleanup: on_land          # on_land | keep
    commit_message: "kanban: {task_id} {title}"
```

- `mode: auto` (default) isolates whenever isolation is available and falls
  back to the shared folder with an
  `⚠ worktree isolation unavailable (<reason>)` audit note on the thread;
  `mode: off` is always the shared folder; `mode: required` refuses the
  launch outright (the take rolls back) instead of risking a clobber.
- `land: manual` records `integration: pending` and defers to
  `kanban integrate <id>`, which runs the same sequence by hand and prints
  landed paths, conflicting paths, or the deferral reason.
- `commit_message` is the template for the commit kanban creates on the task
  branch; the built-in audit messages (`kanban: live snapshot before …`,
  `kanban: land …`) are currently hardcoded and the key is validated but not
  yet consumed.

**Availability and limitations.** The probe (`vcs::availability`, rendered in
Project Settings' read-only Worktree isolation row and as the trailing
`Isolation:` line of `kanban check-sessions`) answers `available`, or
`unavailable — <reason>` for: project not registered, git not found, git too
old (merge-tree needs >= 2.38), not a git repository, unborn HEAD (no
commits yet), or detached HEAD / rebase in progress. Whenever isolation does
not apply, the board behaves exactly as before — shared folder, last writer
wins — with the **provenance overlap warning** as the safety net: sessions
from *different* tasks that ran concurrently and wrote the same path get a
`⚠ provenance overlap` note on both task threads and a
`Provenance overlap: …` line in `check-sessions` (same-task re-runs are
excluded). Stated plainly, isolation does not solve: build artifacts are not
shared between worktrees, so each isolated agent rebuilds from scratch; tool
launched by the agent that resolves absolute paths outside the checkout
(a language server or editor pointed at the main folder) sees a different
directory than the agent's cwd; and the live snapshot deliberately skips
gitignored files, so ignored build outputs are never carried into a worktree.

## Backup & Revert
- Delegated agents are told to copy each existing file they touch into `.kanban/backups/<task_id>/` preserving its repo-relative path.
- Revert spawns a second agent job whose prompt restores every file under that backup dir. Requires existing backups.
- Completing/abandoning a task clears its backups, logs, and session files; abandoning also deletes the task's thread, since the task itself is gone and its id will be reused. The task's `session` field still keeps the id of the session that did the work, even though that session's files are gone.
