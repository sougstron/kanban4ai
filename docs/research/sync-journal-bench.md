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

Filled in after the segment journal lands. The acceptance bar for staying
out of a database is a task write under about 10 ms and `operations_after`
under about 5 ms at 10k ops. Above that, SQLite versus redb is a separate task.
