# Sync journal benchmark

Measurements for the store journal before and after format 2 (append-only
segments). Both runs use the ignored test `journal_write_and_page_at_10k`:

```sh
TMPDIR=$PWD/target/bench-tmp cargo test --release --test sync_journal_bench -- --ignored --nocapture
```

Seed is one project and one task, then 3 000 description ops from `peer-b`,
3 000 `priority` ops from `peer-c` (batches of 200), then 4 000 local
`save_task` title updates. The reported vector sum is 10 006. Each figure is
the median of 7 rounds on this machine.

## Before (v1, full `meta/state.json` rewrite)

| measurement | median |
|---|---|
| one `save_task` | 222.5 ms |
| `operations_after` from the middle, limit 200 | 43.4 ms |
| `operations_after` at the tip (empty page) | 28.4 ms |
| `apply_remote` of one op | 1 805 ms |
| `sync/meta` | 5 763 185 bytes |
| `sync/journal` | 3 390 860 bytes |

The empty page still parses and sorts every stored operation. One remote op
rewrites `state.json` and every projected board file under the store lock.

## After (format 2)

Same command, same seed, release build, median of 7. The bench raises
`snapshot_ops` and `snapshot_secs` so a solo store does not prune the history
the mid-page cursor still has to read. That also skips `projection.json`, so
every `state()` replays the segments.

| measurement | v1 | format 2 |
|---|---|---|
| one `save_task` | 222.5 ms | 111.6 ms |
| `operations_after` from the middle, limit 200 | 43.4 ms | 38.0 ms |
| `operations_after` at the tip (empty page) | 28.4 ms | 28.6 ms |
| `apply_remote` of one op | 1 805 ms | 1 670 ms |
| `sync/meta` | 5 763 185 bytes | 2 092 771 bytes |
| `sync/journal` | 3 390 860 bytes | 3 450 980 bytes |

The write no longer rewrites a growing `state.json`, and `sync/meta` is about
a third of the v1 size. It is still a full replay: the empty page costs the
same 29 ms as v1, so `operations_after` is not yet O(page) on this path.
`apply_remote` of one op is still about 1.7 s. A separate prune test (40
acked batches, small segments) keeps `sync/journal` + `sync/meta` under a
fixed cap; this bench does not ack peers, so the journal stays linear.

## Warm path (peers, checkpoint, in-process cache)

Same seed and the same release binary, median of 7. The bench lists a peer and
leaves `snapshot_ops` at 1000, so a checkpoint does not solo-prune the page.
The process keeps the projection and the conflict groups in memory, keyed by
`change_token`. `operations_after` merges segment cursors and stops at the
page. A repeated run on this disk landed in the same band (write 35–50 ms,
empty page about 1 ms, one remote op about 0.8 s).

| measurement | v1 | format 2, no checkpoint | warm path |
|---|---|---|---|
| one `save_task` | 222.5 ms | 111.6 ms | 34.7 ms |
| `operations_after` from the middle, limit 200 | 43.4 ms | 38.0 ms | 1.7 ms |
| `operations_after` at the tip (empty page) | 28.4 ms | 28.6 ms | 1.1 ms |
| `apply_remote` of one op | 1 805 ms | 1 670 ms | 776 ms |
| `sync/meta` | 5 763 185 bytes | 2 092 771 bytes | 2 119 015 bytes |
| `sync/journal` | 3 390 860 bytes | 3 450 980 bytes | 3 450 980 bytes |

Phase medians for one `save_task` after the 10k seed: intent 7.9 ms, task
file 7.5 ms, journal append 4.1 ms, drift index 3.8 ms, conflict-group update
4.5 ms. The snapshot counter (`since.json`) is no longer fsynced. At ~100 ops
the same write is about 24 ms, so the floor is the fsyncs, not the 10k replay.
One fsync of a small file on this btrfs is about 4–7 ms.

`apply_remote` of one op spends about 470 ms in the conflict walk over the
retained edits of that field, about 90 ms projecting the touched task, and
about 190 ms in the redo fsyncs (journal, task file, HLC, drift,
`remote-pending.json`). The page path does not do that work.

## Database

The bar for leaving the store in files was a task write under about 10 ms and
`operations_after` under about 5 ms at 10k ops.

`operations_after` passes (about 1–2 ms). The write does not. It is three or
four durability barriers (intent, the task file, the journal, the drift
index), and each barrier is its own fsync. SQLite or redb would beat that
only by committing the canonical edit in one WAL fsync and treating the
markdown file as a cache repaired from that commit. Putting a database under
the journal while still fsyncing the task file does not reach 10 ms.

Do not add a database in this change. TASK-513 should not block on an engine
choice: the file journal is enough for backup. A later task can pass the write
bar either by one journal fsync plus crash repair of the task file from the
projection, or by a single-commit store. That choice is not justified by the
page time.

