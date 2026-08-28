//! Worktree isolation (TASK-236, TASK-247): a task's agent runs in its own
//! git worktree cut from a live snapshot of the work folder, so two agents
//! physically cannot overwrite each other's files.

mod common;

use common::RecordingLauncher;
use kanban4ai::core::context::ContextManager;
use kanban4ai::core::models::{IntegrationState, MessageKind, Role, TaskStatus};
use kanban4ai::core::operations::{LandOutcome, Operations};
use kanban4ai::core::project::{ProjectStore, Roots};
use kanban4ai::core::storage::{NewTask, Storage};
use kanban4ai::core::vcs::{self, INTEGRATION_REF};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const COLUMNS: &str = "columns:\n- name: To Do\n  id: todo\n- name: In Progress\n  id: in_progress\n- name: Review\n  id: review\n- name: Done\n  id: done\nnotifications:\n  enabled: false\nauto_launch:\n  enabled: true\n";

fn write_config(data_root: &Path, orchestration: &str) {
    let config = format!("{COLUMNS}orchestration:\n{orchestration}\n");
    fs::write(data_root.join(".kanban/config.yaml"), config).expect("write config");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Registered project whose work folder is a git repo with one commit and
/// live uncommitted changes — the reviewed starting point for a snapshot.
fn git_project_ops(
    orchestration: &str,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    PathBuf,
    Operations,
    RecordingLauncher,
) {
    let store = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    git(work.path(), &["init", "-q", "-b", "main"]);
    git(
        work.path(),
        &["config", "user.email", "kanban@example.test"],
    );
    git(work.path(), &["config", "user.name", "Kanban Test"]);
    fs::write(work.path().join("committed.txt"), "committed\n").unwrap();
    git(work.path(), &["add", "-A"]);
    git(work.path(), &["commit", "-q", "-m", "base"]);
    fs::write(work.path().join("live.txt"), "uncommitted feature work\n").unwrap();

    let project = ProjectStore::at(store.path())
        .add(work.path(), None)
        .unwrap()
        .project;
    Storage::new(&project.data_root).init_board().unwrap();
    write_config(&project.data_root, orchestration);
    let recorder = RecordingLauncher::new();
    let ops = Operations::for_project_with_launcher(&project, Box::new(recorder.clone()));
    let work_path = work.path().to_path_buf();
    (store, work, work_path, ops, recorder)
}

fn worktree_dir(project_data_root: &Path, task_id: &str) -> PathBuf {
    project_data_root.join(".kanban/worktrees").join(task_id)
}

/// False when git refuses (e.g. a ref that does not exist).
fn git_ref_exists(dir: &Path, rev: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", rev])
        .stdin(Stdio::null())
        .output()
        .expect("git spawn")
        .status
        .success()
}

/// Take `task` with an isolated launch and give it recorded context, the
/// minimum an agent `done` needs.
fn take_and_context(ops: &Operations, task_id: &str, session: &str) {
    ops.take_task(task_id, session, true).unwrap().unwrap();
    ContextManager::new(ops.data_root())
        .append_context(task_id, "implemented and tested", "agent", &ops.storage)
        .unwrap();
}

#[test]
fn isolated_launch_snapshots_live_work_and_points_the_agent_at_it() {
    let (_store, _work, work_path, ops, recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    let task = ops.create_task(NewTask::titled("Isolated")).unwrap();

    ops.take_task(&task.id, "ses-iso", true).unwrap().unwrap();

    let taken = ops.storage.load_task(&task.id).unwrap().unwrap();
    let rel = taken.worktree.as_deref().expect("worktree recorded");
    assert_eq!(rel, task.id);
    assert_eq!(taken.branch.as_deref(), Some("kanban/TASK-001"));

    let wt = worktree_dir(ops.data_root(), &task.id);
    assert!(wt.is_dir(), "worktree directory exists");
    // The live snapshot carried the uncommitted feature work.
    assert_eq!(
        fs::read_to_string(wt.join("live.txt")).unwrap(),
        "uncommitted feature work\n"
    );
    assert_eq!(
        fs::read_to_string(wt.join("committed.txt")).unwrap(),
        "committed\n"
    );
    // The branch tip's base is the stored snapshot commit, and the
    // integration ref points at it.
    let base = taken.base_commit.clone().unwrap();
    let repo = vcs::detect(&work_path).unwrap();
    assert_eq!(
        repo.read_ref(INTEGRATION_REF).unwrap().unwrap().as_str(),
        base
    );
    // The user's repo is untouched: HEAD still at the committed base, the
    // live work still uncommitted.
    assert_ne!(git(&work_path, &["rev-parse", "HEAD"]), base);
    assert!(git(&work_path, &["status", "--porcelain"]).contains("?? live.txt"));
    // The agent's world is the worktree.
    assert_eq!(recorder.roots().len(), 1);
    assert_eq!(recorder.roots()[0].0, ops.data_root().to_path_buf());
    assert_eq!(recorder.roots()[0].1, wt);
    assert!(recorder.roots()[0].2.is_some(), "KANBAN_PROJECT stays set");
}

#[test]
fn two_task_snapshots_chain_with_the_snapshot_as_merge_base() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");

    let first = ops.create_task(NewTask::titled("First")).unwrap();
    ops.take_task(&first.id, "ses-one", true).unwrap().unwrap();
    fs::write(work_path.join("live.txt"), "feature grew\n").unwrap();
    let second = ops.create_task(NewTask::titled("Second")).unwrap();
    ops.take_task(&second.id, "ses-two", true).unwrap().unwrap();

    let first_base = ops
        .storage
        .load_task(&first.id)
        .unwrap()
        .unwrap()
        .base_commit
        .unwrap();
    let second_base = ops
        .storage
        .load_task(&second.id)
        .unwrap()
        .unwrap()
        .base_commit
        .unwrap();
    assert_ne!(first_base, second_base);
    let merge_base = git(&work_path, &["merge-base", &first_base, &second_base]);
    assert_eq!(
        merge_base, first_base,
        "the later snapshot descends from the earlier one, not from HEAD"
    );
    let head = git(&work_path, &["rev-parse", "HEAD"]);
    assert_ne!(merge_base, head);
    // The second worktree saw the newer live content.
    let wt2 = worktree_dir(ops.data_root(), &second.id);
    assert_eq!(
        fs::read_to_string(wt2.join("live.txt")).unwrap(),
        "feature grew\n"
    );
}

#[test]
fn seed_head_branches_from_committed_head_and_skips_the_ref() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n    seed: head\n");
    let task = ops.create_task(NewTask::titled("Head seeded")).unwrap();

    ops.take_task(&task.id, "ses-head", true).unwrap().unwrap();

    let taken = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(
        taken.base_commit.unwrap(),
        git(&work_path, &["rev-parse", "HEAD"])
    );
    let repo = vcs::detect(&work_path).unwrap();
    assert_eq!(repo.read_ref(INTEGRATION_REF).unwrap(), None);
}

