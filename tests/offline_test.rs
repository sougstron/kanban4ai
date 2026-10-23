//! Local-first: a store with sync off never touches `sync/`, and a store with
//! an unreachable peer still accepts every command.
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use kanban4ai::core::project::ProjectStore;
use kanban4ai::core::storage::Storage;
use kanban4ai::core::sync::{Journal, Operation};

struct Env {
    work: tempfile::TempDir,
    store: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            work: tempfile::tempdir().unwrap(),
            store: tempfile::tempdir().unwrap(),
        }
    }

    fn work(&self) -> &Path {
        self.work.path()
    }

    fn store(&self) -> &Path {
        self.store.path()
    }

    fn data_root(&self) -> PathBuf {
        ProjectStore::at(self.store())
            .resolve_from_cwd(self.work())
            .expect("resolve")
            .expect("registered")
            .data_root
    }
}

fn kanban(env: &Env) -> Command {
    let mut cmd = Command::cargo_bin("kanban4ai").expect("binary");
    cmd.current_dir(env.work());
    cmd.env("KANBAN_HOME", env.store());
    for (key, _) in std::env::vars() {
        if key.starts_with("KANBAN_") && key != "KANBAN_HOME" {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn init(env: &Env) {
    kanban(env).arg("init").assert().success();
    common::write_quiet_config(&env.data_root(), false);
}

fn git(dir: &Path, args: &[&str]) {
    let out = Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tree_bytes(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                files.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    if root.exists() {
        walk(root, root, &mut files);
    }
    files
}

fn ops(store: &Path) -> Vec<Operation> {
    Journal::open(store).unwrap().operations(None).unwrap()
}

fn op_text(store: &Path) -> String {
    format!("{:?}", ops(store))
}

fn set_enabled(store: &Path, enabled: bool) {
    let path = store.join("config.yaml");
    let mut config: serde_yaml_ng::Value = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_yaml_ng::from_str(&text).ok())
        .unwrap_or_else(|| serde_yaml_ng::Value::Mapping(Default::default()));
    if !config.is_mapping() {
        config = serde_yaml_ng::Value::Mapping(Default::default());
    }
    if !config.get("sync").is_some_and(|sync| sync.is_mapping()) {
        config["sync"] = serde_yaml_ng::Value::Mapping(Default::default());
    }
    config["sync"]["enabled"] = serde_yaml_ng::Value::Bool(enabled);
    fs::write(path, serde_yaml_ng::to_string(&config).unwrap()).unwrap();
}

fn drop_enabled_key(store: &Path) {
    let path = store.join("config.yaml");
    let mut config: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    if let Some(sync) = config
        .get_mut("sync")
        .and_then(|sync| sync.as_mapping_mut())
    {
        sync.remove("enabled");
    }
    fs::write(path, serde_yaml_ng::to_string(&config).unwrap()).unwrap();
}

fn write_server(store: &Path, peers: &str) {
    let path = store.join("config.yaml");
    let mut config: serde_yaml_ng::Value = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_yaml_ng::from_str(&text).ok())
        .unwrap_or_else(|| serde_yaml_ng::Value::Mapping(Default::default()));
    if !config.is_mapping() {
        config = serde_yaml_ng::Value::Mapping(Default::default());
    }
    config["sync"] = serde_yaml_ng::from_str(peers).unwrap();
    fs::write(path, serde_yaml_ng::to_string(&config).unwrap()).unwrap();
}

fn task_file(data_root: &Path, id: &str) -> PathBuf {
    let root = data_root.join(".kanban/tasks");
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_stem().and_then(|name| name.to_str()) == Some(id) {
                return path;
            }
        }
    }
    panic!("missing {id} under {data_root:?}");
}

fn timed(cmd: &mut Command) {
    let started = Instant::now();
    cmd.assert().success();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "command took {:?}",
        started.elapsed()
    );
}