## TASK-516: causal seed, cold CLI, real store

The TASK-511 seed gave the remote ops 2023 clocks and no `prev`, so all
3 000 `peer-b` edits lost to the local task creation. That run carried 3 007
conflicts, a 1.3 MB thread of conflict notes and a 1.2 MB `uids.json`. The
"~470 ms conflict walk" was mostly the cost of that artifact. `BENCH_SEED=legacy`
still reproduces it. The default seed is now causal: peers build on the
creation and on their own last edit. The local writer reloads the task first.
The bench also runs the release binary as a fresh process per call (`show`
and `context`), the way agents call it.

Pair search in `prune` now uses entry/exit intervals of the `prev` forest. An
ancestor check is O(1), and only concurrent pairs are visited. Before, each
pair walked the chain.

| measurement, 10k ops, median of 7 | legacy seed | causal seed |
|---|---|---|
| one `save_task` (warm) | 57.6 ms | 43.8–49.8 ms |
| `operations_after` mid / empty | 1.7 / 1.6 ms | 1.1 / 0.6 ms |
| `apply_remote` of one op | 311 ms (was 735–776) | 61–63 ms |
| process start (`--version`) | 2.9 ms | 2.3–2.4 ms |
| cold `show` | 28 ms | 20–22 ms |
| cold `context` (two captures) | 530 ms | 107–115 ms |
| conflicts / thread | 3 007 / 1.3 MB | 0 / 2 KB |

On tmpfs the same cold `context` takes 15–25 ms. On disk the remainder is
about 17 fsyncs: two captures × (intent, data file, journal, drift, directory
syncs). That is the same floor as the warm write.

### The real store is the scale problem

The bench holds one task. A copy of the maintainer's store (about 1 800 tasks,
36 boards, one peer configured, so sync is on) on the same btrfs disk gives a
different picture:

| step | time |
|---|---|
| first CLI call after an upgrade from 0.7.6 (no `drift.jsonl` yet) | about 20 min, all board locks held |
| `meta/projection.json` afterwards | 38.7 MB (journal 24 MB, blobs 23 MB) |
| `list` | 50 ms |
| `show TASK-511` | about 520 ms |
| `context` | about 1.6 s |

Where `show` goes (strace): about 350 ms is the startup `reconcile` stat of
every board file, and about 320 ms is each parse of the whole-store
`projection.json`. `remote_runs` needs it for a single task. `context` pays
the same parse plus two captures. The projection holds every field of every
task and message, so each CLI process reads the whole store to touch one task.

The append-only journal holds up at this size. The single whole-store
projection and the per-process full reconcile do not. Either the projection is
sharded (per board, or per entity, loaded lazily), or this is where SQLite/redb
earns its place: point reads of one task without parsing the store. The first
reconcile after an upgrade also needs a batched path (one state, no per-file
clone and checkpoint) before 0.8.x reaches a store with peers.

## TASK-516: projection sharded by task (store format 3)

`meta/projection.json` is now `meta/projection/`: `head.json` (frontier,
compaction, prune state), one `tasks/<hash>.json` per task (the task, its
run, thread and messages, bodies included), and `misc.json` for everything
else. A cold CLI process that touches one task (capture of a task, thread or
session file, `remote_runs` for `show`, landing marks) reads the head,
`misc.json`, that task's shard and the operations after the head that can
touch it. Message bodies stay only in their task's shard, so a command on one
task no longer parses the rest of the store's threads.

Synthetic store, 1 800 tasks × 25 messages (a 54 MB format 2 file), median of
5 on this btrfs disk, `projection_scale` in `projection.rs`:

```sh
TMPDIR=$PWD/target/bench-tmp CARGO_PROFILE_DEV_OPT_LEVEL=3 CARGO_TARGET_DIR=target/opt \
  cargo test --lib projection_scale -- --ignored --nocapture
```

| measurement | format 2 | format 3 |
|---|---|---|
| state for one task (cold process) | 220–231 ms (whole file) | 0.1 ms (one 30 KB shard) |
| whole-store load (serve/TUI, apply, prune) | 220–231 ms | 223–239 ms |
| regular checkpoint | 153–158 ms (whole file) | 87–138 ms (dirty shards + head) |
| first checkpoint after upgrade | — | 2.6 s once (1 800 shards, 8 fsync writers; 12 s serial) |

A scoped state is never cached or checkpointed. When a checkpoint is due
(`snapshot_ops`/`snapshot_secs`), that one write loads the whole store once,
so a cold writer pays the old cost about every 1 000 operations or 5 minutes
instead of on every call. The startup `reconcile` stat walk (~350 ms on the
real store) is unchanged and is the next cost of a cold `show`.

Store format 3 fences 0.8.x binaries off (`SYNC_STORE_TOO_NEW`), including a
format 2 server receiving a format 3 bootstrap snapshot: the server node must
be upgraded with the laptop.