#[test]
fn unregistered_board_never_isolates() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "kanban@example.test"]);
    git(dir.path(), &["config", "user.name", "Kanban Test"]);
    fs::write(dir.path().join("f.txt"), "x\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "base"]);
    let storage = Storage::new(dir.path());
    storage.init_board().unwrap();
    write_config(dir.path(), "  isolation:\n    mode: auto\n");
    let recorder = RecordingLauncher::new();
    let ops = Operations::with_launcher(dir.path(), Box::new(recorder.clone()));

    let task = ops.create_task(NewTask::titled("In place")).unwrap();
    ops.take_task(&task.id, "ses-place", true).unwrap().unwrap();

    let taken = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert!(taken.worktree.is_none() && taken.branch.is_none() && taken.base_commit.is_none());
    assert_eq!(recorder.roots()[0].1, dir.path().to_path_buf());
    assert!(
        fs::read_dir(dir.path().join(".kanban/worktrees"))
            .unwrap()
            .count()
            == 0,
        "no worktree is created for an unregistered board"
    );
}

#[test]
fn mode_auto_falls_back_to_the_shared_folder_with_a_note_when_not_a_repo() {
    let store = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = ProjectStore::at(store.path())
        .add(work.path(), None)
        .unwrap()
        .project;
    Storage::new(&project.data_root).init_board().unwrap();
    write_config(&project.data_root, "  isolation:\n    mode: auto\n");
    let recorder = RecordingLauncher::new();
    let ops = Operations::for_project_with_launcher(&project, Box::new(recorder.clone()));

    let task = ops.create_task(NewTask::titled("No git")).unwrap();
    ops.take_task(&task.id, "ses-nogit", true).unwrap().unwrap();

    let taken = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert!(taken.worktree.is_none());
    assert_eq!(recorder.roots()[0].1, work.path().to_path_buf());
    let thread = kanban4ai::core::thread::ThreadManager::new(ops.data_root())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|m| m.body.contains("worktree isolation unavailable")),
        "the fallback is audited on the thread"
    );
}