fn question_id(env: &Env, task: &str) -> String {
    let text =
        fs::read_to_string(env.data_root().join(format!(".kanban/threads/{task}.yaml"))).unwrap();
    let mut current = None;
    let mut question = None;
    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches("- ");
        if let Some(id) = trimmed.strip_prefix("id: ")
            && id.starts_with("MSG-")
        {
            current = Some(id.to_string());
        }
        if trimmed.starts_with("kind:") && trimmed.contains("question") {
            question = current.clone();
        }
    }
    question.or(current).expect("question id")
}

#[test]
fn disabled_store_never_touches_sync() {
    let env = Env::new();
    init(&env);
    for title in ["One", "Two", "Three"] {
        kanban(&env).args(["create", title]).assert().success();
    }
    kanban(&env)
        .args(["move", "TASK-002", "in_progress"])
        .assert()
        .success();
    kanban(&env)
        .args(["edits", "TASK-001", "a review note"])
        .assert()
        .success();
    kanban(&env)
        .args(["suggest", "TASK-001", "a thread note"])
        .assert()
        .success();
    kanban(&env)
        .args(["ask", "TASK-001", "Ship it?", "--agent"])
        .assert()
        .success();
    let msg = question_id(&env, "TASK-001");
    kanban(&env)
        .args(["answer", "TASK-001", &msg, "yes"])
        .assert()
        .success();
    kanban(&env)
        .args(["context", "TASK-001", "implemented"])
        .assert()
        .success();
    kanban(&env)
        .args(["take", "TASK-003", "--session", "ses-off", "--agent"])
        .assert()
        .success();
    kanban(&env)
        .args(["context", "TASK-003", "delegated work"])
        .assert()
        .success();
    kanban(&env)
        .args(["done", "TASK-003", "--session", "ses-off", "--agent"])
        .assert()
        .success();
    kanban(&env).args(["done", "TASK-003"]).assert().success();
    kanban(&env).arg("archive-done").assert().success();

    assert!(!env.store().join("sync").exists());
    let config = fs::read_to_string(env.store().join("config.yaml")).unwrap_or_default();
    assert!(!config.contains("node:"), "{config}");
    let storage = Storage::new(env.data_root());
    for id in ["TASK-001", "TASK-002", "TASK-003"] {
        let task = storage.load_task(id).unwrap().expect(id);
        assert_eq!(task.id, id);
        let path = task_file(env.data_root().as_path(), id);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\n"), "{text}");
        assert!(text.contains(&format!("id: {id}")), "{text}");
    }
}

#[test]
fn legacy_journal_untouched_when_disabled() {
    let env = Env::new();
    set_enabled(env.store(), true);
    init(&env);
    kanban(&env).args(["create", "Legacy"]).assert().success();
    assert!(env.store().join("sync").is_dir());
    drop_enabled_key(env.store());
    let _ = fs::remove_file(env.store().join("sync/armed"));
    let before = tree_bytes(&env.store().join("sync"));
    kanban(&env).arg("list").assert().success();
    kanban(&env)
        .args(["edits", "TASK-001", "offline edit"])
        .assert()
        .success();
    kanban(&env)
        .args(["suggest", "TASK-001", "offline note"])
        .assert()
        .success();
    assert_eq!(tree_bytes(&env.store().join("sync")), before);
    assert!(!env.store().join("sync/armed").exists());
}

