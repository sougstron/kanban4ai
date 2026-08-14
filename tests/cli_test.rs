//! End-to-end CLI smoke tests: the binary must speak the same contract as the
//! Python `kanban` CLI (same commands, same key output lines).

mod common;

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kanban4ai::core::project::ProjectStore;
use predicates::prelude::*;

/// Isolated work folder + store. Every binary invocation sets `KANBAN_HOME`
/// so the suite never touches the developer's real store.
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
        ProjectStore::at(self.store.path())
            .resolve_from_cwd(self.work())
            .expect("resolve store")
            .expect("cwd is a registered project")
            .data_root
    }

    fn kanban(&self) -> PathBuf {
        self.data_root().join(".kanban")
    }
}

fn kanban(env: &Env) -> Command {
    let mut cmd = Command::cargo_bin("kanban4ai").expect("binary builds");
    cmd.current_dir(env.work());
    cmd.env("KANBAN_HOME", env.store.path());
    cmd.env_remove("KANBAN_SESSION");
    cmd.env_remove("KANBAN_PROJECT");
    cmd
}

fn board() -> Env {
    let env = Env::new();
    kanban(&env).arg("init").assert().success();
    common::write_quiet_config(&env.data_root(), false);
    env
}

#[test]
fn init_creates_board() {
    let dir = Env::new();
    kanban(&dir)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized project "))
        .stdout(predicate::str::contains(" for "));
    assert!(dir.kanban().join("config.yaml").is_file());
    assert!(
        !dir.work().join(".kanban").exists(),
        "init must not create a local .kanban"
    );
}