#[test]
fn mode_required_refuses_to_launch_when_unavailable() {
    let store = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = ProjectStore::at(store.path())
        .add(work.path(), None)
        .unwrap()
        .project;
    Storage::new(&project.data_root).init_board().unwrap();
    write_config(&project.data_root, "  isolation:\n    mode: required\n");
    let recorder = RecordingLauncher::new();
    let ops = Operations::for_project_with_launcher(&project, Box::new(recorder.clone()));

    let task = ops.create_task(NewTask::titled("Needs git")).unwrap();
    let err = ops.take_task(&task.id, "ses-req", true).unwrap_err();
    assert!(err.to_string().contains("isolation is required"));
    assert!(recorder.calls().is_empty(), "nothing launched");
    // The take rolled back: the task is back in To Do with no worktree.
    let rolled_back = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(rolled_back.status, TaskStatus::Todo);
    assert!(rolled_back.worktree.is_none());
}

#[test]
fn relaunch_reuses_the_same_worktree_and_branch() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    let task = ops.create_task(NewTask::titled("Reused")).unwrap();
    ops.take_task(&task.id, "ses-a", true).unwrap().unwrap();
    let first = ops.storage.load_task(&task.id).unwrap().unwrap();
    let first_base = first.base_commit.clone().unwrap();
    let wt = worktree_dir(ops.data_root(), &task.id);

    // The "agent" commits work on its branch, then the task re-runs.
    fs::write(wt.join("agent.txt"), "agent work\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "agent work"]);
    ops.take_task(&task.id, "ses-b", true).unwrap().unwrap();

    let second = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(second.base_commit.as_deref(), Some(first_base.as_str()));
    assert_eq!(second.worktree, first.worktree);
    assert_eq!(second.branch, first.branch);
    assert_eq!(
        fs::read_to_string(wt.join("agent.txt")).unwrap(),
        "agent work\n"
    );
    let repo = vcs::detect(&work_path).unwrap();
    assert_eq!(
        repo.read_ref(INTEGRATION_REF).unwrap().unwrap().as_str(),
        first_base,
        "no second snapshot chained"
    );
}

#[test]
fn detached_jobs_and_prompts_target_the_worktree() {
    let (_store, _work, _work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    let task = ops.create_task(NewTask::titled("Cwd probe")).unwrap();
    ops.take_task(&task.id, "ses-cwd", true).unwrap().unwrap();
    let taken = ops.storage.load_task(&task.id).unwrap().unwrap();
    let wt = worktree_dir(ops.data_root(), &task.id);

    let job = ops
        .detach_command(
            &task.id,
            "ses-cwd",
            Some(10),
            Some("pwd probe"),
            &["pwd".to_string()],
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !job.status_file.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        fs::read_to_string(&job.log_file).unwrap().trim(),
        wt.display().to_string()
    );

    let prompt = kanban4ai::agent::build_agent_prompt(
        Roots::new(ops.data_root(), &wt, Some("p")),
        &taken,
        "ses-cwd",
        false,
        Role::Executor,
    )
    .unwrap();
    assert!(prompt.contains("isolated git checkout"));
    assert!(prompt.contains("kanban/TASK-001"));
    assert!(prompt.contains(&wt.display().to_string()));
}

#[test]
fn done_lands_the_branch_without_committing_or_staging_on_the_user_branch() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    let task = ops.create_task(NewTask::titled("Lander")).unwrap();
    take_and_context(&ops, &task.id, "ses-land");

    let wt = worktree_dir(ops.data_root(), &task.id);
    // The agent edits a tracked file, adds a new one, and forgets to commit
    // a second new file. None of that exists in the work folder yet.
    fs::write(wt.join("committed.txt"), "agent touched committed\n").unwrap();
    fs::write(wt.join("feature.txt"), "feature\n").unwrap();
    fs::write(wt.join("late.txt"), "forgot to commit\n").unwrap();
    let branch_tip = git(&wt, &["rev-parse", "HEAD"]);

    // The human works in the work folder while the agent runs: edits a file
    // the branch never touches and creates a new one.
    fs::write(work_path.join("live.txt"), "human typed more\n").unwrap();
    fs::write(work_path.join("human.txt"), "human note\n").unwrap();
    let head_before = git(&work_path, &["rev-parse", "HEAD"]);
    let base = ops
        .storage
        .load_task(&task.id)
        .unwrap()
        .unwrap()
        .base_commit
        .unwrap();

    let reviewed = ops
        .complete_task(&task.id, "ses-land", true)
        .unwrap()
        .unwrap();

    assert_eq!(reviewed.status, TaskStatus::Review);
    assert_eq!(reviewed.integration, IntegrationState::Landed);
    assert_eq!(
        fs::read_to_string(work_path.join("committed.txt")).unwrap(),
        "agent touched committed\n",
        "the agent's edit landed"
    );
    assert_eq!(
        fs::read_to_string(work_path.join("feature.txt")).unwrap(),
        "feature\n"
    );
    assert_eq!(
        fs::read_to_string(work_path.join("late.txt")).unwrap(),
        "forgot to commit\n",
        "uncommitted agent work lands too"
    );
    assert_eq!(
        fs::read_to_string(work_path.join("live.txt")).unwrap(),
        "human typed more\n",
        "the concurrent human edit survived byte-identical"
    );
    assert_eq!(
        fs::read_to_string(work_path.join("human.txt")).unwrap(),
        "human note\n"
    );

    // CRITICAL INVARIANT: no commit on the user's branch, nothing staged.
    assert_eq!(git(&work_path, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&work_path, &["diff", "--cached", "--name-only"]), "");
    let status = git(&work_path, &["status", "--porcelain"]);
    assert!(status.contains("M committed.txt"), "{status}");
    assert!(status.contains("feature.txt"), "{status}");

    // The integration ref advanced to a merge commit of the landed tree with
    // parents [previous integration tip, task branch tip]. The branch tip
    // itself is new (commit_all ran during done): it descends from the
    // worktree HEAD captured above and carries the uncommitted work.
    let repo = vcs::detect(&work_path).unwrap();
    let integration = repo
        .read_ref(INTEGRATION_REF)
        .unwrap()
        .unwrap()
        .as_str()
        .to_string();
    assert_eq!(
        git(&work_path, &["rev-parse", &format!("{integration}^1")]),
        base
    );
    assert_eq!(
        git(&work_path, &["rev-parse", &format!("{integration}^2^")]),
        branch_tip
    );
    assert_eq!(
        git(&work_path, &["show", &format!("{integration}^2:late.txt")]),
        "forgot to commit"
    );
    assert_ne!(integration, head_before);
    assert_eq!(
        git(&work_path, &["show", &format!("{integration}:feature.txt")]),
        "feature"
    );

    // Cleanup (on_land default): the worktree and branch are gone, and the
    // task no longer points at them.
    assert!(!wt.exists(), "worktree removed after landing");
    assert!(!git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));
    let done = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert!(done.worktree.is_none() && done.branch.is_none() && done.base_commit.is_none());
}

