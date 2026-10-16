# Store backups

`kanban backup` writes a consistent snapshot of the store. A sync replica is
not a backup: deletes replicate, and a peer that has already accepted a
tombstone will not grow the task back from ordinary field updates. Restore in
board mode puts the files back and `sync::activate` records an explicit
`restore` operation, which wins over that tombstone.

The snapshot logic lives in `src/core/backup.rs`. The CLI is a thin wrapper.

## What is included

Core archive, always:

- `<store>/config.yaml`
- `projects/*/project.yaml` and `project.yaml.removed`
- each board `.kanban/`: `tasks/**`, `threads/**`, `config.yaml`,
  `instructions/**`, `assets/images/**`, `context/**`, `stats/**`
- `sync/` when it exists (a store with sync off has none, and that is not an
  error): `FORMAT`, `hlc`, `node.yaml`, `peers.yaml`, `tokens.yaml`,
  `exchanges.yaml`, `landing-intent.json`, `drift.jsonl`, `journal/**`,
  `meta/**`, `relay/**`, `legacy-v1/**`, `blobs/**`

`drift.jsonl` and `blobs/` are included so a full restore can reconcile.
Without the drift index every file looks new; without the blobs a real
difference cannot name the previous bytes.

Artifacts archive, separate file, same timestamp: `.kanban/assets/**` except
`images/`. Automatic snapshots add it when `backup.artifacts` is `weekly`
(no artifacts archive younger than 7 days) or `always` (every due core
snapshot). `off` never writes one. `kanban backup create --with-artifacts`
writes one immediately.

Never included, including paths that exist on a live store but are not part
of the board:

- `logs/`, `worktrees/`, `backups/`, `sessions/`, `detached/`, `provenance/`,
  `forms/`, `wip/`, `code/`, `runtime/`, `net/`
- `*.lock`, `presence.yaml`, `pair-code.yaml`, `update-status.json`
- `limits.json`, `limits-history.jsonl`, `limits-refresh-cwd/`, provider
  limit caches (`claude-*`, `codex-*`)
- `daemon.lock`, `pump.lock`, `.no-board/`, `recent_models`,
  `runner-control.lock`

Symlinks are skipped, not followed.

## Locks and archive format

Core files are copied while locks are held in this order: store
(`<store>/.lock`), every board (`.kanban/.lock`, sorted by project id), then
`sync/.lock` when `sync/` exists. The copy is a real copy into
`<backup.dir>/.staging-<stamp>/` (hard links would break, because the board
replaces files by rename). Locks are dropped before gzip and hashing.

Archives:

- `<backup.dir>/kanban-<YYYYMMDD-HHMMSS>[-N]-<kind>.tar.gz`
- artifacts: `kanban-<YYYYMMDD-HHMMSS>[-N]-<kind>-artifacts.tar.gz`

`kind` is `manual`, `daily`, or `pre-restore`. The first tar entry is
`MANIFEST.json` (`backup_format` 1): version, sync format, time, node id,
projects (id, name, `data_root` relative to the store), and each file's size,
sha256, and mode. Paths inside the archive are `store/...` and
`boards/<project-id>/.kanban/...`.

The file is written to `*.partial`, fsynced, renamed, and the directory is
fsynced. Archives are mode 0600, the directory is 0700, and secret files keep
their mode. A backup directory inside the store is refused
(`BACKUP_DIR_INSIDE_STORE`).

Default directory: `$XDG_STATE_HOME/kanban4ai/backups`, or
`~/.local/state/kanban4ai/backups`. It is outside the store so removing the
store does not remove snapshots. `KANBAN_HOME` does not move this default;
tests and scratch stores pass `--dir` or set `backup.dir`.

## Restore

`kanban backup restore` verifies the archive first (hashes, task/thread/config
parsing, `sync_format` not newer than this binary). A failure leaves the
store untouched (`BACKUP_VERIFY_FAILED`, `BACKUP_FORMAT_NEWER`). An older sync
format goes through the normal open migrations.

Then:

1. Write a `pre-restore` core snapshot. Retention keeps it for 7 days.
2. Extract into `<store>/.restore-<stamp>/` on the same filesystem and parse
   again.
3. Under the same lock order, rename current targets aside and move the
   staged tree into place. Any error renames them back. The aside copy is
   removed only after success.
4. Drop the locks and call `sync::activate`. Restored files become local
   operations. A task that still has a tombstone gets `entity: restore` with
   a newer HLC.

Modes:

- `board` (default). Replace board files, `project.yaml`, and the global
  config except machine-local sections (`backup` included). `sync/` is not
  touched. `work_path`, `migrated_from`, and `last_opened_at` on an existing
  `project.yaml` stay as they are on this machine. `--project ID` restores
  one board and does not touch the global config or other boards.
- `full`. Disaster recovery, including `sync/`. Refused when the destination
  already has tasks unless `--force` is set. Identity files
  (`node.yaml`, `peers.yaml`, `tokens.yaml`) and the `node` config section
  are not restored unless `--identity` is passed. Without that flag the
  store gets a new node id and peers must pair again. `--identity` prints a
  warning: two live machines with one node id break HLC/LWW.

An artifacts archive is restored only when the path is that archive. Files
are copied into place; extra files on disk are left alone. A core
`pre-restore` snapshot is still taken.

Without a terminal, `--yes` is required. Active agent sessions on a board
being restored refuse the command (`BACKUP_ACTIVE_SESSIONS`) unless
`--force`.

## Retention

`select_keep` is a pure function. Core and artifacts are selected separately.

- `daily`: the newest `backup.keep_daily` daily archives (default 7).
- `weekly`: the newest daily or manual archive in each of the last
  `backup.keep_weekly` ISO weeks (default 4).
- `monthly`: the newest daily or manual archive in each of the last
  `backup.keep_monthly` calendar months (default 6).
- `manual`: the newest `backup.keep_manual` manual archives (default 10).
- `pre-restore`: 7 days.

The size cap (`backup.max_total_mb`, default 2048) then drops the oldest
archives until core plus artifacts fit, artifacts first. The newest core
archive that passed verify is never deleted. `kanban backup prune` does this,
and so does every create. `.partial` and `.staging-*` older than one hour are
removed.

## Automatic snapshots

`backup.auto` (default true) runs from the store daemon tick, off the tick
thread, at `backup.interval_hours` (default 24). A try-lock on
`<backup.dir>/.lock` skips the run when another process is already snapshotting.
A failure is logged once and does not fail the tick. Stores under the system
temp directory are skipped so tests do not write into the user state dir.
The TUI pump uses the same tick.

## Lock-hold measurement

Locks are held only for the copy. On the live store (2026-10-16) that copy
took the time recorded below; compression happens afterwards.

Measured locked copy: see the number filled in after the timing run.
