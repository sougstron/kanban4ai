//! End-to-end CLI smoke tests: the binary must speak the same contract as the
//! Python `kanban` CLI (same commands, same key output lines).

mod common;

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kanban4ai::core::daemon;
use kanban4ai::core::operations::Operations;
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
fn stop_closes_active_session_and_keeps_task_in_progress() {
    let dir = board();
    kanban(&dir).args(["create", "Stop me"]).assert().success();
    kanban(&dir)
        .args(["take", "TASK-001", "--session", "ses-cli-stop", "--agent"])
        .assert()
        .success();

    kanban(&dir)
        .args(["stop", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Stopped TASK-001 session ses-cli-stop",
        ));

    kanban(&dir)
        .arg("sessions")
        .assert()
        .success()
        .stdout(predicate::str::contains("No active sessions."));

    kanban(&dir)
        .args(["show", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Status: in_progress"))
        .stdout(predicate::str::contains("ses-cli-stop"));

    kanban(&dir)
        .args(["stop", "TASK-001"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("has no active session"));

    kanban(&dir)
        .args(["stop", "TASK-404"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Task TASK-404 not found"));
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
fn project_rename_updates_the_board_settings_name() {
    let dir = Env::new();
    kanban(&dir)
        .args(["project", "add", ".", "--name", "Folder Name"])
        .assert()
        .success();

    let config = dir.kanban().join("config.yaml");
    let before = std::fs::read_to_string(&config).expect("board config");
    assert!(
        before.contains("name: Kanban") || before.contains("name: Folder Name"),
        "{before}"
    );

    kanban(&dir)
        .args(["project", "rename", "Folder Name", "Ledger Book"])
        .assert()
        .success();

    let after = std::fs::read_to_string(&config).expect("board config after rename");
    assert!(
        after.contains("name: Ledger Book"),
        "the projects list reads tui.name, so a CLI rename must write it: {after}"
    );
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

#[test]
fn statusline_bridge_feeds_the_claude_limits_row() {
    let dir = Env::new();
    let payload = r#"{"session_id":"ses-1","model":{"id":"claude-opus-5"},"rate_limits":{
        "five_hour":{"used_percentage":34.0,"resets_at":2000000000},
        "seven_day":{"used_percentage":3.0,"resets_at":2000000000}}}"#;

    kanban(&dir)
        .arg("statusline-bridge")
        .write_stdin(payload)
        .assert()
        .success()
        .stdout("");

    let bridge: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.store().join("claude-rate-limits.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bridge["windows"][0]["label"], "5h");
    assert_eq!(bridge["windows"][0]["remaining_percent"], 66.0);
    assert_eq!(bridge["windows"][1]["label"], "7d");
    assert_eq!(bridge["windows"][1]["remaining_percent"], 97.0);

    // A current bridge answers without polling the usage endpoint, so the
    // claude entry is deterministic even with the network unreachable.
    let output = kanban(&dir)
        .args(["limits", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let limits: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let claude = limits["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["provider"] == "claude")
        .unwrap();
    assert_eq!(claude["state"], "ready");
    assert_eq!(claude["windows"][0]["label"], "5h");
    assert_eq!(claude["windows"][0]["remaining_percent"], 66.0);
}

#[test]
fn statusline_bridge_ignores_payloads_without_rate_limits() {
    let dir = Env::new();

    kanban(&dir)
        .arg("statusline-bridge")
        .write_stdin(r#"{"session_id":"ses-1","context_window":{"total_input_tokens":10}}"#)
        .assert()
        .success()
        .stdout("");

    assert!(!dir.store().join("claude-rate-limits.json").exists());
}

#[test]
fn limits_bridge_install_wraps_and_restores_the_statusline() {
    let dir = Env::new();
    let config = tempfile::tempdir().unwrap();
    let settings = config.path().join("settings.json");
    std::fs::write(
        &settings,
        r#"{"model":"claude-opus-5","statusLine":{"type":"command","command":"tr a-z A-Z","refreshInterval":60}}"#,
    )
    .unwrap();

    let install = |dir: &Env| {
        let mut cmd = kanban(dir);
        cmd.env("CLAUDE_CONFIG_DIR", config.path());
        cmd.args(["limits", "bridge", "install"]).assert().success();
    };
    install(&dir);

    let settings_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let command = settings_json["statusLine"]["command"].as_str().unwrap();
    assert!(command.contains("claude-statusline-bridge.sh"));
    assert_eq!(settings_json["statusLine"]["type"], "command");
    assert_eq!(settings_json["statusLine"]["refreshInterval"], 60);
    assert_eq!(settings_json["model"], "claude-opus-5");
    assert!(
        settings
            .with_file_name("settings.json.kanban4ai-bak")
            .exists()
    );

    let wrapper = dir.store().join("claude-statusline-bridge.sh");
    let script = std::fs::read_to_string(&wrapper).unwrap();
    assert!(script.contains("statusline-bridge"));
    assert!(script.contains("printf '%s' \"$payload\" | tr a-z A-Z"));
    use std::os::unix::fs::PermissionsExt;
    assert!(std::fs::metadata(&wrapper).unwrap().permissions().mode() & 0o111 != 0);
    assert_eq!(
        std::fs::read_to_string(dir.store().join("claude-statusline-bridge.original"))
            .unwrap()
            .trim(),
        "tr a-z A-Z"
    );

    // Reinstall does not nest the wrap; the sidecar keeps the true original.
    install(&dir);
    let reinstalled: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        reinstalled["statusLine"]["command"].as_str().unwrap(),
        command
    );

    // The wrapper records a statusline payload and passes it through to the
    // original command unchanged.
    assert_cmd::Command::new("sh")
        .arg(&wrapper)
        .env("KANBAN_HOME", dir.store())
        .write_stdin(
            r#"{"rate_limits":{"five_hour":{"used_percentage":10.0,"resets_at":2000000000}}}"#,
        )
        .assert()
        .success()
        .stdout(predicate::str::contains("RATE_LIMITS"));
    let bridge: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.store().join("claude-rate-limits.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bridge["windows"][0]["remaining_percent"], 90.0);

    let mut cmd = kanban(&dir);
    cmd.env("CLAUDE_CONFIG_DIR", config.path());
    cmd.args(["limits", "bridge", "remove"]).assert().success();

    let restored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(restored["statusLine"]["command"], "tr a-z A-Z");
    assert_eq!(restored["model"], "claude-opus-5");
    assert!(!dir.store().join("claude-statusline-bridge.sh").exists());
    assert!(
        !dir.store()
            .join("claude-statusline-bridge.original")
            .exists()
    );
    assert!(
        !settings
            .with_file_name("settings.json.kanban4ai-bak")
            .exists()
    );
}

fn project_ops(env: &Env) -> Operations {
    let project = ProjectStore::at(env.store())
        .resolve_from_cwd(env.work())
        .expect("resolve")
        .expect("registered");
    Operations::for_project(&project)
}

fn write_board_config(env: &Env, body: &str) {
    std::fs::write(env.kanban().join("config.yaml"), body).expect("config");
}

#[test]
fn daemon_once_is_quiet_on_an_empty_store() {
    let env = Env::new();
    kanban(&env)
        .args(["daemon", "--once"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn daemon_refuses_a_second_instance() {
    let env = board();
    let store = ProjectStore::at(env.store());
    let _lock = daemon::try_lock(&store).expect("hold daemon.lock");
    kanban(&env)
        .args(["daemon", "--once"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already running"));
}

#[test]
fn daemon_once_releases_the_lock() {
    let env = board();
    kanban(&env).args(["daemon", "--once"]).assert().success();
    kanban(&env).args(["daemon", "--once"]).assert().success();
}

#[test]
fn daemon_interval_zero_is_rejected() {
    let env = Env::new();
    kanban(&env)
        .args(["daemon", "--once", "--interval", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--interval must be greater than 0",
        ));
}

#[test]
fn daemon_skips_a_missing_work_folder_with_one_warning() {
    let env = board();
    let project = ProjectStore::at(env.store())
        .resolve_from_cwd(env.work())
        .unwrap()
        .unwrap();
    let work = env.work().to_path_buf();
    std::fs::remove_dir_all(&work).expect("remove work folder");
    kanban(&env)
        .current_dir(env.store())
        .args(["daemon", "--once"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("work folder is gone")
                .and(predicate::str::contains(&project.id)),
        );
}

#[test]
fn daemon_skips_a_project_with_queue_disabled() {
    let env = board();
    write_board_config(
        &env,
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  queue_enabled: false\n",
    );
    kanban(&env).args(["create", "Parked"]).assert().success();
    project_ops(&env).enqueue_task("TASK-001").unwrap();
    kanban(&env)
        .args(["daemon", "--once"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    let task = project_ops(&env).get_task("TASK-001").unwrap().unwrap();
    assert_eq!(
        task.run_phase,
        Some(kanban4ai::core::models::RunPhase::Queued)
    );
}

#[test]
fn daemon_unknown_project_is_an_error() {
    let env = board();
    kanban(&env)
        .args(["daemon", "--once", "--project", "no-such-board"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such project: no-such-board"));
}

#[test]
fn daemon_launches_a_queued_task_without_a_terminal() {
    let env = board();
    let sleeper = env.work().join("fake-agent.sh");
    std::fs::write(&sleeper, "#!/bin/sh\nexec sleep 30\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&sleeper).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&sleeper, perms).unwrap();
    }
    write_board_config(
        &env,
        &format!(
            "notifications:\n  enabled: false\n\
auto_launch:\n  enabled: true\n  use_tmux: true\n  terminal_fallback: true\n  default_agent: opencode\n\
agents:\n  opencode:\n    command: {}\n    extra_args: []\n",
            sleeper.display()
        ),
    );
    kanban(&env)
        .args(["create", "Daemon launch", "--backend", "opencode"])
        .assert()
        .success();
    project_ops(&env).enqueue_task("TASK-001").unwrap();

    let output = kanban(&env)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .args(["daemon", "--once"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dispatch TASK-001"))
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);
    let log = std::fs::read_to_string(env.store().join("logs/daemon.log")).expect("daemon.log");
    assert!(log.contains("dispatch TASK-001"), "{log}");

    let task = project_ops(&env).get_task("TASK-001").unwrap().unwrap();
    let session = task.session.expect("session pinned");
    assert!(
        stdout.contains(&session),
        "dispatch line should name the session: {stdout}"
    );

    if which("tmux") {
        let attached = std::process::Command::new("tmux")
            .args(["has-session", "-t", &format!("={session}")])
            .status()
            .expect("tmux has-session")
            .success();
        assert!(attached, "tmux session {session} should be attachable");
    }

    kanban(&env).args(["stop", "TASK-001"]).assert().success();
}

fn which(command: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let path = dir.join(command);
            path.is_file()
        })
    })
}