#[test]
fn landing_deletes_task_files_and_keeps_unrelated_human_edits() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    // A second committed file the task will delete.
    fs::write(work_path.join("delete-me.txt"), "obsolete\n").unwrap();
    git(&work_path, &["add", "-A"]);
    git(&work_path, &["commit", "-q", "-m", "add delete-me"]);

    let task = ops.create_task(NewTask::titled("Deleter")).unwrap();
    take_and_context(&ops, &task.id, "ses-del");
    let wt = worktree_dir(ops.data_root(), &task.id);
    fs::remove_file(wt.join("delete-me.txt")).unwrap();
    fs::write(wt.join("kept.txt"), "agent adds\n").unwrap();

    // The human edits a different committed file while the agent works.
    fs::write(work_path.join("committed.txt"), "human edit\n").unwrap();
    let head_before = git(&work_path, &["rev-parse", "HEAD"]);

    let reviewed = ops
        .complete_task(&task.id, "ses-del", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.integration, IntegrationState::Landed);

    assert!(
        !work_path.join("delete-me.txt").exists(),
        "a file deleted by the task is removed from the work folder"
    );
    assert_eq!(
        fs::read_to_string(work_path.join("kept.txt")).unwrap(),
        "agent adds\n"
    );
    assert_eq!(
        fs::read_to_string(work_path.join("committed.txt")).unwrap(),
        "human edit\n",
        "unrelated human edit survives byte-identical"
    );
    assert_eq!(git(&work_path, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&work_path, &["diff", "--cached", "--name-only"]), "");
}

