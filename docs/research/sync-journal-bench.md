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
