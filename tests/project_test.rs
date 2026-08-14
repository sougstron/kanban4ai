//! Projects store / registry (`core::project`), phase 1 of TASK-163.

use std::fs;
use std::path::{Path, PathBuf};

use kanban4ai::core::error::KanbanError;
use kanban4ai::core::migrate::relocate_board;
use kanban4ai::core::models::{Session, SessionStatus};
use kanban4ai::core::project::{AddOptions, Project, ProjectStore, normalize_path, slugify};
use kanban4ai::core::session::SessionManager;
use kanban4ai::core::storage::Storage;
use kanban4ai::core::timefmt;

/// Store plus a folder to hold work directories, both inside one tempdir.
fn temp_store() -> (tempfile::TempDir, ProjectStore) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let store = ProjectStore::at(dir.path().join("store"));
    fs::create_dir_all(dir.path().join("work")).expect("create work dir");
    (dir, store)
}

fn work_dir(root: &Path, name: &str) -> PathBuf {
    let path = root.join("work").join(name);
    fs::create_dir_all(&path).expect("create work folder");
    normalize_path(&path)
}

fn add(store: &ProjectStore, path: &Path) -> Project {
    store.add(path, None).expect("add project").project
}

#[test]
fn add_registers_a_project_and_writes_project_yaml() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "kanban4ai");

    let added = store.add(&work, None).expect("add project");

    assert!(added.created);
    assert!(!added.restored);
    let project = added.project;
    assert_eq!(project.id, "kanban4ai");
    assert_eq!(project.name, "kanban4ai");
    assert_eq!(project.work_path, work);
    assert_eq!(project.data_root, store.projects_dir().join("kanban4ai"));
    assert_eq!(project.kanban_dir(), project.data_root.join(".kanban"));
    assert_eq!(project.last_opened_at, None);

    let yaml = fs::read_to_string(project.data_root.join("project.yaml")).expect("read yaml");
    assert!(yaml.contains("id: kanban4ai"));
    assert!(yaml.contains(&format!("work_path: {}", work.display())));
    assert!(
        yaml.contains("created_at: '"),
        "timestamps are quoted: {yaml}"
    );
    assert!(!yaml.contains("last_opened_at"));
    assert!(!yaml.contains("migrated_from"));

    // Nothing is written into the work folder — the point of the feature.
    assert_eq!(fs::read_dir(&work).unwrap().count(), 0);
}

#[test]
fn add_uses_the_slug_of_the_folder_name() {
    let (dir, store) = temp_store();

    let project = add(&store, &work_dir(dir.path(), "My Project"));

    assert_eq!(project.id, "my-project");
    assert_eq!(project.name, "My Project");
}

#[test]
fn add_accepts_an_explicit_display_name() {
    let (dir, store) = temp_store();

    let project = store
        .add(&work_dir(dir.path(), "repo"), Some("  Nice Board  "))
        .expect("add project")
        .project;

    assert_eq!(project.id, "repo");
    assert_eq!(project.name, "Nice Board");
}

#[test]
fn colliding_folder_names_get_numeric_suffixes() {
    let (dir, store) = temp_store();
    fs::create_dir_all(dir.path().join("work/a")).unwrap();
    fs::create_dir_all(dir.path().join("work/b")).unwrap();

    let first = add(&store, &work_dir(dir.path(), "a/repo"));
    let second = add(&store, &work_dir(dir.path(), "b/repo"));

    assert_eq!(first.id, "repo");
    assert_eq!(second.id, "repo-2");
    assert_ne!(first.data_root, second.data_root);
}

#[test]
fn add_is_idempotent_for_an_already_registered_folder() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    let first = store.add(&work, None).expect("add project");

    let again = store.add(&work, None).expect("re-add project");

    assert!(!again.created);
    assert_eq!(again.project.id, first.project.id);
    assert_eq!(again.project.created_at, first.project.created_at);
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn add_rejects_a_path_that_is_not_a_directory() {
    let (dir, store) = temp_store();
    let file = dir.path().join("work/file.txt");
    fs::write(&file, "x").unwrap();

    assert!(store.add(&file, None).is_err());
    assert!(store.add(&dir.path().join("work/missing"), None).is_err());
}