#[test]
fn conflicting_lands_report_conflict_and_integrate_lands_after_resolution() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    // The regression case from the investigation: one file, two fields,
    // both sides edit both fields.
    fs::write(work_path.join("hero.txt"), "armor = 10\nhp_regen = 5\n").unwrap();
    git(&work_path, &["add", "-A"]);
    git(&work_path, &["commit", "-q", "-m", "add hero"]);

    let task = ops.create_task(NewTask::titled("Armor")).unwrap();
    take_and_context(&ops, &task.id, "ses-armor");
    let wt = worktree_dir(ops.data_root(), &task.id);
    fs::write(wt.join("hero.txt"), "armor = 99\nhp_regen = 55\n").unwrap();

    // The human rewrites both fields while the agent works.
    fs::write(work_path.join("hero.txt"), "armor = 20\nhp_regen = 8\n").unwrap();
    let head_before = git(&work_path, &["rev-parse", "HEAD"]);

    let reviewed = ops
        .complete_task(&task.id, "ses-armor", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.status, TaskStatus::Review);
    assert_eq!(reviewed.integration, IntegrationState::Conflict);

    // Nothing was written anywhere; the worktree and branch are kept.
    assert_eq!(
        fs::read_to_string(work_path.join("hero.txt")).unwrap(),
        "armor = 20\nhp_regen = 8\n",
        "the human's live values are untouched by a conflicting land"
    );
    assert_eq!(git(&work_path, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&work_path, &["diff", "--cached", "--name-only"]), "");
    assert!(wt.exists(), "conflicting worktree is kept");
    assert!(git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));
    // Neither side's fields are lost: the branch tip still carries the
    // agent's values, the work folder the human's.
    let branch_blob = git(&wt, &["show", "HEAD:hero.txt"]);
    assert!(branch_blob.contains("armor = 99") && branch_blob.contains("hp_regen = 55"));

    // The thread names the conflicting path.
    let thread = kanban4ai::core::thread::ThreadManager::new(ops.data_root())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|m| { m.body.contains("merge conflict") && m.body.contains("hero.txt") })
    );

    // TASK-249: the structured conflict report is routed through the
    // review-edits buffer — the human sees the conflicting path, the base
    // commit, all three versions as blob ids, and how to finish.
    let edits = reviewed.review_edits.clone();
    let base = ops
        .storage
        .load_task(&task.id)
        .unwrap()
        .unwrap()
        .base_commit
        .unwrap();
    assert!(edits.contains("hero.txt"), "{edits}");
    assert!(edits.contains(&base), "{edits}");
    assert!(
        edits.contains(&format!("kanban done {}", task.id)),
        "{edits}"
    );
    assert!(edits.contains(&wt.display().to_string()), "{edits}");
    let report_blob = |label: &str| {
        let line = edits
            .lines()
            .find(|l| l.contains(label))
            .unwrap_or_else(|| panic!("no {label} line in the report: {edits}"));
        let oid = line.split(": ").last().unwrap().trim();
        git(&work_path, &["cat-file", "blob", oid])
    };
    assert_eq!(report_blob("stage 1"), "armor = 10\nhp_regen = 5");
    assert_eq!(report_blob("stage 2"), "armor = 20\nhp_regen = 8");
    assert_eq!(report_blob("stage 3"), "armor = 99\nhp_regen = 55");

    // The human side is merged INTO the isolated worktree: the markers and
    // both sides' values are there, the work folder is still untouched.
    let wt_hero = fs::read_to_string(wt.join("hero.txt")).unwrap();
    assert!(wt_hero.contains("<<<<<<<"), "{wt_hero}");
    assert!(wt_hero.contains("armor = 99"), "{wt_hero}");
    assert!(wt_hero.contains("armor = 20"), "{wt_hero}");

    // Integrating without resolving still conflicts and writes nothing.
    let (again, outcome) = ops.integrate_task(&task.id).unwrap().unwrap();
    assert_eq!(again.integration, IntegrationState::Conflict);
    assert_eq!(
        outcome,
        LandOutcome::Conflict {
            paths: vec!["hero.txt".to_string()]
        }
    );
    assert_eq!(
        fs::read_to_string(work_path.join("hero.txt")).unwrap(),
        "armor = 20\nhp_regen = 8\n"
    );

    // Resolution: the values are settled (resolver flow), the worktree
    // commit carries them, and the human accepts them into the work folder.
    fs::write(wt.join("hero.txt"), "armor = 50\nhp_regen = 30\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "resolve landing"]);
    fs::write(work_path.join("hero.txt"), "armor = 50\nhp_regen = 30\n").unwrap();

    let (landed, outcome) = ops.integrate_task(&task.id).unwrap().unwrap();
    assert_eq!(landed.integration, IntegrationState::Landed);
    assert!(
        matches!(outcome, LandOutcome::Landed { .. }),
        "expected a clean land, got {outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(work_path.join("hero.txt")).unwrap(),
        "armor = 50\nhp_regen = 30\n"
    );
    assert_eq!(git(&work_path, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&work_path, &["diff", "--cached", "--name-only"]), "");
    assert!(!wt.exists(), "resolved land cleans the worktree up");
    assert!(!git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));

    // A second integrate is refused: nothing new to land.
    let err = ops.integrate_task(&task.id).unwrap_err();
    assert!(err.to_string().contains("already landed"), "{err}");
}

