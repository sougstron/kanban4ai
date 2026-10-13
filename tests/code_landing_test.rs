mod common;
use kanban4ai::core::{
    code,
    context::ContextManager,
    models::IntegrationState,
    operations::Operations,
    project::{Project, ProjectStore},
    storage::{NewTask, Storage},
    sync::{Journal, install_snapshot},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn config(p: &Project, manual: bool) {
    kanban4ai::core::storage::atomic_write_text(&p.kanban_dir().join("config.yaml"), &format!("notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  isolation:\n    mode: auto\n    land: {}\n", if manual { "manual" } else { "worktree" })).unwrap();
}
struct Pair {
    _temp: tempfile::TempDir,
    a: PathBuf,
    b: PathBuf,
    pa: Project,
    pb: Project,
    remote: PathBuf,
}
fn pair(manual: bool) -> Pair {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::write(
        a.join("config.yaml"),
        "node:\n  name: server\n  role: server\n",
    )
    .unwrap();
    Journal::open(&a).unwrap();
    let work = temp.path().join("wa");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.name", "Test"]);
    git(&work, &["config", "user.email", "test@example.test"]);
    fs::write(work.join("base"), "base\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "base"]);
    let remote = temp.path().join("remote.git");
    git(
        temp.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&work, &["push", "-u", "origin", "main"]);
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let pa = ProjectStore::at(&a).add(&work, None).unwrap().project;
    Storage::new(&pa.data_root).init_board().unwrap();
    config(&pa, manual);
    Journal::open(&b).unwrap();
    install_snapshot(&b, &Journal::open(&a).unwrap().snapshot().unwrap()).unwrap();
    let wb = temp.path().join("wb");
    git(
        temp.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            wb.to_str().unwrap(),
        ],
    );
    git(&wb, &["config", "user.name", "Test"]);
    git(&wb, &["config", "user.email", "test@example.test"]);
    let pb = ProjectStore::at(&b).set_path(&pa.id, &wb).unwrap();
    config(&pb, manual);
    Pair {
        _temp: temp,
        a,
        b,
        pa,
        pb,
        remote,
    }
}
fn ops(p: &Project) -> Operations {
    Operations::for_project_with_launcher(p, Box::new(common::RecordingLauncher::new()))
}
fn sync(a: &Path, b: &Path) {
    let a = Journal::open(a).unwrap();
    let b = Journal::open(b).unwrap();
    let page = a.operations_after(&b.vector().unwrap(), 10000).unwrap();
    b.apply_remote(&page.operations, 300_000).unwrap();
}
fn take(p: &Project, id: &str, session: &str, file: &str) {
    let ops = ops(p);
    ops.take_task(id, session, true).unwrap().unwrap();
    ContextManager::new(&p.data_root)
        .append_context(id, "implemented", "agent", &ops.storage)
        .unwrap();
    fs::write(p.kanban_dir().join("worktrees").join(id).join(file), file).unwrap();
}
#[test]
fn completed_uncommitted_result_survives_cleanup_and_lands_on_peer_without_touching_head_or_index()
{
    let p = pair(false);
    let a = ops(&p.pa);
    let task = a.create_task(NewTask::titled("replicate result")).unwrap();
    sync(&p.a, &p.b);
    let head = git(&p.pb.work_path, &["rev-parse", "HEAD"]);
    let index = git(&p.pb.work_path, &["write-tree"]);
    fs::write(p.pb.work_path.join("human"), "keep").unwrap();
    take(&p.pa, &task.id, "ses-result", "result");
    a.complete_task(&task.id, "ses-result", true).unwrap();
    let run = Journal::open(&p.a)
        .unwrap()
        .runs(&p.pa.id, &task.id)
        .unwrap()
        .into_values()
        .next()
        .unwrap();
    assert!(run.tip.is_some());
    code::reconcile(&p.a).unwrap();
    sync(&p.a, &p.b);
    code::reconcile(&p.b).unwrap();
    assert_eq!(
        fs::read_to_string(p.pb.work_path.join("result")).unwrap(),
        "result"
    );
    assert_eq!(
        fs::read_to_string(p.pb.work_path.join("human")).unwrap(),
        "keep"
    );
    assert_eq!(git(&p.pb.work_path, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&p.pb.work_path, &["write-tree"]), index);
    let task = ops(&p.pb).get_task(&task.id).unwrap().unwrap();
    assert_eq!(task.integration, IntegrationState::Landed);
    assert!(task.landing_pending.is_none());
    code::reconcile(&p.b).unwrap();
}
#[test]
fn offline_duplicate_results_are_retained_on_both_nodes_and_require_review_selection() {
    let p = pair(true);
    let a = ops(&p.pa);
    let task = a.create_task(NewTask::titled("duplicate")).unwrap();
    sync(&p.a, &p.b);
    take(&p.pa, &task.id, "ses-a", "from-a");
    take(&p.pb, &task.id, "ses-b", "from-b");
    // Reconnect while both executors are alive. The first completion changes
    // replicated status to Review before the second local executor finishes.
    sync(&p.a, &p.b);
    sync(&p.b, &p.a);
    a.complete_task(&task.id, "ses-a", true).unwrap();
    sync(&p.a, &p.b);
    ops(&p.pb)
        .complete_task(&task.id, "ses-b", true)
        .unwrap()
        .unwrap();
    code::reconcile(&p.a).unwrap();
    code::reconcile(&p.b).unwrap();
    sync(&p.a, &p.b);
    sync(&p.b, &p.a);
    code::reconcile(&p.a).unwrap();
    code::reconcile(&p.b).unwrap();
    let runs = Journal::open(&p.a)
        .unwrap()
        .runs(&p.pa.id, &task.id)
        .unwrap();
    let mut ordered: Vec<_> = runs.values().collect();
    ordered.sort_by_key(|r| (&r.start_hlc, &r.node_id));
    let canonical = format!("kanban/{}", task.id);
    let other = format!("{canonical}@{}", ordered[1].name);
    for node in [&p.pa, &p.pb] {
        assert_eq!(
            git(&node.work_path, &["rev-parse", &canonical]),
            ordered[0].tip.as_ref().unwrap().as_str()
        );
        assert_eq!(
            git(&node.work_path, &["rev-parse", &other]),
            ordered[1].tip.as_ref().unwrap().as_str()
        );
        let t = ops(node).get_task(&task.id).unwrap().unwrap();
        assert!(t.run_duplicate);
        assert!(t.landing_pending.is_some());
        assert!(
            !node.work_path.join("from-a").exists(),
            "{}: {:?}",
            node.work_path.display(),
            t.integration
        );
        assert!(
            !node.work_path.join("from-b").exists(),
            "{}: {:?}",
            node.work_path.display(),
            t.integration
        );
        ops(node).integrate_received(&task.id, &other).unwrap();
        let root = if node.data_root == p.pa.data_root {
            &p.a
        } else {
            &p.b
        };
        code::reconcile(root).unwrap();
        let journal = Journal::open(root).unwrap();
        assert!(!journal.landings(&node.id, &task.id).unwrap()[&journal.node.node_id].pending);
        assert_eq!(
            git(&node.work_path, &["rev-parse", &canonical]),
            ordered[0].tip.as_ref().unwrap().as_str()
        );
    }
    sync(&p.a, &p.b);
    sync(&p.b, &p.a);
    code::reconcile(&p.a).unwrap();
    code::reconcile(&p.b).unwrap();
    for node in [&p.pa, &p.pb] {
        assert!(
            ops(node)
                .get_task(&task.id)
                .unwrap()
                .unwrap()
                .landing_pending
                .is_none()
        );
    }
    assert_eq!(
        git(&p.remote, &["rev-parse", &other]),
        ordered[1].tip.as_ref().unwrap().as_str()
    );
}
#[test]
fn conflicting_peer_disk_is_untouched_and_keeps_node_specific_landing_pending() {
    let p = pair(false);
    let a = ops(&p.pa);
    let task = a.create_task(NewTask::titled("conflict")).unwrap();
    sync(&p.a, &p.b);
    take(&p.pa, &task.id, "ses-conflict", "base");
    a.complete_task(&task.id, "ses-conflict", true).unwrap();
    code::reconcile(&p.a).unwrap();
    sync(&p.a, &p.b);
    fs::write(p.pb.work_path.join("base"), "human conflict\n").unwrap();
    code::reconcile(&p.b).unwrap();
    assert_eq!(
        fs::read_to_string(p.pb.work_path.join("base")).unwrap(),
        "human conflict\n"
    );
    let t = ops(&p.pb).get_task(&task.id).unwrap().unwrap();
    assert_eq!(t.integration, IntegrationState::Conflict);
    assert!(t.landing_pending.is_some());
    sync(&p.b, &p.a);
    code::reconcile(&p.a).unwrap();
    assert_eq!(
        ops(&p.pa)
            .get_task(&task.id)
            .unwrap()
            .unwrap()
            .landing_pending
            .as_deref(),
        Some("laptop")
    );
}

#[test]
fn unavailable_remote_retains_completed_code_and_retries_after_reconnection() {
    let p = pair(false);
    let a = ops(&p.pa);
    let task = a
        .create_task(NewTask::titled("offline completion"))
        .unwrap();
    sync(&p.a, &p.b);
    take(&p.pa, &task.id, "ses-offline", "offline-result");
    let hidden = p.remote.with_extension("offline");
    fs::rename(&p.remote, &hidden).unwrap();
    a.complete_task(&task.id, "ses-offline", true).unwrap();
    let journal = Journal::open(&p.a).unwrap();
    let run = journal.runs(&p.pa.id, &task.id).unwrap()[&journal.node.node_id].clone();
    let reference = format!(
        "refs/kanban/runs/{}/{}/{}",
        task.id, run.generation, run.node_id
    );
    assert_eq!(
        git(&p.pa.work_path, &["rev-parse", &reference]),
        run.tip.as_ref().unwrap().as_str()
    );
    assert!(code::reconcile(&p.a).is_err());
    // Publication failed, but this node's already successful landing did not.
    assert!(!journal.landings(&p.pa.id, &task.id).unwrap()[&journal.node.node_id].pending);
    fs::rename(&hidden, &p.remote).unwrap();
    code::reconcile(&p.a).unwrap();
    sync(&p.a, &p.b);
    code::reconcile(&p.b).unwrap();
    assert_eq!(
        fs::read_to_string(p.pb.work_path.join("offline-result")).unwrap(),
        "offline-result"
    );
    assert_eq!(
        git(&p.pb.work_path, &["rev-parse", &reference]),
        run.tip.unwrap()
    );
}