#[test]
fn list_is_sorted_by_name_and_skips_damaged_registrations() {
    let (dir, store) = temp_store();
    store
        .add(&work_dir(dir.path(), "zeta"), Some("Zeta"))
        .unwrap();
    store
        .add(&work_dir(dir.path(), "alpha"), Some("alpha"))
        .unwrap();
    store
        .add(&work_dir(dir.path(), "mid"), Some("Mid"))
        .unwrap();
    // A directory that is not a project, and one with an unparsable file.
    fs::create_dir_all(store.projects_dir().join("stray")).unwrap();
    fs::create_dir_all(store.projects_dir().join("broken")).unwrap();
    fs::write(
        store.projects_dir().join("broken/project.yaml"),
        "not: [valid",
    )
    .unwrap();

    let names: Vec<String> = store.list().unwrap().into_iter().map(|p| p.name).collect();

    assert_eq!(names, vec!["alpha", "Mid", "Zeta"]);
}

#[test]
fn list_on_a_store_that_does_not_exist_yet_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = ProjectStore::at(dir.path().join("nope"));

    assert!(store.list().unwrap().is_empty());
}

#[test]
fn get_returns_none_for_unknown_or_escaping_ids() {
    let (dir, store) = temp_store();
    add(&store, &work_dir(dir.path(), "repo"));

    assert!(store.get("repo").unwrap().is_some());
    assert!(store.get("nope").unwrap().is_none());
    assert!(store.get("../..").unwrap().is_none());
    assert!(store.get("/etc").unwrap().is_none());
}

#[test]
fn find_matches_id_name_or_path() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    store.add(&work, Some("My Board")).unwrap();

    assert_eq!(store.find("repo").unwrap().unwrap().id, "repo");
    assert_eq!(store.find("my board").unwrap().unwrap().id, "repo");
    assert_eq!(
        store.find(work.to_str().unwrap()).unwrap().unwrap().id,
        "repo"
    );
    assert!(store.find("unknown").unwrap().is_none());
}

#[test]
fn rename_changes_the_name_but_never_the_id_or_data_root() {
    let (dir, store) = temp_store();
    let project = add(&store, &work_dir(dir.path(), "repo"));

    let renamed = store.rename(&project.id, "Renamed").unwrap();

    assert_eq!(renamed.id, project.id);
    assert_eq!(renamed.data_root, project.data_root);
    assert_eq!(renamed.name, "Renamed");
    assert_eq!(store.get("repo").unwrap().unwrap().name, "Renamed");
    assert!(store.rename(&project.id, "   ").is_err());
    assert!(store.rename("nope", "x").is_err());
}

#[test]
fn set_path_repoints_a_project() {
    let (dir, store) = temp_store();
    let project = add(&store, &work_dir(dir.path(), "repo"));
    let moved = work_dir(dir.path(), "repo-moved");

    let updated = store.set_path(&project.id, &moved).unwrap();

    assert_eq!(updated.work_path, moved);
    assert_eq!(updated.id, project.id);
    assert_eq!(
        store.resolve_from_cwd(&moved).unwrap().unwrap().id,
        project.id
    );
    assert!(
        store
            .set_path(&project.id, &dir.path().join("gone"))
            .is_err()
    );
}

#[test]
fn a_project_survives_its_work_folder_disappearing() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    let project = add(&store, &work);
    fs::remove_dir_all(&work).unwrap();

    let listed = store.get(&project.id).unwrap().unwrap();

    assert_eq!(listed.work_path, work);
    assert!(listed.work_path_missing());
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn remove_without_purge_keeps_the_board_and_re_add_reclaims_it() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    let project = store.add(&work, Some("Board")).unwrap().project;
    let marker = project.kanban_dir().join("tasks/todo/TASK-001.md");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(&marker, "task").unwrap();

    store.remove(&project.id, false).unwrap();
    assert!(store.list().unwrap().is_empty());
    assert!(store.get(&project.id).unwrap().is_none());
    assert!(marker.exists(), "board data is kept for a later re-add");

    let readded = store.add(&work, None).expect("re-add");

    assert!(readded.created);
    assert!(readded.restored);
    assert_eq!(readded.project.id, project.id);
    assert_eq!(readded.project.name, "Board");
    assert_eq!(readded.project.created_at, project.created_at);
    assert!(marker.exists());
    assert!(!project.data_root.join("project.yaml.removed").exists());
}

