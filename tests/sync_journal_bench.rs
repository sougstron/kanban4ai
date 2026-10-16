//! Before/after timings for the sync journal. Ignored: it seeds 10k operations
//! and prints medians. The public API it calls exists on both journal layouts.
//!
//! ```sh
//! cargo test --release --test sync_journal_bench -- --ignored --nocapture
//! ```
use kanban4ai::core::storage::{NewTask, Storage};
use kanban4ai::core::sync::{Hlc, Journal, Operation, VersionVector};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_bytes(&path);
        } else {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

fn median(samples: &mut [u128]) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

struct Seed<'a> {
    node: &'a str,
    store: &'a str,
    project: &'a str,
    id: &'a str,
    wall: u64,
}

fn remote_op(
    seed: &Seed<'_>,
    seq: u64,
    field: &str,
    value: Value,
    prev: Option<&Hlc>,
) -> Operation {
    let mut prev_map = BTreeMap::new();
    if let Some(clock) = prev {
        prev_map.insert(field.into(), clock.clone());
    }
    let node = seed.node;
    Operation {
        v: 1,
        op: format!("{node}:{seq}"),
        hlc: Hlc(seed.wall + seq, 0, node.into()),
        store: seed.store.into(),
        project: seed.project.into(),
        entity: "task".into(),
        id: seed.id.into(),
        set: BTreeMap::from([(field.into(), value)]),
        prev: prev_map,
        actor: "human".into(),
        extra: BTreeMap::new(),
    }
}

