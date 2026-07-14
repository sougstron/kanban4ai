//! End-to-end CLI smoke tests: the binary must speak the same contract as the
//! Python `kanban` CLI (same commands, same key output lines).

mod common;

use assert_cmd::Command;
use predicates::prelude::*;

fn kanban(dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("kanban4ai").expect("binary builds");
    cmd.current_dir(dir);
    cmd.env_remove("KANBAN_SESSION");
    cmd
}

/// Init a board in a temp dir with notifications and auto-launch disabled.
fn board() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    kanban(dir.path()).arg("init").assert().success();
    common::write_quiet_config(dir.path(), false);
    dir
}

#[test]
fn init_creates_board() {
    let dir = tempfile::tempdir().unwrap();
    kanban(dir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Initialized kanban board at ./.kanban",
        ));
    assert!(dir.path().join(".kanban/config.yaml").is_file());
}

#[test]
fn create_list_show_flow() {
    let dir = board();
    kanban(dir.path())
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

    kanban(dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("TASK-001"))
        .stdout(predicate::str::contains("todo"));

    kanban(dir.path())
        .args(["show", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Title: Fix login bug"))
        .stdout(predicate::str::contains("Users cannot log in"));
}

#[test]
fn list_json_is_valid_and_complete() {
    let dir = board();
    kanban(dir.path())
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

    let output = kanban(dir.path())
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
    kanban(dir.path())
        .args(["create", "Rules"])
        .assert()
        .success();

    kanban(dir.path())
        .args(["move", "TASK-001", "review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 moved to review"));

    kanban(dir.path())
        .args(["move", "TASK-001", "done", "--agent"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Permission denied: Agent cannot move tasks to Done",
        ));

    kanban(dir.path())
        .args(["move", "TASK-001", "nowhere"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Invalid status 'nowhere'"));

    kanban(dir.path())
        .args(["move", "TASK-001", "done"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 moved to done"));
}

#[test]
fn take_and_done_agent_flow() {
    let dir = board();
    kanban(dir.path())
        .args(["create", "Agent job"])
        .assert()
        .success();

    kanban(dir.path())
        .args(["take", "TASK-001", "--session", "ses-cli", "--agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Task TASK-001 assigned to session ses-cli",
        ))
        .stdout(predicate::str::contains("Status: in_progress"));

    // agent done without context is refused
    kanban(dir.path())
        .args(["done", "TASK-001", "--session", "ses-cli", "--agent"])
        .assert()
        .success()
        .stderr(predicate::str::contains("without recording context"));

    kanban(dir.path())
        .args(["context", "TASK-001", "implemented the fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Context added to TASK-001"));

    kanban(dir.path())
        .args(["done", "TASK-001", "--session", "ses-cli", "--agent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 marked as review"));

    // human confirms
    kanban(dir.path())
        .args(["done", "TASK-001", "--session", "ses-cli"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 marked as done"));
}

#[test]
fn question_pipeline_via_cli() {
    let dir = board();
    kanban(dir.path())
        .args(["create", "Ask me"])
        .assert()
        .success();

    kanban(dir.path())
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

    kanban(dir.path())
        .args(["questions", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[question] Tabs or spaces?"))
        .stdout(predicate::str::contains("variants: Tabs, Spaces"));

    kanban(dir.path())
        .args(["answer", "TASK-001", "0", "Spaces"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Answer added to TASK-001"));

    kanban(dir.path())
        .args(["questions", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No open messages."));

    kanban(dir.path())
        .args(["suggest", "TASK-001", "Could also add linting"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Suggestion added to TASK-001"));
}

#[test]
fn chain_set_show_clear() {
    let dir = board();
    kanban(dir.path())
        .args(["create", "Target"])
        .assert()
        .success();
    kanban(dir.path())
        .args(["create", "Follower"])
        .assert()
        .success();

    kanban(dir.path())
        .args(["chain", "TASK-002", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("TASK-002 chained to TASK-001"));

    kanban(dir.path())
        .args(["chain", "TASK-002"])
        .assert()
        .success()
        .stdout(predicate::str::contains("TASK-002 is chained to TASK-001"));

    kanban(dir.path())
        .args(["chain", "TASK-002", "TASK-002"])
        .assert()
        .success()
        .stderr(predicate::str::contains("cannot be chained to itself"));

    kanban(dir.path())
        .args(["chain", "TASK-002", "--clear"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Chain removed from TASK-002"));
}

#[test]
fn edits_and_rerun() {
    let dir = board();
    kanban(dir.path())
        .args(["create", "Reviewable"])
        .assert()
        .success();
    kanban(dir.path())
        .args(["move", "TASK-001", "review"])
        .assert()
        .success();

    kanban(dir.path())
        .args(["edits", "TASK-001", "Handle the edge case too"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Review edits saved on TASK-001"));

    kanban(dir.path())
        .args(["rerun", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Task TASK-001 re-running (ses-"));
}

#[test]
fn archive_flow() {
    let dir = board();
    kanban(dir.path())
        .args(["create", "Old work"])
        .assert()
        .success();
    kanban(dir.path())
        .args(["move", "TASK-001", "done"])
        .assert()
        .success();

    kanban(dir.path())
        .arg("archive-done")
        .assert()
        .success()
        .stdout(predicate::str::contains("Archived 1 done task(s)."));

    kanban(dir.path())
        .arg("archive")
        .assert()
        .success()
        .stdout(predicate::str::contains("TASK-001"))
        .stdout(predicate::str::contains("Old work"));
}

#[test]
fn sessions_heartbeat_check_recover() {
    let dir = board();
    kanban(dir.path())
        .args(["create", "Crashy"])
        .assert()
        .success();
    kanban(dir.path())
        .args(["take", "TASK-001", "--session", "ses-hb", "--agent"])
        .assert()
        .success();

    kanban(dir.path())
        .args(["heartbeat", "--session", "ses-hb"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Heartbeat updated for session ses-hb",
        ));

    kanban(dir.path())
        .arg("sessions")
        .assert()
        .success()
        .stdout(predicate::str::contains("ses-hb"))
        .stdout(predicate::str::contains("Crashy"));

    kanban(dir.path())
        .arg("check-sessions")
        .assert()
        .success()
        .stdout(predicate::str::contains("No crashed sessions found."));

    kanban(dir.path())
        .args(["recover", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Task TASK-001 recovered and moved to To Do",
        ));
}

#[test]
fn compact_reports_no_context() {
    let dir = board();
    kanban(dir.path())
        .args(["create", "Empty"])
        .assert()
        .success();
    kanban(dir.path())
        .args(["compact", "TASK-001"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No context found for this task."));
}

#[test]
fn revert_command_reports_missing_backups() {
    let dir = board();
    kanban(dir.path())
        .args(["create", "Needs revert"])
        .assert()
        .success();

    kanban(dir.path())
        .args(["revert", "TASK-001", "--session", "ses-revert-test"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to launch revert"));
}

#[test]
fn version_flag_works() {
    let dir = tempfile::tempdir().unwrap();
    kanban(dir.path())
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("kanban"));
}

#[test]
fn tui_requires_interactive_terminal_and_attach_reports_missing_task() {
    let dir = board();
    kanban(dir.path())
        .arg("tui")
        .assert()
        .failure()
        .stderr(predicate::str::contains("interactive terminal"));

    kanban(dir.path())
        .args(["attach", "TASK-404"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Task TASK-404 not found"));
}