#[test]
fn a_different_folder_never_reclaims_a_removed_project() {
    let (dir, store) = temp_store();
    let first = add(&store, &work_dir(dir.path(), "repo"));
    store.remove(&first.id, false).unwrap();
    fs::create_dir_all(dir.path().join("work/other")).unwrap();

    let second = add(&store, &work_dir(dir.path(), "other/repo"));

    assert_eq!(second.id, "repo-2");
    assert_ne!(second.data_root, first.data_root);
}

#[test]
fn remove_with_purge_deletes_the_board_data() {
    let (dir, store) = temp_store();
    let project = add(&store, &work_dir(dir.path(), "repo"));
    fs::create_dir_all(project.kanban_dir().join("tasks")).unwrap();

    store.remove(&project.id, true).unwrap();

    assert!(!project.data_root.exists());
    assert!(store.list().unwrap().is_empty());
    assert!(store.remove(&project.id, true).is_err());
}

#[test]
fn touch_opened_records_the_last_open_time() {
    let (dir, store) = temp_store();
    let project = add(&store, &work_dir(dir.path(), "repo"));

    store.touch_opened(&project.id).unwrap();

    let opened = store.get(&project.id).unwrap().unwrap();
    assert!(opened.last_opened_at.is_some());
    assert_eq!(opened.created_at, project.created_at);
    let yaml = fs::read_to_string(project.data_root.join("project.yaml")).unwrap();
    assert!(yaml.contains("last_opened_at: '"), "{yaml}");
}

#[test]
fn resolve_from_cwd_matches_the_work_folder_exactly() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    let project = add(&store, &work);
    add(&store, &work_dir(dir.path(), "other"));

    assert_eq!(
        store.resolve_from_cwd(&work).unwrap().unwrap().id,
        project.id
    );
    assert!(
        store
            .resolve_from_cwd(&dir.path().join("work"))
            .unwrap()
            .is_none()
    );
    assert!(store.resolve_from_cwd(dir.path()).unwrap().is_none());
}

#[test]
fn resolve_from_cwd_does_not_walk_ancestors() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    add(&store, &work);
    let nested = work.join("src/core");
    fs::create_dir_all(&nested).unwrap();

    // A subdirectory is deliberately not the project: the CLI phase adopts an
    // unregistered `<cwd>/.kanban`, and a walk would let it move a parent board.
    assert!(store.resolve_from_cwd(&nested).unwrap().is_none());
}

#[test]
fn resolve_from_cwd_normalizes_the_path_it_is_given() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    let project = add(&store, &work);

    let noisy = work.join("./src/..");
    fs::create_dir_all(work.join("src")).unwrap();

    assert_eq!(
        store.resolve_from_cwd(&noisy).unwrap().unwrap().id,
        project.id
    );
}

#[test]
fn the_store_lock_is_reentrant_within_a_thread() {
    let (dir, store) = temp_store();

    let outer = store.lock().expect("outer lock");
    let inner = store.lock().expect("inner lock (reentrant)");
    // Mutations take the lock themselves; they must not deadlock under it.
    let project = add(&store, &work_dir(dir.path(), "repo"));
    store.rename(&project.id, "Renamed").unwrap();
    drop(inner);
    drop(outer);

    assert!(store.root().join(".lock").exists());
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn the_store_lock_excludes_another_process() {
    let (dir, store) = temp_store();
    add(&store, &work_dir(dir.path(), "repo"));
    let lock_path = store.root().join(".lock");

    let _guard = store.lock().expect("lock");

    // flock is per-process, so exclusion has to be checked from another one.
    let held = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "exec 9<>{} && flock -n 9 && echo free || echo busy",
            shell_quote(&lock_path)
        ))
        .output();
    match held {
        Ok(output) if output.status.success() => {
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "busy");
        }
        // No `flock(1)` available — the reentrancy test still covers the guard.
        _ => {}
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