#[test]
fn create_list_show_flow() {
    let dir = board();
    kanban(&dir)
        .args([
            "create",
            "Fix login bug",
            "--description",
            "Users cannot log in",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Created task TASK-001: Fix login bug",
        ));

    kanban(&dir)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("TASK-001"))
        .stdout(predicate::str::contains("todo"));

    kanban(&dir)
        .args(["show", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Title: Fix login bug"))
        .stdout(predicate::str::contains("Users cannot log in"));
}

#[test]
fn list_json_is_valid_and_complete() {
    let dir = board();
    kanban(&dir)
        .args([
            "create",
            "Json task",
            "--model",
            "opus",
            "--backend",
            "claude",
        ])
        .assert()
        .success();

    let output = kanban(&dir)
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let task = &parsed[0];
    assert_eq!(task["id"], "TASK-001");
    assert_eq!(task["status"], "todo");
    assert_eq!(task["ai_model"], "opus");
    assert_eq!(task["agent_backend"], "claude");
    assert!(task["created_at"].is_string());
}

#[test]
fn move_and_agent_rules() {
    let dir = board();
    kanban(&dir).args(["create", "Rules"]).assert().success();

    kanban(&dir)
        .args(["move", "TASK-001", "review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 moved to review"));

    kanban(&dir)
        .args(["move", "TASK-001", "done", "--agent"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Permission denied: Agent cannot move tasks to Done",
        ));

    kanban(&dir)
        .args(["move", "TASK-001", "nowhere"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Invalid status 'nowhere'"));

    kanban(&dir)
        .args(["move", "TASK-001", "done"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 moved to done"));
}

#[test]
fn take_and_done_agent_flow() {
    let dir = board();
    kanban(&dir)
        .args(["create", "Agent job"])
        .assert()
        .success();

    kanban(&dir)
        .args(["take", "TASK-001", "--session", "ses-cli", "--agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Task TASK-001 assigned to session ses-cli",
        ))
        .stdout(predicate::str::contains("Status: in_progress"));

    // agent done without context is refused
    kanban(&dir)
        .args(["done", "TASK-001", "--session", "ses-cli", "--agent"])
        .assert()
        .success()
        .stderr(predicate::str::contains("without recording context"));

    kanban(&dir)
        .args(["context", "TASK-001", "implemented the fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Context added to TASK-001"));

    kanban(&dir)
        .args(["done", "TASK-001", "--session", "ses-cli", "--agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 marked as review"));

    // human confirms
    kanban(&dir)
        .args(["done", "TASK-001", "--session", "ses-cli"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 marked as done"));
}

#[test]
fn question_pipeline_via_cli() {
    let dir = board();
    kanban(&dir).args(["create", "Ask me"]).assert().success();

    kanban(&dir)
        .args([
            "ask",
            "TASK-001",
            "Tabs or spaces?",
            "--agent",
            "--variants",
            "Tabs",
            "--variants",
            "Spaces",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Question added to TASK-001"))
        .stdout(predicate::str::contains("Task has pending questions."));

    kanban(&dir)
        .args(["questions", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[question] Tabs or spaces?"))
        .stdout(predicate::str::contains("variants: Tabs, Spaces"));

    kanban(&dir)
        .args(["answer", "TASK-001", "0", "Spaces"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Answer added to TASK-001"));

    kanban(&dir)
        .args(["questions", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No open messages."));

    kanban(&dir)
        .args(["suggest", "TASK-001", "Could also add linting"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Suggestion added to TASK-001"));
}

#[test]
fn ask_form_posts_questions_from_yaml_file() {
    let dir = board();
    kanban(&dir).args(["create", "Form me"]).assert().success();

    let form_path = dir.work().join("form.yaml");
    std::fs::write(
        &form_path,
        "questions:\n  - prompt: Which backend?\n    options: [OAuth2, API key]\n  - prompt: Any constraints?\n",
    )
    .unwrap();

    kanban(&dir)
        .args(["ask-form", "TASK-001", "--file"])
        .arg(&form_path)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Posted 2 question(s) from form to TASK-001",
        ))
        .stdout(predicate::str::contains("Task has pending questions."));

    kanban(&dir)
        .args(["questions", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[question] Which backend?"))
        .stdout(predicate::str::contains("variants: OAuth2, API key"))
        .stdout(predicate::str::contains("[question] Any constraints?"));
}

/// An agent's cwd is the code folder, so `--file .kanban/forms/…` (the shape
/// the prompt used before the split, and habit afterwards) cannot be found
/// there; the path is retried against the board.
#[test]
fn ask_form_and_context_resolve_a_file_against_the_board() {
    let dir = board();
    kanban(&dir)
        .args(["create", "Board-relative file"])
        .assert()
        .success();

    let forms = dir.kanban().join("forms");
    std::fs::create_dir_all(&forms).unwrap();
    std::fs::write(
        forms.join("TASK-001.ask.yaml"),
        "questions:\n  - prompt: Which root?\n",
    )
    .unwrap();
    std::fs::write(dir.kanban().join("note.txt"), "board-relative note\n").unwrap();

    kanban(&dir)
        .args([
            "ask-form",
            "TASK-001",
            "--file",
            ".kanban/forms/TASK-001.ask.yaml",
            "--agent",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Posted 1 question(s)"));

    kanban(&dir)
        .args(["context", "TASK-001", "", "--file", "note.txt"])
        .assert()
        .success();

    kanban(&dir)
        .args(["questions", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[question] Which root?"));
    let thread_file = dir.kanban().join("threads/TASK-001.yaml");
    assert!(
        std::fs::read_to_string(&thread_file)
            .unwrap()
            .contains("board-relative note")
    );
}

#[test]
fn ask_form_rejects_an_invalid_form() {
    let dir = board();
    kanban(&dir).args(["create", "Bad form"]).assert().success();

    let form_path = dir.work().join("bad.yaml");
    std::fs::write(&form_path, "questions: []\n").unwrap();

    kanban(&dir)
        .args(["ask-form", "TASK-001", "--file"])
        .arg(&form_path)
        .assert()
        .failure();

    // Nothing was posted.
    kanban(&dir)
        .args(["questions", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No open messages."));
}

#[test]
fn reject_and_unreject_quarantine_a_context_message() {
    let dir = board();
    kanban(&dir)
        .args(["create", "Reject me"])
        .assert()
        .success();

    // MSG-001/002 are the seeded system/task messages, so this context lands
    // on MSG-003.
    kanban(&dir)
        .args(["context", "TASK-001", "poisoned note", "--source", "agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Context added to TASK-001"));

    kanban(&dir)
        .args(["reject", "TASK-001", "MSG-003"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Message MSG-003 rejected on TASK-001",
        ));

    let raw = std::fs::read_to_string(dir.kanban().join("threads/TASK-001.yaml")).unwrap();
    assert!(raw.contains("status: rejected"));

    kanban(&dir)
        .args(["reject", "TASK-001", "MSG-404"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Message MSG-404 not found on TASK-001",
        ));

    kanban(&dir)
        .args(["unreject", "TASK-001", "MSG-003"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Message MSG-003 restored on TASK-001",
        ));

    let raw = std::fs::read_to_string(dir.kanban().join("threads/TASK-001.yaml")).unwrap();
    assert!(!raw.contains("status: rejected"));
}

#[test]
fn chain_set_show_clear() {
    let dir = board();
    kanban(&dir).args(["create", "Target"]).assert().success();
    kanban(&dir).args(["create", "Follower"]).assert().success();

    kanban(&dir)
        .args(["chain", "TASK-002", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("TASK-002 chained to TASK-001"));

    kanban(&dir)
        .args(["chain", "TASK-002"])
        .assert()
        .success()
        .stdout(predicate::str::contains("TASK-002 is chained to TASK-001"));

    kanban(&dir)
        .args(["chain", "TASK-002", "TASK-002"])
        .assert()
        .success()
        .stderr(predicate::str::contains("cannot be chained to itself"));

    kanban(&dir)
        .args(["chain", "TASK-002", "--clear"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Chain removed from TASK-002"));
}

#[test]
fn edits_and_rerun() {
    let dir = board();
    kanban(&dir)
        .args(["create", "Reviewable"])
        .assert()
        .success();
    kanban(&dir)
        .args(["move", "TASK-001", "review"])
        .assert()
        .success();

    kanban(&dir)
        .args(["edits", "TASK-001", "Handle the edge case too"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Review edits saved on TASK-001"));

    kanban(&dir)
        .args(["rerun", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 re-running (ses-"));
}

#[test]
fn archive_flow() {
    let dir = board();
    kanban(&dir).args(["create", "Old work"]).assert().success();
    kanban(&dir)
        .args(["move", "TASK-001", "done"])
        .assert()
        .success();

    kanban(&dir)
        .arg("archive-done")
        .assert()
        .success()
        .stdout(predicate::str::contains("Archived 1 done task(s)."));

    kanban(&dir)
        .arg("archive")
        .assert()
        .success()
        .stdout(predicate::str::contains("TASK-001"))
        .stdout(predicate::str::contains("Old work"));
}

#[test]
fn detach_runs_command_and_declares_wait() {
    let dir = board();
    kanban(&dir)
        .args(["create", "Long export"])
        .assert()
        .success();
    kanban(&dir)
        .args(["take", "TASK-001", "--session", "ses-detach-cli", "--agent"])
        .assert()
        .success();

    kanban(&dir)
        .args([
            "detach",
            "TASK-001",
            "--session",
            "ses-detach-cli",
            "--eta",
            "30",
            "--note",
            "cli smoke export",
            "--",
            "sh",
            "-c",
            "echo cli-detached-ok",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Detached pid"))
        .stdout(predicate::str::contains(".kanban/detached/TASK-001-"))
        .stdout(predicate::str::contains("Relaunch deadline:"));

    let detached_dir = dir.kanban().join("detached");
    let status_file = wait_for_status_file(&detached_dir);
    assert_eq!(
        std::fs::read_to_string(&status_file).unwrap().trim(),
        "0",
        "detached command records a clean exit"
    );

    // A command is required after `--`.
    kanban(&dir)
        .args(["detach", "TASK-001", "--session", "ses-detach-cli"])
        .assert()
        .failure();
}

/// Poll `.kanban/detached/` until the job's `.status` file lands.
fn wait_for_status_file(detached_dir: &std::path::Path) -> std::path::PathBuf {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(entries) = std::fs::read_dir(detached_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "status") {
                    return path;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached status file never appeared in {}",
            detached_dir.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[test]
fn sessions_heartbeat_check_recover() {
    let dir = board();
    kanban(&dir).args(["create", "Crashy"]).assert().success();
    kanban(&dir)
        .args(["take", "TASK-001", "--session", "ses-hb", "--agent"])
        .assert()
        .success();

    kanban(&dir)
        .args(["heartbeat", "--session", "ses-hb"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Heartbeat updated for session ses-hb",
        ));

    kanban(&dir)
        .arg("sessions")
        .assert()
        .success()
        .stdout(predicate::str::contains("ses-hb"))
        .stdout(predicate::str::contains("Crashy"));

    kanban(&dir)
        .arg("check-sessions")
        .assert()
        .success()
        .stdout(predicate::str::contains("No crashed sessions found."));

    kanban(&dir)
        .args(["recover", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Task TASK-001 recovered and moved to To Do",
        ));
}

#[test]
fn sessions_uses_saved_name_when_task_file_is_missing() {
    let dir = board();
    kanban(&dir)
        .args(["create", "Missing task label"])
        .assert()
        .success();
    kanban(&dir)
        .args([
            "take",
            "TASK-001",
            "--session",
            "ses-missing-task",
            "--agent",
        ])
        .assert()
        .success();
    std::fs::remove_file(dir.kanban().join("tasks/in_progress/TASK-001.md")).unwrap();

    kanban(&dir)
        .arg("sessions")
        .assert()
        .success()
        .stdout(predicate::str::contains("ses-missing-task"))
        .stdout(predicate::str::contains("Missing task label"));
}

#[test]
fn compact_reports_no_context() {
    let dir = board();
    kanban(&dir).args(["create", "Empty"]).assert().success();
    kanban(&dir)
        .args(["compact", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No context found for this task."));
}

#[test]
fn revert_command_reports_missing_backups() {
    let dir = board();
    kanban(&dir)
        .args(["create", "Needs revert"])
        .assert()
        .success();

    kanban(&dir)
        .args(["revert", "TASK-001", "--session", "ses-revert-test"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to launch revert"));
}

#[test]
fn version_flag_works() {
    let dir = Env::new();
    kanban(&dir)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("kanban"));
}

#[test]
fn tui_requires_interactive_terminal_and_attach_reports_missing_task() {
    let dir = board();
    kanban(&dir)
        .arg("tui")
        .assert()
        .failure()
        .stderr(predicate::str::contains("interactive terminal"));

    kanban(&dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("interactive terminal"));

    kanban(&dir)
        .args(["attach", "TASK-404"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Task TASK-404 not found"));
}

#[test]
fn init_is_a_noop_when_already_registered() {
    let dir = board();
    kanban(&dir).args(["create", "Keep me"]).assert().success();

    kanban(&dir)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("already initialized"));

    kanban(&dir)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("TASK-001"))
        .stdout(predicate::str::contains("Keep me"));
    assert!(!dir.work().join(".kanban").exists());
}

#[test]
fn init_migrates_a_legacy_local_board() {
    let dir = Env::new();
    std::fs::create_dir_all(dir.work().join(".kanban/tasks/todo")).unwrap();
    std::fs::write(
        dir.work().join(".kanban/config.yaml"),
        "tui:\n  name: Legacy\n",
    )
    .unwrap();
    std::fs::write(dir.work().join(".kanban/tasks/todo/TASK-001.md"), "old").unwrap();

    kanban(&dir)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized project Legacy"));

    assert!(!dir.work().join(".kanban").exists());
    assert!(dir.kanban().join("tasks/todo/TASK-001.md").is_file());
}

#[test]
fn list_without_a_project_exits_one() {
    let dir = Env::new();
    kanban(&dir)
        .arg("list")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not inside a kanban project"));
}

#[test]
fn project_add_list_rename_remove_and_path() {
    let dir = Env::new();
    kanban(&dir)
        .args(["project", "add", ".", "--name", "Board One"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Added project Board One"));

    kanban(&dir)
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Board One"));

    kanban(&dir)
        .args(["project", "rename", "Board One", "Board Two"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Renamed project"));

    kanban(&dir)
        .args(["project", "path", "Board Two"])
        .assert()
        .success()
        .stdout(predicate::str::contains(dir.work().display().to_string()));

    kanban(&dir)
        .args(["project", "remove", "Board Two", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Unregistered project"));

    kanban(&dir)
        .args(["project", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No projects registered."));
}

#[test]
fn project_flag_selects_a_registered_board() {
    let dir = board();
    kanban(&dir).args(["create", "Flagged"]).assert().success();

    let elsewhere = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("kanban4ai").expect("binary builds");
    cmd.current_dir(elsewhere.path());
    cmd.env("KANBAN_HOME", dir.store());
    cmd.env_remove("KANBAN_SESSION");
    cmd.env_remove("KANBAN_PROJECT");
    cmd.args(["--project", dir.work().to_str().unwrap(), "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Flagged"));
}

#[test]
fn list_silently_adopts_an_unregistered_local_board() {
    let dir = Env::new();
    std::fs::create_dir_all(dir.work().join(".kanban/tasks/todo")).unwrap();
    std::fs::write(
        dir.work().join(".kanban/config.yaml"),
        "tui:\n  name: Adopted\n",
    )
    .unwrap();

    kanban(&dir)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("No tasks found."));

    assert!(!dir.work().join(".kanban").exists());
    assert!(dir.kanban().join("config.yaml").is_file());

    kanban(&dir)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("No tasks found."));
}

#[test]
fn list_leaves_a_board_in_place_when_a_session_is_live() {
    let dir = Env::new();
    std::fs::create_dir_all(dir.work().join(".kanban/sessions")).unwrap();
    std::fs::write(
        dir.work().join(".kanban/config.yaml"),
        "tui:\n  name: Live\n",
    )
    .unwrap();
    let now = kanban4ai::core::timefmt::format(&kanban4ai::core::timefmt::now());
    std::fs::write(
        dir.work().join(".kanban/sessions/ses-live.yaml"),
        format!(
            "id: ses-live\ntask_id: TASK-001\nstatus: active\nstarted_at: '{now}'\nlast_seen: '{now}'\n"
        ),
    )
    .unwrap();

    kanban(&dir)
        .arg("list")
        .assert()
        .success()
        .stderr(predicate::str::contains("active agent sessions"))
        .stdout(predicate::str::contains("No tasks found."));

    assert!(dir.work().join(".kanban/config.yaml").is_file());
}

#[test]
fn init_copy_leaves_the_local_board() {
    let dir = Env::new();
    std::fs::create_dir_all(dir.work().join(".kanban/tasks/todo")).unwrap();
    std::fs::write(
        dir.work().join(".kanban/config.yaml"),
        "tui:\n  name: Copy\n",
    )
    .unwrap();
    std::fs::write(dir.work().join(".kanban/tasks/todo/TASK-001.md"), "stay").unwrap();

    kanban(&dir).args(["init", "--copy"]).assert().success();

    assert!(dir.work().join(".kanban/tasks/todo/TASK-001.md").is_file());
    assert_eq!(
        std::fs::read_to_string(dir.kanban().join("tasks/todo/TASK-001.md")).unwrap(),
        "stay"
    );
}

#[test]
fn list_from_a_subdirectory_uses_the_registered_project() {
    let dir = board();
    kanban(&dir).args(["create", "Nested"]).assert().success();
    let nested = dir.work().join("src/lib");
    std::fs::create_dir_all(&nested).unwrap();

    let mut cmd = Command::cargo_bin("kanban4ai").expect("binary builds");
    cmd.current_dir(&nested);
    cmd.env("KANBAN_HOME", dir.store());
    cmd.env_remove("KANBAN_SESSION");
    cmd.env_remove("KANBAN_PROJECT");
    cmd.arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("Nested"));
}