fn seed_local_files(env: &Env) {
    init(env);
    kanban(env).args(["create", "Baseline"]).assert().success();
    kanban(env)
        .args(["suggest", "TASK-001", "thread body"])
        .assert()
        .success();
    let kanban_dir = env.data_root().join(".kanban");
    for (rel, bytes) in [
        ("instructions/x.md", b"# rules\n".as_slice()),
        ("assets/images/a.png", b"\x89PNG".as_slice()),
        ("assets/other.bin", b"secret".as_slice()),
        ("context/c.md", b"local context\n".as_slice()),
    ] {
        let path = kanban_dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}

fn assert_whitelist_baseline(env: &Env) {
    kanban(env).arg("list").assert().success();
    let text = op_text(env.store());
    for needle in [
        "TASK-001",
        "instructions/x.md",
        "assets/images/a.png",
        "thread body",
    ] {
        assert!(text.contains(needle), "missing {needle} in {text}");
    }
    assert!(!text.contains("other.bin"), "{text}");
    assert!(!text.contains("context/c.md"), "{text}");
    assert!(env.store().join("sync/armed").is_file());
    let before = ops(env.store()).len();
    kanban(env)
        .args(["edits", "TASK-001", "one delta"])
        .assert()
        .success();
    let after = ops(env.store());
    assert_eq!(after.len(), before + 1, "{after:?}");
    assert!(format!("{after:?}").contains("one delta"));
}

#[test]
fn enable_later_snapshots_whitelist() {
    let env = Env::new();
    seed_local_files(&env);
    assert!(!env.store().join("sync").exists());
    set_enabled(env.store(), true);
    assert_whitelist_baseline(&env);
}

#[test]
fn enable_later_via_server_config_snapshots_whitelist() {
    let env = Env::new();
    seed_local_files(&env);
    write_server(
        env.store(),
        "server: {node_id: n-peer, name: peer, urls: ['http://127.0.0.1:9']}\n",
    );
    assert_whitelist_baseline(&env);
}

#[test]
fn disable_edit_reenable_keeps_both_deltas() {
    let env = Env::new();
    init(&env);
    for title in ["Alpha", "Beta", "Gamma"] {
        kanban(&env).args(["create", title]).assert().success();
    }
    set_enabled(env.store(), true);
    kanban(&env)
        .args(["edits", "TASK-001", "field-a"])
        .assert()
        .success();
    drop_enabled_key(env.store());
    kanban(&env)
        .args(["depends", "TASK-001", "--on", "TASK-002"])
        .assert()
        .success();
    set_enabled(env.store(), true);
    kanban(&env)
        .args(["chain", "TASK-001", "TASK-003"])
        .assert()
        .success();
    let text = op_text(env.store());
    assert!(text.contains("field-a"), "{text}");
    assert!(text.contains("TASK-002"), "{text}");
    assert!(text.contains("TASK-003"), "{text}");
    let replica = tempfile::tempdir().unwrap();
    Journal::open(replica.path()).unwrap();
    common::bootstrap(env.store(), replica.path());
    let task = walk_task(&replica.path().join("projects"), "TASK-001");
    assert!(task.contains("field-a"), "{task}");
    assert!(task.contains("TASK-002"), "{task}");
    assert!(task.contains("TASK-003"), "{task}");
}

fn walk_task(projects: &Path, id: &str) -> String {
    fn walk(dir: &Path, id: &str) -> Option<String> {
        for entry in fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = walk(&path, id) {
                    return Some(found);
                }
            } else if path.file_name().and_then(|n| n.to_str()) == Some(&format!("{id}.md")) {
                return fs::read_to_string(path).ok();
            }
        }
        None
    }
    walk(projects, id).unwrap_or_else(|| panic!("missing {id}"))
}

#[test]
fn unreachable_server_commands_work_offline() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let env = Env::new();
    init(&env);
    write_server(
        env.store(),
        &format!(
            "server: {{node_id: n-closed, name: closed, urls: ['http://127.0.0.1:{port}', 'http://192.0.2.1:9']}}\n"
        ),
    );
    let before = if env.store().join("sync").exists() {
        ops(env.store()).len()
    } else {
        0
    };
    timed(kanban(&env).args(["create", "Offline"]));
    timed(kanban(&env).args(["move", "TASK-001", "in_progress"]));
    timed(kanban(&env).args(["edits", "TASK-001", "still local"]));
    timed(kanban(&env).args(["suggest", "TASK-001", "thread"]));
    timed(kanban(&env).args(["ask", "TASK-001", "ok?", "--agent"]));
    let msg = question_id(&env, "TASK-001");
    timed(kanban(&env).args(["answer", "TASK-001", &msg, "yes"]));
    timed(kanban(&env).args(["take", "TASK-001", "--session", "ses-off", "--agent"]));
    let after = ops(env.store());
    assert!(after.len() > before, "{} -> {}", before, after.len());
    let replica = tempfile::tempdir().unwrap();
    Journal::open(replica.path()).unwrap();
    common::bootstrap(env.store(), replica.path());
    let task = walk_task(&replica.path().join("projects"), "TASK-001");
    assert!(task.contains("id: TASK-001"), "{task}");
    assert_eq!(task.matches("id: TASK-001").count(), 1);
}