#[test]
fn slugify_and_normalize_are_reusable_helpers() {
    assert_eq!(slugify("Some Repo!"), "some-repo");
    assert_eq!(
        normalize_path(Path::new("/tmp/a/./b/../c")),
        PathBuf::from("/tmp/a/c")
    );
}

fn write_local_board(work: &Path, name: &str, marker: &str) {
    let kanban = work.join(".kanban");
    fs::create_dir_all(kanban.join("tasks/todo")).unwrap();
    fs::write(
        kanban.join("config.yaml"),
        format!("tui:\n  name: {name}\n"),
    )
    .unwrap();
    fs::write(kanban.join("tasks/todo/TASK-001.md"), marker).unwrap();
}

fn write_live_session(data_root: &Path) {
    Storage::new(data_root).init_board().unwrap();
    SessionManager::new(data_root)
        .save_session(&Session {
            id: "ses-live".into(),
            task_id: "TASK-001".into(),
            name: None,
            started_at: timefmt::now(),
            status: SessionStatus::Active,
            last_seen: timefmt::now(),
            ended_at: None,
            wait_until: None,
            wait_note: None,
            wait_exited: false,
        })
        .unwrap();
}

#[test]
fn add_moves_a_local_board_into_the_store() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    write_local_board(&work, "Legacy Board", "keep-me");

    let added = store.add(&work, None).expect("add");

    assert!(added.created);
    assert!(!work.join(".kanban").exists());
    let dest = added.project.kanban_dir().join("tasks/todo/TASK-001.md");
    assert_eq!(fs::read_to_string(dest).unwrap(), "keep-me");
    assert_eq!(added.project.name, "Legacy Board");
    assert_eq!(added.project.migrated_from, Some(work.join(".kanban")));
}

#[test]
fn add_copy_leaves_the_source_board() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    write_local_board(&work, "Kanban", "copied");

    let added = store
        .add_with(
            &work,
            None,
            AddOptions {
                copy: true,
                ..AddOptions::default()
            },
        )
        .expect("copy");

    assert!(work.join(".kanban/tasks/todo/TASK-001.md").exists());
    assert_eq!(
        fs::read_to_string(added.project.kanban_dir().join("tasks/todo/TASK-001.md")).unwrap(),
        "copied"
    );
}

#[test]
fn add_force_copy_uses_the_verified_copy_path() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    write_local_board(&work, "Kanban", "via-copy");

    let added = store
        .add_with(
            &work,
            None,
            AddOptions {
                force_copy: true,
                ..AddOptions::default()
            },
        )
        .expect("force copy");

    assert!(!work.join(".kanban").exists());
    assert_eq!(
        fs::read_to_string(added.project.kanban_dir().join("tasks/todo/TASK-001.md")).unwrap(),
        "via-copy"
    );
}

#[test]
fn add_refuses_while_a_session_is_live() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    write_local_board(&work, "Kanban", "x");
    write_live_session(&work);

    let err = store.add(&work, None).expect_err("live session");

    assert!(matches!(err, KanbanError::ActiveSessions(_)));
    assert!(work.join(".kanban/tasks/todo/TASK-001.md").exists());
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn add_force_overrides_a_live_session() {
    let (dir, store) = temp_store();
    let work = work_dir(dir.path(), "repo");
    write_local_board(&work, "Kanban", "forced");
    write_live_session(&work);

    let added = store
        .add_with(
            &work,
            None,
            AddOptions {
                force: true,
                ..AddOptions::default()
            },
        )
        .expect("force");

    assert!(!work.join(".kanban").exists());
    assert!(
        added
            .project
            .kanban_dir()
            .join("tasks/todo/TASK-001.md")
            .exists()
    );
}

#[test]
fn relocate_leaves_source_when_the_copy_fails() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("marker"), "data").unwrap();
    let dest = dir.path().join("dest");
    fs::write(&dest, "i-am-a-file").unwrap();

    let err = relocate_board(
        &src,
        &dest,
        &AddOptions {
            force_copy: true,
            ..AddOptions::default()
        },
    )
    .expect_err("dest is a file");

    assert!(matches!(err, KanbanError::Io(_) | KanbanError::Invalid(_)));
    assert_eq!(fs::read_to_string(src.join("marker")).unwrap(), "data");
}
