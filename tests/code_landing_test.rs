mod common;
use kanban4ai::core::{
    code,
    context::ContextManager,
    models::IntegrationState,
    operations::Operations,
    project::{Project, ProjectStore},
    storage::{NewTask, Storage},
    sync::Journal,
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
    pair_with_role(manual, "laptop")
}

fn pair_with_role(manual: bool, role: &str) -> Pair {
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
    kanban4ai::core::sync::enable_sync(&a).unwrap();
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
    fs::create_dir_all(&b).unwrap();
    fs::write(
        b.join("config.yaml"),
        format!("node:\n  name: {role}\n  role: {role}\n"),
    )
    .unwrap();
    Journal::open(&b).unwrap();
    kanban4ai::core::sync::enable_sync(&b).unwrap();
    common::bootstrap(&a, &b);
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
fn reconcile_pushes_results_without_holding_the_board_lock() {
    let p = pair(false);
    let a = ops(&p.pa);
    let task = a.create_task(NewTask::titled("unlocked push")).unwrap();
    take(&p.pa, &task.id, "ses-unlocked", "unlocked-result");
    a.complete_task(&task.id, "ses-unlocked", true).unwrap();
    // Completion already published; drop the remote copies so reconcile
    // pushes again while the remote probes the pushing board's lock.
    for reference in git(
        &p.remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/kanban",
            "refs/heads/kanban",
        ],
    )
    .lines()
    {
        git(&p.remote, &["update-ref", "-d", reference]);
    }
    let probe = p.remote.with_extension("probe");
    let hook = p.remote.join("hooks/pre-receive");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nif flock -n '{}' true; then echo free; else echo held; fi >> '{}'\n",
            p.pa.kanban_dir().join(".lock").display(),
            probe.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    code::reconcile(&p.a).unwrap();
    let probes = fs::read_to_string(&probe).unwrap();
    assert!(probes.contains("free"), "{probes}");
    assert!(!probes.contains("held"), "{probes}");
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

fn seed_continuation(p: &Project, id: &str, tip: &str) {
    let storage = Storage::new(&p.data_root);
    let _guard = storage.lock().unwrap();
    let mut task = storage.load_task(id).unwrap().unwrap();
    kanban4ai::core::wip::retire(p, &mut task).unwrap();
    task.continue_from = Some(tip.into());
    storage.save_task(&task).unwrap();
}

#[test]
fn mobile_server_wip_roundtrip_preserves_dirty_files_and_retained_checkouts() {
    let p = pair_with_role(true, "mobile");
    let task = ops(&p.pa)
        .create_task(NewTask::titled("continue WIP"))
        .unwrap();
    sync(&p.a, &p.b);
    take(&p.pb, &task.id, "ses-mobile-wip", "mobile-change");
    let mobile_wt = p.pb.kanban_dir().join("worktrees").join(&task.id);
    let head = git(&mobile_wt, &["rev-parse", "HEAD"]);
    let index = git(&mobile_wt, &["write-tree"]);
    git(&mobile_wt, &["config", "--unset", "user.name"]);
    git(&mobile_wt, &["config", "--unset", "user.email"]);
    let tip =
        kanban4ai::core::wip::checkpoint(&p.pb, &ops(&p.pb).get_task(&task.id).unwrap().unwrap())
            .unwrap();
    assert_eq!(git(&mobile_wt, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&mobile_wt, &["write-tree"]), index);
    assert_eq!(
        git(&p.remote, &["show", &format!("{tip}:mobile-change")]),
        "mobile-change"
    );
    kanban4ai::core::session::SessionManager::new(&p.pb.data_root)
        .close_session("ses-mobile-wip")
        .unwrap();
    seed_continuation(&p.pb, &task.id, &tip);
    sync(&p.b, &p.a);
    ops(&p.pa)
        .take_task(&task.id, "ses-server-wip", true)
        .unwrap()
        .unwrap();
    let server_task = ops(&p.pa).get_task(&task.id).unwrap().unwrap();
    let server_wt =
        p.pa.kanban_dir()
            .join("worktrees")
            .join(server_task.worktree.as_ref().unwrap());
    kanban4ai::core::vcs::populate_worktree(&server_wt).unwrap();
    assert_eq!(
        fs::read_to_string(server_wt.join("mobile-change")).unwrap(),
        "mobile-change"
    );
    fs::write(server_wt.join("server-change"), "server-change").unwrap();
    let second = kanban4ai::core::wip::checkpoint(&p.pa, &server_task).unwrap();
    kanban4ai::core::session::SessionManager::new(&p.pa.data_root)
        .close_session("ses-server-wip")
        .unwrap();
    seed_continuation(&p.pa, &task.id, &second);
    sync(&p.a, &p.b);
    ops(&p.pb)
        .take_task(&task.id, "ses-mobile-return", true)
        .unwrap()
        .unwrap();
    let returned = ops(&p.pb).get_task(&task.id).unwrap().unwrap();
    assert_ne!(returned.worktree.as_deref(), Some(task.id.as_str()));
    let returned_wt =
        p.pb.kanban_dir()
            .join("worktrees")
            .join(returned.worktree.unwrap());
    kanban4ai::core::vcs::populate_worktree(&returned_wt).unwrap();
    assert_eq!(
        fs::read_to_string(returned_wt.join("mobile-change")).unwrap(),
        "mobile-change"
    );
    assert_eq!(
        fs::read_to_string(returned_wt.join("server-change")).unwrap(),
        "server-change"
    );
    assert_eq!(
        fs::read_to_string(mobile_wt.join("mobile-change")).unwrap(),
        "mobile-change"
    );
    let runs = Journal::open(&p.b)
        .unwrap()
        .runs(&p.pb.id, &task.id)
        .unwrap();
    assert_eq!(runs.values().map(|r| r.generation).max(), Some(3));
}

#[test]
fn partitioned_double_continuation_retains_both_seeded_results_for_review() {
    let p = pair(true);
    let task = ops(&p.pa)
        .create_task(NewTask::titled("double continuation"))
        .unwrap();
    sync(&p.a, &p.b);
    take(&p.pa, &task.id, "ses-source-wip", "seeded");
    let tip =
        kanban4ai::core::wip::checkpoint(&p.pa, &ops(&p.pa).get_task(&task.id).unwrap().unwrap())
            .unwrap();
    kanban4ai::core::session::SessionManager::new(&p.pa.data_root)
        .close_session("ses-source-wip")
        .unwrap();
    seed_continuation(&p.pa, &task.id, &tip);
    sync(&p.a, &p.b);
    for (project, session, file) in [
        (&p.pa, "ses-a-continue", "result-a"),
        (&p.pb, "ses-b-continue", "result-b"),
    ] {
        let op = ops(project);
        op.take_task(&task.id, session, true).unwrap().unwrap();
        let current = op.get_task(&task.id).unwrap().unwrap();
        let wt = project
            .kanban_dir()
            .join("worktrees")
            .join(current.worktree.unwrap());
        kanban4ai::core::vcs::populate_worktree(&wt).unwrap();
        assert_eq!(fs::read_to_string(wt.join("seeded")).unwrap(), "seeded");
        fs::write(wt.join(file), file).unwrap();
        op.complete_task(&task.id, session, true).unwrap();
    }
    code::reconcile(&p.a).unwrap();
    code::reconcile(&p.b).unwrap();
    sync(&p.a, &p.b);
    sync(&p.b, &p.a);
    code::reconcile(&p.a).unwrap();
    code::reconcile(&p.b).unwrap();
    for project in [&p.pa, &p.pb] {
        assert!(
            ops(project)
                .get_task(&task.id)
                .unwrap()
                .unwrap()
                .run_duplicate
        );
        let refs = git(
            &project.work_path,
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/heads/kanban/{}@*", task.id),
            ],
        );
        let other = refs.lines().next().unwrap();
        let canonical = format!("refs/heads/kanban/{}", task.id);
        let a = git(&project.work_path, &["ls-tree", "--name-only", &canonical]);
        let b = git(&project.work_path, &["ls-tree", "--name-only", other]);
        assert!(a.contains("seeded") && b.contains("seeded"));
        assert!(
            (a.contains("result-a") && b.contains("result-b"))
                || (a.contains("result-b") && b.contains("result-a"))
        );
    }
}
