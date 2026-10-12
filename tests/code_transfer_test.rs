mod common;
use kanban4ai::core::{
    code,
    operations::Operations,
    project::ProjectStore,
    storage::{NewTask, Storage},
    vcs,
};
use std::{fs, path::Path, process::Command};

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn fixture(root: &Path) -> (std::path::PathBuf, kanban4ai::core::project::Project) {
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.name", "Test"]);
    git(&work, &["config", "user.email", "test@example.test"]);
    fs::write(work.join("tracked"), "base").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "base"]);
    let p = ProjectStore::at(root.join("sender"))
        .add(&work, None)
        .unwrap()
        .project;
    Storage::new(&p.data_root).init_board().unwrap();
    (work, p)
}
fn replicate_project(root: &Path, p: &kanban4ai::core::project::Project) {
    let dest = root.join("projects").join(&p.id);
    fs::create_dir_all(dest.join(".kanban")).unwrap();
    let mut value: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(p.data_root.join("project.yaml")).unwrap())
            .unwrap();
    value["work_path"] = "".into();
    fs::write(
        dest.join("project.yaml"),
        serde_yaml_ng::to_string(&value).unwrap(),
    )
    .unwrap();
    Storage::new(&dest).init_board().unwrap();
}
#[test]
fn bundle_clone_and_update_preserve_checkouts_and_seed_remote_live_work() {
    let temp = tempfile::tempdir().unwrap();
    let (work, p) = fixture(temp.path());
    fs::write(work.join("tracked"), "staged").unwrap();
    git(&work, &["add", "tracked"]);
    fs::write(work.join("tracked"), "live dirty").unwrap();
    fs::write(work.join("untracked"), "unpublished").unwrap();
    let head = git(&work, &["rev-parse", "HEAD"]);
    let index = git(&work, &["write-tree"]);
    let receiver = temp.path().join("receiver");
    replicate_project(&receiver, &p);
    assert_eq!(code::status(&receiver, &p.id).unwrap().state, "missing");
    let (bundle, _) = code::export(&temp.path().join("sender"), &p.id, &[]).unwrap();
    let status = code::receive(&receiver, &p.id, bundle.path(), None).unwrap();
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&work, &["write-tree"]), index);
    assert_eq!(
        git(&status.path, &["show", "refs/kanban/integration:tracked"]),
        "live dirty"
    );
    assert_eq!(
        git(&status.path, &["show", "refs/kanban/integration:untracked"]),
        "unpublished"
    );
    assert_eq!(
        vcs::detect(&status.path).unwrap().code_remote().unwrap(),
        None
    );
    fs::write(status.path.join("tracked"), "receiver human edit").unwrap();
    fs::write(work.join("tracked"), "next live version").unwrap();
    let (next, _) = code::export(
        &temp.path().join("sender"),
        &p.id,
        &[status.integration.unwrap()],
    )
    .unwrap();
    code::receive(&receiver, &p.id, next.path(), None).unwrap();
    assert_eq!(
        fs::read_to_string(status.path.join("tracked")).unwrap(),
        "receiver human edit"
    );
    assert_eq!(git(&status.path, &["rev-parse", "HEAD"]), head);
    let received = ProjectStore::at(&receiver).get(&p.id).unwrap().unwrap();
    fs::write(received.kanban_dir().join("config.yaml"), "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  isolation:\n    mode: auto\n").unwrap();
    let ops = Operations::for_project_with_launcher(
        &received,
        Box::new(common::RecordingLauncher::new()),
    );
    let task = ops.create_task(NewTask::titled("Peer live seed")).unwrap();
    ops.take_task(&task.id, "ses-code-transfer", true)
        .unwrap()
        .unwrap();
    let wt = received.kanban_dir().join("worktrees").join(&task.id);
    assert_eq!(
        fs::read_to_string(wt.join("tracked")).unwrap(),
        "next live version"
    );
    assert_eq!(
        fs::read_to_string(wt.join("untracked")).unwrap(),
        "unpublished"
    );
}
#[test]
fn remote_preparation_transfers_unpublished_spine_without_pushing_main() {
    let temp = tempfile::tempdir().unwrap();
    let (work, p) = fixture(temp.path());
    let remote = temp.path().join("remote.git");
    git(temp.path(), &["init", "--bare", "-q", "remote.git"]);
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "origin", "main"]);
    let main = git(&remote, &["rev-parse", "main"]);
    fs::write(work.join("tracked"), "remote live").unwrap();
    let (_bundle, source) = code::export(&temp.path().join("sender"), &p.id, &[]).unwrap();
    vcs::detect(&work)
        .unwrap()
        .publish_code(source.as_deref().unwrap())
        .unwrap();
    let receiver = temp.path().join("receiver");
    replicate_project(&receiver, &p);
    let status = code::prepare_from_remote(&receiver, &p.id, source.as_deref().unwrap()).unwrap();
    assert_eq!(
        git(&status.path, &["show", "refs/kanban/integration:tracked"]),
        "remote live"
    );
    assert_eq!(git(&remote, &["rev-parse", "main"]), main);
    let received = ProjectStore::at(&receiver).get(&p.id).unwrap().unwrap();
    fs::write(received.kanban_dir().join("config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  isolation:\n    mode: auto\n").unwrap();
    let ops = Operations::for_project_with_launcher(
        &received,
        Box::new(common::RecordingLauncher::new()),
    );
    let task = ops
        .create_task(NewTask::titled("Publish peer task branch"))
        .unwrap();
    ops.take_task(&task.id, "ses-code-remote", true)
        .unwrap()
        .unwrap();
    let taken = ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(
        git(&remote, &["rev-parse", taken.branch.as_deref().unwrap()]),
        taken.base_commit.unwrap()
    );
    assert_eq!(git(&remote, &["rev-parse", "main"]), main);
}

#[test]
fn invalid_bundle_leaves_no_clone_and_mobile_requires_remote() {
    let temp = tempfile::tempdir().unwrap();
    let (_, p) = fixture(temp.path());
    let receiver = temp.path().join("receiver");
    replicate_project(&receiver, &p);
    let invalid_bundle = tempfile::NamedTempFile::new().unwrap();
    fs::write(invalid_bundle.path(), b"not a bundle").unwrap();
    assert!(code::receive(&receiver, &p.id, invalid_bundle.path(), None).is_err());
    assert_eq!(code::status(&receiver, &p.id).unwrap().state, "missing");
    assert!(!receiver.join("clones").join(&p.id).exists());
    fs::write(
        receiver.join("config.yaml"),
        "node:\n  name: mobile\n  role: mobile\n",
    )
    .unwrap();
    let journal = kanban4ai::core::sync::Journal::open(&receiver).unwrap();
    assert_eq!(journal.node.role, kanban4ai::core::sync::NodeRole::Mobile);
    assert!(
        code::receive(&receiver, &p.id, invalid_bundle.path(), None)
            .err()
            .unwrap()
            .to_string()
            .contains("SYNC_NO_REMOTE")
    );
    assert!(
        code::prepare(&receiver, &p.id)
            .err()
            .unwrap()
            .to_string()
            .contains("SYNC_NO_REMOTE")
    );
}