fn pin_runner(env: &Env, task: &str, runner: &str) {
    let path = task_file(env.data_root().as_path(), task);
    let text = fs::read_to_string(&path).unwrap();
    let updated = text.replacen("status:", &format!("runner: {runner}\nstatus:"), 1);
    fs::write(path, updated).unwrap();
}

fn launch_config(env: &Env) {
    fs::write(
        env.data_root().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\n  default_agent: opencode\nrules:\n  auto_launch_on_delegate: true\nagents:\n  opencode:\n    command: /bin/true\n    extra_args: []\n",
    )
    .unwrap();
}

fn pair_closed(env: &Env) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    write_server(
        env.store(),
        &format!(
            "server: {{node_id: n-peer, name: peer, urls: ['http://127.0.0.1:{port}']}}\nwip_push: true\n"
        ),
    );
    fs::create_dir_all(env.store().join("sync")).unwrap();
    fs::write(
        env.store().join("sync/server.yaml"),
        "node_id: n-peer\ntoken: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    )
    .unwrap();
}

#[test]
fn explicit_remote_runner_offline_queues() {
    let env = Env::new();
    git(env.work(), &["init", "-q", "-b", "main"]);
    git(env.work(), &["config", "user.email", "t@example.test"]);
    git(env.work(), &["config", "user.name", "Test"]);
    fs::write(env.work().join("readme"), "base\n").unwrap();
    git(env.work(), &["add", "readme"]);
    git(env.work(), &["commit", "-qm", "base"]);
    let bare = env.store().join("origin.git");
    git(
        env.store(),
        &["init", "-q", "--bare", bare.to_str().unwrap()],
    );
    git(
        env.work(),
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(env.work(), &["push", "-q", "-u", "origin", "main"]);
    init(&env);
    launch_config(&env);
    pair_closed(&env);
    kanban(&env).args(["create", "Remote"]).assert().success();
    pin_runner(&env, "TASK-001", "peer");
    let started = Instant::now();
    kanban(&env)
        .args(["take", "TASK-001", "--session", "ses-off", "--agent"])
        .assert()
        .success()
        .stderr(predicates::str::contains("queued"));
    assert!(started.elapsed() < Duration::from_secs(5));
    let task = Storage::new(env.data_root())
        .load_task("TASK-001")
        .unwrap()
        .unwrap();
    assert!(task.runner_request.is_some(), "{task:?}");

    let bare_env = Env::new();
    init(&bare_env);
    launch_config(&bare_env);
    pair_closed(&bare_env);
    kanban(&bare_env)
        .args(["create", "No remote"])
        .assert()
        .success();
    pin_runner(&bare_env, "TASK-001", "peer");
    let before = fs::read(task_file(bare_env.data_root().as_path(), "TASK-001")).unwrap();
    kanban(&bare_env)
        .args(["take", "TASK-001", "--session", "ses-off", "--agent"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("SYNC_NO_REMOTE"))
        .stderr(predicates::str::contains("--runner"));
    let after = fs::read(task_file(bare_env.data_root().as_path(), "TASK-001")).unwrap();
    assert_eq!(before, after);
}

#[test]
#[ignore = "prints disabled-store save_task cost against a raw write; compare with 0.7.6"]
fn bench_disabled_saves_against_raw_writes() {
    use kanban4ai::core::storage::{NewTask, atomic_write_text};
    let env = Env::new();
    init(&env);
    let storage = Storage::new(env.data_root());
    let mut task = storage.create_task(NewTask::titled("bench")).unwrap();
    let started = Instant::now();
    for n in 0..500 {
        task.description = format!("edit {n}");
        storage.save_task(&task).unwrap();
    }
    let saved = started.elapsed();
    let raw_path = env.store().join("raw.txt");
    let started = Instant::now();
    for n in 0..500 {
        atomic_write_text(&raw_path, &format!("edit {n}")).unwrap();
    }
    eprintln!(
        "disabled save_task x500: {:?}; atomic_write_text x500: {:?}",
        saved,
        started.elapsed()
    );
}
