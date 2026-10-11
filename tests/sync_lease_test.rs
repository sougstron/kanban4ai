use kanban4ai::core::storage::{NewTask, Storage, atomic_write_text};
use kanban4ai::core::sync::{Journal, Operation};
use serde_json::json;
use std::fs;

fn ops(journal: &Journal) -> Vec<Operation> {
    journal.state().unwrap().operations.into_values().collect()
}

fn pair(
    block: u64,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Journal,
    Journal,
    Storage,
    Storage,
) {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let ja = Journal::open(a.path()).unwrap();
    let jb = Journal::open(b.path()).unwrap();
    let mut identity = jb.node.clone();
    identity.store_id = ja.node.store_id.clone();
    fs::write(
        b.path().join("sync/node.yaml"),
        serde_yaml_ng::to_string(&identity).unwrap(),
    )
    .unwrap();
    let jb = Journal::open(b.path()).unwrap();
    atomic_write_text(
        &a.path().join("config.yaml"),
        &format!(
            "sync:\n  id_block: {block}\nnode:\n  node_id: {}\n  name: laptop\n  role: laptop\n",
            ja.node.node_id
        ),
    )
    .unwrap();
    let sa = Storage::new(a.path().join("projects/demo"));
    sa.init_board().unwrap();
    atomic_write_text(
        &sa.project_path.join("project.yaml"),
        "id: demo\nname: Demo\ncreated_at: '2026-01-01T00:00:00'\n",
    )
    .unwrap();
    let sb = Storage::new(b.path().join("projects/demo"));
    sb.init_board().unwrap();
    jb.apply_remote(&ops(&ja), 300_000).unwrap();
    (a, b, ja, jb, sa, sb)
}

fn grant(ja: &Journal, jb: &Journal, remaining: u64) -> Vec<Operation> {
    jb.apply_remote(&ops(ja), 300_000).unwrap();
    ja.apply_remote(&ops(jb), 300_000).unwrap();
    jb.apply_remote(&ops(ja), 300_000).unwrap();
    let grants = ja
        .prepare_id_leases(
            "demo",
            &jb.node.node_id,
            &jb.state().unwrap().vector,
            remaining,
        )
        .unwrap();
    jb.apply_remote(&grants, 300_000).unwrap();
    ja.confirm_id_leases(&jb.node.node_id, &jb.state().unwrap().vector)
        .unwrap();
    grants
}

#[test]
fn bootstrap_migrates_live_ids_and_tombstones_then_allocates_disjoint_offline_blocks() {
    let (a, b, ja, jb, sa, sb) = pair(4);
    let old = sa.create_task(NewTask::titled("old")).unwrap();
    sa.delete_task(&old.id).unwrap();
    // A legacy file absent from the journal still raises the bootstrap floor.
    fs::write(sa.tasks_dir.join("archive/TASK-090.md"), "legacy file").unwrap();
    let grants = grant(&ja, &jb, 0);
    assert_eq!(grants[0].set["from"], 91);
    assert_eq!(grants[0].set["to"], 94);
    assert_eq!(grants[1].set["from"], 95);
    let x = sa.create_task(NewTask::titled("offline a")).unwrap();
    let y = sb.create_task(NewTask::titled("offline b")).unwrap();
    assert_eq!(x.id, "TASK-091");
    assert_eq!(y.id, "TASK-095");
    sa.delete_task(&x.id).unwrap();
    sb.delete_task(&y.id).unwrap();
    let reopened_a = Storage::new(a.path().join("projects/demo"));
    let reopened_b = Storage::new(b.path().join("projects/demo"));
    assert_eq!(
        reopened_a
            .create_task(NewTask::titled("a after restart"))
            .unwrap()
            .id,
        "TASK-092"
    );
    assert_eq!(
        reopened_b
            .create_task(NewTask::titled("b after restart"))
            .unwrap()
            .id,
        "TASK-096"
    );
    ja.compact(&[ja.state().unwrap().vector]).unwrap();
    assert_eq!(
        sa.create_task(NewTask::titled("after compact")).unwrap().id,
        "TASK-093"
    );
}

#[test]
fn acknowledgement_is_required_and_lost_responses_do_not_allocate_again() {
    let (_a, _b, ja, jb, sa, sb) = pair(4);
    assert!(
        ja.prepare_id_leases("demo", &jb.node.node_id, &Default::default(), 0)
            .unwrap_err()
            .to_string()
            .contains("VECTOR_UNCONFIRMED")
    );
    let vector = jb.state().unwrap().vector;
    let first = ja
        .prepare_id_leases("demo", &jb.node.node_id, &vector, 0)
        .unwrap();
    assert!(
        sa.create_task(NewTask::titled("unconfirmed"))
            .unwrap_err()
            .to_string()
            .contains("ID_BLOCK_EXHAUSTED")
    );
    let again = ja
        .prepare_id_leases("demo", &jb.node.node_id, &vector, 0)
        .unwrap();
    assert_eq!(
        first.iter().map(|op| &op.op).collect::<Vec<_>>(),
        again.iter().map(|op| &op.op).collect::<Vec<_>>()
    );
    jb.apply_remote(&first, 300_000).unwrap();
    assert_eq!(
        sb.create_task(NewTask::titled("peer applied")).unwrap().id,
        "TASK-005"
    );
    ja.confirm_id_leases(&jb.node.node_id, &jb.state().unwrap().vector)
        .unwrap();
    assert_eq!(
        sa.create_task(NewTask::titled("confirmed")).unwrap().id,
        "TASK-001"
    );
    assert!(
        jb.prepare_id_leases("demo", &ja.node.node_id, &ja.state().unwrap().vector, 0)
            .unwrap_err()
            .to_string()
            .contains("LEASE_AUTHORITY")
    );
}