/// The full resolver round trip (TASK-249, `on_conflict: resolver`): a
/// conflicted land merges the human side into the task's own worktree,
/// dispatches a resolver run immediately, and a resolution followed by
/// `kanban done` lands both changes.
#[test]
fn on_conflict_resolver_auto_dispatches_and_done_lands_both_changes() {
    let (_store, _work, work_path, ops, recorder) = git_project_ops(
        "  isolation:\n    mode: auto\n    seed: head\n    on_conflict: resolver\n",
    );
    fs::write(work_path.join("hero.txt"), "armor = 10\nhp_regen = 5\n").unwrap();
    git(&work_path, &["add", "-A"]);
    git(&work_path, &["commit", "-q", "-m", "add hero"]);

    let task = ops.create_task(NewTask::titled("Resolver")).unwrap();
    take_and_context(&ops, &task.id, "ses-resolve");
    let wt = worktree_dir(ops.data_root(), &task.id);
    let base = ops
        .storage
        .load_task(&task.id)
        .unwrap()
        .unwrap()
        .base_commit
        .unwrap();

    fs::write(wt.join("hero.txt"), "armor = 99\nhp_regen = 55\n").unwrap();

    // The human commits the conflicting edit on their branch and leaves an
    // unrelated uncommitted file alone.
    fs::write(work_path.join("hero.txt"), "armor = 20\nhp_regen = 8\n").unwrap();
    git(&work_path, &["add", "hero.txt"]);
    git(&work_path, &["commit", "-q", "-m", "human hero edit"]);
    let head_before = git(&work_path, &["rev-parse", "HEAD"]);
    fs::write(work_path.join("human.txt"), "human note\n").unwrap();

    let reviewed = ops
        .complete_task(&task.id, "ses-resolve", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.integration, IntegrationState::Conflict);

    // A resolver run was dispatched immediately on a fresh session.
    let calls = recorder.calls();
    assert_eq!(calls.len(), 2, "resolver auto-dispatched: {calls:?}");
    assert_eq!(calls[1].0, task.id);
    let dispatched = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert_eq!(dispatched.status, TaskStatus::InProgress);
    assert_eq!(dispatched.session.as_deref(), Some(calls[1].1.as_str()));
    assert!(
        dispatched.review_edits.is_empty(),
        "the report was folded into the thread"
    );
    let thread = kanban4ai::core::thread::ThreadManager::new(ops.data_root())
        .unwrap()
        .load(&task.id)
        .unwrap();
    let report = thread
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::ReviewEdit)
        .map(|m| m.body.clone())
        .expect("conflict report on the thread");
    assert!(report.contains("hero.txt"), "{report}");
    assert!(report.contains(&base), "{report}");
    let report_blob = |label: &str| {
        let line = report
            .lines()
            .find(|l| l.contains(label))
            .unwrap_or_else(|| panic!("no {label} line in the report: {report}"));
        let oid = line.split(": ").last().unwrap().trim();
        git(&work_path, &["cat-file", "blob", oid])
    };
    assert_eq!(report_blob("stage 1"), "armor = 10\nhp_regen = 5");
    assert_eq!(report_blob("stage 2"), "armor = 20\nhp_regen = 8");
    assert_eq!(report_blob("stage 3"), "armor = 99\nhp_regen = 55");

    // The conflict path keeps both sides as commits and the worktree present,
    // with the markers and the work folder untouched.
    let wt_hero = fs::read_to_string(wt.join("hero.txt")).unwrap();
    assert!(wt_hero.contains("<<<<<<<"), "{wt_hero}");
    assert!(wt_hero.contains("armor = 99"), "{wt_hero}");
    assert!(wt_hero.contains("armor = 20"), "{wt_hero}");
    assert_eq!(
        fs::read_to_string(work_path.join("hero.txt")).unwrap(),
        "armor = 20\nhp_regen = 8\n"
    );
    assert_eq!(git(&work_path, &["rev-parse", "HEAD"]), head_before);
    assert!(git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));

    // The resolver settles both intents, commits, and finishes with done —
    // which re-runs the landing.
    fs::write(wt.join("hero.txt"), "armor = 50\nhp_regen = 30\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "resolve: keep both intents"]);
    let session = dispatched.session.clone().unwrap();
    let landed = ops
        .complete_task(&task.id, &session, true)
        .unwrap()
        .unwrap();
    assert_eq!(landed.integration, IntegrationState::Landed);

    assert_eq!(
        fs::read_to_string(work_path.join("hero.txt")).unwrap(),
        "armor = 50\nhp_regen = 30\n",
        "the resolution carrying both fields landed in the work folder"
    );
    assert_eq!(
        fs::read_to_string(work_path.join("human.txt")).unwrap(),
        "human note\n",
        "the human's unrelated uncommitted edit is intact"
    );
    assert_eq!(git(&work_path, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&work_path, &["diff", "--cached", "--name-only"]), "");
    assert!(!wt.exists(), "resolved land cleans the worktree up");
    assert!(!git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));
}