#[test]
#[ignore = "release benchmark; prints medians for one task write and ops_after"]
fn journal_write_and_page_at_10k() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("projects/demo");
    let storage = Storage::new(&board);
    storage.init_board().unwrap();
    fs::write(
        board.join("project.yaml"),
        "id: demo\nname: demo\ncreated_at: '2026-01-01T00:00:00'\nwork_path: ''\n",
    )
    .unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let mut config = fs::read_to_string(dir.path().join("config.yaml")).unwrap_or_default();
    // A peer keeps the snapshot from solo-pruning the page under test.
    // snapshot_ops stays at the default scale so the warm path reads the
    // projection cache instead of replaying every segment.
    config.push_str(
        "\nsync:\n  peers:\n    - {name: bench-peer}\n  journal:\n    segment_bytes: 4194304\n    snapshot_ops: 1000\n    snapshot_secs: 300\n",
    );
    fs::write(dir.path().join("config.yaml"), config).unwrap();
    kanban4ai::core::sync::enable_sync(dir.path()).unwrap();
    let mut task = storage.create_task(NewTask::titled("seed")).unwrap();
    let state = journal.state().unwrap();
    let project = state.project_uids.get("demo").expect("project uid").clone();
    let task_uid = state.task_uids.values().next().expect("task uid").clone();
    let store_id = journal.node.store_id.clone();
    // `legacy` reproduces the TASK-511 seed: remote clocks from 2023 with no
    // `prev`, so every remote edit loses to the local creation and the seed
    // carries ~3 000 conflicts and their thread notes. The default seed is
    // causal: peers saw the creation, and later edits build on the last one.
    let legacy = std::env::var("BENCH_SEED").is_ok_and(|seed| seed == "legacy");
    let created = state
        .fields
        .values()
        .find(|field| field.clocks.contains_key("title"))
        .expect("task fields")
        .clocks
        .clone();
    let wall = if legacy {
        1_700_000_000_000
    } else {
        created["title"].0
    };
    let local_target = std::env::var("BENCH_LOCAL")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4_000);
    let remote_each = std::env::var("BENCH_REMOTE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3_000);
    let mut last_b = None;
    for (node, field) in [("peer-b", "description"), ("peer-c", "priority")] {
        let mut batch = Vec::new();
        let mut prev = if legacy {
            None
        } else {
            created.get(field).cloned()
        };
        for seq in 1..=remote_each {
            let value = if field == "priority" {
                Value::from(seq)
            } else {
                Value::String(format!("{node}-{seq}"))
            };
            let op = remote_op(
                &Seed {
                    node,
                    store: &store_id,
                    project: &project,
                    id: &task_uid,
                    wall,
                },
                seq,
                field,
                value,
                prev.as_ref(),
            );
            prev = Some(op.hlc.clone());
            batch.push(op);
            if batch.len() == 200 {
                journal
                    .apply_remote(&std::mem::take(&mut batch), u64::MAX)
                    .unwrap();
            }
        }
        if !batch.is_empty() {
            journal.apply_remote(&batch, u64::MAX).unwrap();
        }
        if node == "peer-b" {
            last_b = prev;
        }
    }
    if !legacy {
        task = storage.load_task(&task.id).unwrap().unwrap();
    }
    for n in 0..local_target {
        task.title = format!("local-{n}");
        storage.save_task(&task).unwrap();
    }
    let seeded = journal.state().unwrap();
    let total: u64 = seeded.vector.values().sum();
    eprintln!("seeded vector sum {total} entries {}", seeded.vector.len());

    let rounds = 7usize;
    let mut writes = Vec::with_capacity(rounds);
    for n in 0..rounds {
        task.title = format!("measure-{n}");
        let started = Instant::now();
        storage.save_task(&task).unwrap();
        writes.push(started.elapsed().as_micros());
    }
    let state = journal.state().unwrap();
    let mut middle = VersionVector::new();
    for (node, seq) in &state.vector {
        middle.insert(node.clone(), seq / 2);
    }
    let mut page_mid = Vec::with_capacity(rounds);
    let mut page_empty = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        let page = journal.operations_after(&middle, 200).unwrap();
        page_mid.push(started.elapsed().as_micros());
        let expect = 200.min(total.saturating_sub(total / 2) as usize);
        assert!(
            page.operations.len() >= expect.min(1) || total < 2,
            "mid page {}",
            page.operations.len()
        );
        let started = Instant::now();
        let page = journal.operations_after(&state.vector, 200).unwrap();
        page_empty.push(started.elapsed().as_micros());
        assert!(page.operations.is_empty(), "tail page");
    }
    let mut apply_one = Vec::with_capacity(rounds);
    let mut cursor = remote_each;
    for n in 0..rounds {
        cursor += 1;
        let op = remote_op(
            &Seed {
                node: "peer-b",
                store: &store_id,
                project: &project,
                id: &task_uid,
                wall,
            },
            cursor,
            "description",
            Value::String(format!("one-{n}")),
            if legacy { None } else { last_b.as_ref() },
        );
        last_b = Some(op.hlc.clone());
        let started = Instant::now();
        journal.apply_remote(&[op], u64::MAX).unwrap();
        apply_one.push(started.elapsed().as_micros());
    }
    // Cold path: a fresh CLI process per call, as agents run it. Nothing is
    // cached in memory; each call loads the checkpoint and replays its tail.
    let cli = |args: &[&str]| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_kanban4ai"));
        cmd.env("KANBAN_HOME", dir.path()).args(args);
        for (key, _) in std::env::vars() {
            if key.starts_with("KANBAN_") && key != "KANBAN_HOME" {
                cmd.env_remove(key);
            }
        }
        let started = Instant::now();
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        started.elapsed().as_micros()
    };
    let id = task.id.clone();
    let mut cold_start = Vec::with_capacity(rounds);
    let mut cold_read = Vec::with_capacity(rounds);
    let mut cold_write = Vec::with_capacity(rounds);
    for n in 0..rounds {
        cold_start.push(cli(&["--version"]));
        cold_read.push(cli(&["--project", "demo", "show", &id]));
        let note = format!("cold-{n}");
        cold_write.push(cli(&[
            "--project",
            "demo",
            "context",
            &id,
            &note,
            "--source",
            "agent",
        ]));
    }
    eprintln!(
        "bench cold_start_us={} cold_show_us={} cold_context_us={}",
        median(&mut cold_start),
        median(&mut cold_read),
        median(&mut cold_write),
    );
    let conflicts = fs::read_to_string(dir.path().join("sync/meta/conflicts.jsonl"))
        .map(|text| text.lines().count())
        .unwrap_or(0);
    let thread_bytes = fs::metadata(board.join(".kanban/threads").join(format!("{id}.yaml")))
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!("bench conflicts={conflicts} thread_bytes={thread_bytes}");
    let meta = dir_bytes(&dir.path().join("sync/meta"));
    let journal_bytes = dir_bytes(&dir.path().join("sync/journal"));
    eprintln!(
        "bench write_us={} page_mid_us={} page_empty_us={} apply_one_us={} meta_bytes={meta} journal_bytes={journal_bytes}",
        median(&mut writes),
        median(&mut page_mid),
        median(&mut page_empty),
        median(&mut apply_one),
    );
}