#[test]
fn prefetch_renews_below_half_and_offline_exhaustion_never_reuses_ids() {
    let (_a, _b, ja, jb, sa, sb) = pair(4);
    grant(&ja, &jb, 0);
    for i in 1..=2 {
        assert_eq!(
            sa.create_task(NewTask::titled("a")).unwrap().id,
            format!("TASK-{i:03}")
        );
    }
    assert!(grant(&ja, &jb, jb.remaining_task_ids("demo").unwrap()).is_empty());
    assert_eq!(sa.create_task(NewTask::titled("a")).unwrap().id, "TASK-003");
    let renewal = grant(&ja, &jb, jb.remaining_task_ids("demo").unwrap());
    assert_eq!(renewal.len(), 1);
    assert_eq!(renewal[0].set["from"], 9);
    assert_eq!(ja.remaining_task_ids("demo").unwrap(), 5);
    for i in [4, 9, 10, 11, 12] {
        let task = sa.create_task(NewTask::titled("a")).unwrap();
        assert_eq!(task.id, format!("TASK-{i:03}"));
        sa.delete_task(&task.id).unwrap();
    }
    assert!(
        sa.create_task(NewTask::titled("empty"))
            .unwrap_err()
            .to_string()
            .contains("ID_BLOCK_EXHAUSTED")
    );
    assert_eq!(
        sb.create_task(NewTask::titled("still offline")).unwrap().id,
        "TASK-005"
    );
    assert_eq!(ja.remaining_task_ids("demo").unwrap(), 0);
    let renewed = grant(&ja, &jb, jb.remaining_task_ids("demo").unwrap());
    assert_eq!(renewed.len(), 1);
    assert_eq!(renewed[0].set["from"], 13);
    assert_eq!(
        sa.create_task(NewTask::titled("after reconnect"))
            .unwrap()
            .id,
        "TASK-013"
    );
}

#[test]
fn overlapping_or_mutable_remote_grants_are_rejected_without_advancing_state() {
    let (_a, _b, ja, jb, _sa, _sb) = pair(4);
    let original = grant(&ja, &jb, 0);
    let before = serde_json::to_value(jb.state().unwrap()).unwrap();
    let mut bad = original[0].clone();
    let sequence = ja.state().unwrap().vector[&ja.node.node_id] + 1;
    bad.op = format!("{}:{sequence}", ja.node.node_id);
    bad.id = bad.op.clone();
    bad.hlc.1 += 1;
    assert!(
        jb.apply_remote(&[bad.clone()], 300_000)
            .unwrap_err()
            .to_string()
            .contains("LEASE_CONFLICT")
    );
    assert_eq!(serde_json::to_value(jb.state().unwrap()).unwrap(), before);
    bad.set.insert("to".into(), json!(0));
    assert!(jb.apply_remote(&[bad], 300_000).is_err());
    assert_eq!(serde_json::to_value(jb.state().unwrap()).unwrap(), before);
}

#[test]
fn allocation_is_consumed_durably_even_when_the_task_is_not_written() {
    let (a, _b, ja, jb, sa, _sb) = pair(2);
    grant(&ja, &jb, 0);
    assert_eq!(sa.get_next_id().unwrap(), "TASK-001");
    drop(sa);
    let storage = Storage::new(a.path().join("projects/demo"));
    assert_eq!(
        storage
            .create_task(NewTask::titled("restart after reservation"))
            .unwrap()
            .id,
        "TASK-002"
    );
    assert!(storage.get_next_id().is_err());
}

#[test]
fn unpaired_legacy_boards_keep_max_plus_one_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    let task = storage.create_task(NewTask::titled("legacy")).unwrap();
    storage.delete_task(&task.id).unwrap();
    assert_eq!(storage.get_next_id().unwrap(), "TASK-001");
    assert!(!dir.path().join("sync").exists());
}

#[test]
fn deleted_highest_id_remains_closed_when_migrating_to_leases() {
    let (_a, _b, ja, jb, sa, _sb) = pair(4);
    let task = sa.create_task(NewTask::titled("highest")).unwrap();
    sa.delete_task(&task.id).unwrap();
    ja.compact(&[ja.state().unwrap().vector]).unwrap();
    let grants = grant(&ja, &jb, 0);
    assert_eq!(grants[0].set["from"], 2);
    assert_eq!(
        sa.create_task(NewTask::titled("migrated")).unwrap().id,
        "TASK-002"
    );
}

#[test]
fn concurrent_writers_consume_each_number_once() {
    let (_a, _b, ja, jb, sa, _sb) = pair(12);
    grant(&ja, &jb, 0);
    let handles: Vec<_> = (0..12)
        .map(|_| {
            let root = sa.project_path.clone();
            std::thread::spawn(move || {
                Storage::new(root)
                    .create_task(NewTask::titled("concurrent"))
                    .unwrap()
                    .id
            })
        })
        .collect();
    let mut ids: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        (1..=12)
            .map(|id| format!("TASK-{id:03}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(ja.remaining_task_ids("demo").unwrap(), 0);
}