#[test]
fn manual_land_mode_defers_the_landing_to_integrate() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n    land: manual\n");
    let task = ops.create_task(NewTask::titled("Manual land")).unwrap();
    take_and_context(&ops, &task.id, "ses-manual");
    let wt = worktree_dir(ops.data_root(), &task.id);
    fs::write(wt.join("feature.txt"), "manual landing\n").unwrap();
    let head_before = git(&work_path, &["rev-parse", "HEAD"]);

    let reviewed = ops
        .complete_task(&task.id, "ses-manual", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.status, TaskStatus::Review);
    assert_eq!(reviewed.integration, IntegrationState::Pending);
    assert!(
        !work_path.join("feature.txt").exists(),
        "land: manual writes nothing on done"
    );
    assert!(wt.exists());

    let (landed, outcome) = ops.integrate_task(&task.id).unwrap().unwrap();
    assert_eq!(landed.integration, IntegrationState::Landed);
    assert!(matches!(outcome, LandOutcome::Landed { .. }));
    assert_eq!(
        fs::read_to_string(work_path.join("feature.txt")).unwrap(),
        "manual landing\n"
    );
    assert_eq!(git(&work_path, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&work_path, &["diff", "--cached", "--name-only"]), "");
    assert!(!wt.exists(), "integrate applies the on_land cleanup too");
}

#[test]
fn integrate_refuses_tasks_without_an_isolated_branch() {
    let (_store, _work, _work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: off\n");
    let task = ops.create_task(NewTask::titled("Plain")).unwrap();
    ops.take_task(&task.id, "ses-plain", false)
        .unwrap()
        .unwrap();

    let err = ops.integrate_task(&task.id).unwrap_err();
    assert!(err.to_string().contains("no isolated branch"), "{err}");
    assert!(ops.integrate_task("TASK-999").unwrap().is_none());
}

/// TASK-250: dropping a task drops its worktree and its branch — the branch
/// carries unmerged agent work here, and abandon is an explicit human
/// discard.
#[test]
fn abandon_removes_both_the_worktree_and_the_branch() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    let task = ops.create_task(NewTask::titled("Doomed")).unwrap();
    ops.take_task(&task.id, "ses-doom", true).unwrap().unwrap();
    let wt = worktree_dir(ops.data_root(), &task.id);
    fs::write(wt.join("feature.txt"), "unmerged work\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "agent work"]);
    assert!(git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));

    ops.abandon_task(&task.id).unwrap();

    assert!(!wt.exists(), "the worktree directory is gone");
    assert!(
        !git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"),
        "the unmerged task branch is gone too"
    );
    assert_eq!(
        git(
            &work_path,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads/"]
        ),
        "main",
        "no other ref was touched"
    );
}

/// TASK-250: a task that reached Done without a landing (here `land:
/// manual`) still drops its worktree and branch on the way through
/// `move_task_to_done`.
#[test]
fn done_without_landing_still_clears_the_worktree_and_branch() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n    land: manual\n");
    let task = ops.create_task(NewTask::titled("Manual")).unwrap();
    take_and_context(&ops, &task.id, "ses-manual-done");
    let wt = worktree_dir(ops.data_root(), &task.id);
    fs::write(wt.join("feature.txt"), "never landed\n").unwrap();

    let reviewed = ops
        .complete_task(&task.id, "ses-manual-done", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.status, TaskStatus::Review);
    assert!(wt.exists(), "land: manual keeps the worktree for review");

    ops.move_task(&task.id, "done", false).unwrap();

    assert!(!wt.exists(), "Done is terminal: the worktree is gone");
    assert!(
        !git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"),
        "and the never-landed branch with it"
    );
    let done = ops.storage.load_task(&task.id).unwrap().unwrap();
    assert!(done.worktree.is_none() && done.branch.is_none() && done.base_commit.is_none());
}

/// TASK-250: the GC pass reclaims the worktree and branch of a task that
/// vanished without its cleanup — a crash, a kill, a hand-deleted directory
/// or task file — while a live task's artifacts stay put.
#[test]
fn the_gc_pass_reclaims_orphans_and_spares_live_tasks() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    let live = ops.create_task(NewTask::titled("Live")).unwrap();
    ops.take_task(&live.id, "ses-live", true).unwrap().unwrap();
    let orphan = ops.create_task(NewTask::titled("Orphan")).unwrap();
    ops.take_task(&orphan.id, "ses-orphan", true)
        .unwrap()
        .unwrap();
    let orphan_wt = worktree_dir(ops.data_root(), &orphan.id);
    fs::write(orphan_wt.join("feature.txt"), "abandoned mid-run\n").unwrap();
    git(&orphan_wt, &["add", "-A"]);
    git(&orphan_wt, &["commit", "-q", "-m", "agent work"]);

    // A kill between the task-file deletion and the cleanup leaves both
    // artifacts behind with no task to own them.
    ops.storage.delete_task(&orphan.id).unwrap();
    assert!(orphan_wt.exists());
    assert!(git_ref_exists(&work_path, "refs/heads/kanban/TASK-002"));

    ops.abandon_stalled_tasks().unwrap();

    assert!(!orphan_wt.exists(), "the orphan worktree directory is gone");
    assert!(
        !git_ref_exists(&work_path, "refs/heads/kanban/TASK-002"),
        "the orphan branch is gone"
    );
    assert!(
        worktree_dir(ops.data_root(), &live.id).is_dir(),
        "a live task keeps its worktree"
    );
    assert!(
        git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"),
        "a live task keeps its branch"
    );
}

