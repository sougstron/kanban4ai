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

## Database

The bar for leaving the store in files was a task write under about 10 ms and
`operations_after` under about 5 ms at 10k ops. Format 2 misses both (112 ms
and 29–38 ms) when the projection is not checkpointed. Do not add SQLite or
redb in this change. TASK-513 waits.

Before choosing an engine, remeasure with peers configured (a snapshot must
not solo-prune the page under test) and with an in-process projection cache
keyed by `change_token`, or a per-project shard. Parsing `projection.json` or
replaying every segment is the likely cost of the write and the empty page.
If that run is still above the bar, compare SQLite and redb as its own task.