/// TASK-250: with no task holding a worktree the integration ref is
/// re-baselined to a fresh snapshot parented on HEAD, so the old snapshot
/// chain stops being a GC root; a held worktree pins it.
#[test]
fn the_integration_ref_is_rebaselined_once_no_task_holds_a_worktree() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    let repo = vcs::detect(&work_path).unwrap();

    let first = ops.create_task(NewTask::titled("First")).unwrap();
    take_and_context(&ops, &first.id, "ses-first");
    fs::write(
        worktree_dir(ops.data_root(), &first.id).join("f.txt"),
        "1\n",
    )
    .unwrap();
    let landed = ops
        .complete_task(&first.id, "ses-first", true)
        .unwrap()
        .unwrap();
    assert_eq!(landed.integration, IntegrationState::Landed);
    let pinned = repo.read_ref(INTEGRATION_REF).unwrap().unwrap();
    assert_ne!(pinned.as_str(), git(&work_path, &["rev-parse", "HEAD"]));

    // A live task holds a worktree: the ref stays where the landing put it.
    let second = ops.create_task(NewTask::titled("Second")).unwrap();
    ops.take_task(&second.id, "ses-second", true)
        .unwrap()
        .unwrap();
    let held = repo.read_ref(INTEGRATION_REF).unwrap().unwrap();
    ops.abandon_stalled_tasks().unwrap();
    assert_eq!(repo.read_ref(INTEGRATION_REF).unwrap().unwrap(), held);

    // The holder goes away; the next pass re-baselines onto HEAD.
    ops.abandon_task(&second.id).unwrap();
    assert!(repo.read_ref(INTEGRATION_REF).unwrap().is_some());
    ops.abandon_stalled_tasks().unwrap();
    let rebaselined = repo.read_ref(INTEGRATION_REF).unwrap().unwrap();
    assert_ne!(rebaselined, held, "the old snapshot chain was released");
    assert_eq!(
        git(
            &work_path,
            &["rev-parse", &format!("{}^", rebaselined.as_str())]
        ),
        git(&work_path, &["rev-parse", "HEAD"]),
        "the fresh baseline is parented on HEAD"
    );
}

/// TASK-250: a Conflict worktree is the one place unmerged agent work
/// lives — it survives the GC pass, Done, and abandon.
#[test]
fn a_conflict_worktree_survives_every_cleanup_path() {
    let (_store, _work, work_path, ops, _recorder) =
        git_project_ops("  isolation:\n    mode: auto\n");
    fs::write(work_path.join("hero.txt"), "armor = 10\nhp_regen = 5\n").unwrap();
    git(&work_path, &["add", "-A"]);
    git(&work_path, &["commit", "-q", "-m", "add hero"]);

    let task = ops.create_task(NewTask::titled("Conflicted")).unwrap();
    take_and_context(&ops, &task.id, "ses-conflict");
    let wt = worktree_dir(ops.data_root(), &task.id);
    fs::write(wt.join("hero.txt"), "armor = 99\nhp_regen = 55\n").unwrap();
    fs::write(work_path.join("hero.txt"), "armor = 20\nhp_regen = 8\n").unwrap();

    let reviewed = ops
        .complete_task(&task.id, "ses-conflict", true)
        .unwrap()
        .unwrap();
    assert_eq!(reviewed.integration, IntegrationState::Conflict);
    assert!(wt.exists() && git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));

    // The GC pass: the task is live, nothing is touched.
    ops.abandon_stalled_tasks().unwrap();
    assert!(wt.exists() && git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));

    // Done without a resolution: still never auto-deleted.
    ops.move_task(&task.id, "done", false).unwrap();
    assert!(wt.exists() && git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));

    // Abandon drops tasks, but a Conflict worktree is left for the human.
    ops.abandon_task(&task.id).unwrap();
    assert!(wt.exists() && git_ref_exists(&work_path, "refs/heads/kanban/TASK-001"));
}
