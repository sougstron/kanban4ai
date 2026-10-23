use super::*;
use crate::tui::tests::{app_with_board, key, render_snapshot};

fn ready(rx: &Receiver<AppEvent>) -> TickResult {
    match rx
        .recv_timeout(Duration::from_secs(10))
        .expect("worker reply")
    {
        AppEvent::TickReady(result) => *result,
        other => panic!("unexpected event: {other:?}"),
    }
}

fn background(app: &mut App) -> Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel();
    app.enable_background_launches(tx);
    assert!(app.tick_worker.is_some());
    rx
}

fn has_task(app: &App, id: &str) -> bool {
    app.board
        .columns
        .iter()
        .flat_map(|column| &column.tasks)
        .any(|task| task.id == id)
}

#[test]
fn tick_worker_stale_snapshot_keeps_user_action_and_applies_progress() {
    let (_dir, mut app) = app_with_board();
    let first = app
        .ops
        .create_task(NewTask::titled("Before refresh"))
        .unwrap();
    app.reload_board().unwrap();
    let rx = background(&mut app);
    assert!(app.request_tick_refresh(true));
    let mut result = ready(&rx);
    let later = app
        .ops
        .create_task(NewTask::titled("User action wins"))
        .unwrap();
    app.refresh_after_action().unwrap();
    let progress = SessionProgress {
        todos_done: 2,
        todos_total: 3,
        ..Default::default()
    };
    result.progress.insert(first.id.clone(), progress.clone());
    app.apply_tick_result(result).unwrap();
    assert!(has_task(&app, &later.id));
    assert_eq!(app.session_progress.get(&first.id), Some(&progress));
    // Rejected snapshots schedule one fresh read, rather than sticking stale forever.
    app.apply_tick_result(ready(&rx)).unwrap();
    assert!(has_task(&app, &later.id));
    assert!(!app.tick_in_flight);
}

#[test]
fn tick_worker_refresh_preserves_detail_editor_scroll_and_focus() {
    let (_dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Open detail")).unwrap();
    app.reload_board().unwrap();
    app.open_focused_detail().unwrap();
    let detail = app.detail.as_mut().unwrap();
    detail.scroll = 7;
    detail.focus = DetailFocus::Thread;
    detail.review_edits.insert_str("Unsaved local edit");
    let rx = background(&mut app);
    let added = app
        .ops
        .create_task(NewTask::titled("External card"))
        .unwrap();
    assert!(app.request_tick_refresh(false));
    app.apply_tick_result(ready(&rx)).unwrap();
    assert!(has_task(&app, &added.id));
    let detail = app.detail.as_ref().unwrap();
    assert_eq!(detail.task_id, task.id);
    assert_eq!(detail.scroll, 7);
    assert_eq!(detail.focus, DetailFocus::Thread);
    assert_eq!(textarea_text(&detail.review_edits), "Unsaved local edit");
}

#[test]
fn tick_worker_ignores_other_board_and_previous_visit() {
    let (_dir, mut app) = app_with_board();
    let rx = background(&mut app);
    for other_path in [true, false] {
        assert!(app.request_tick_refresh(true));
        let mut result = ready(&rx);
        result.progress.insert(
            "foreign".into(),
            SessionProgress {
                tokens: Some(99),
                ..Default::default()
            },
        );
        if other_path {
            result.path = app.ops.data_root().join("other-board");
        } else {
            result.worker_id = result.worker_id.wrapping_add(1);
        }
        app.apply_tick_result(result).unwrap();
        assert!(!app.session_progress.contains_key("foreign"));
        assert!(
            app.tick_in_flight,
            "foreign reply must not clear current request"
        );
        // We deliberately altered the only real reply above; no work remains.
        app.tick_in_flight = false;
    }
}

#[test]
fn tick_worker_blocked_refresh_leaves_navigation_responsive_and_coalesces() {
    let (_dir, mut app) = app_with_board();
    for title in ["First", "Second", "Third"] {
        app.ops.create_task(NewTask::titled(title)).unwrap();
    }
    app.reload_board().unwrap();
    let rx = background(&mut app);
    let (release, gate) = mpsc::channel();
    let (entered, started) = mpsc::channel();
    assert!(app.tick_worker.as_ref().unwrap().request(TickRequest {
        generation: app.board_generation,
        main_stamps: app.board.stamps,
        global_changed: false,
        live: tick_worker::live_tasks(&app.board),
        before_read: Some((gate, entered)),
    }));
    app.tick_in_flight = true;
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    app.handle_key(key(KeyCode::Down)).unwrap();
    assert_eq!(app.focused_card, 1, "key handled before worker is released");
    for _ in 0..10 {
        let generation = app.note_fs_changed();
        app.reload_debounced_change(generation).unwrap();
    }
    assert!(rx.try_recv().is_err());
    release.send(()).unwrap();
    app.apply_tick_result(ready(&rx)).unwrap();
    app.apply_tick_result(ready(&rx)).unwrap();
    assert!(!app.tick_in_flight);
    assert!(!app.tick_pending);
    assert_eq!(app.focused_card, 1);
    assert!(rx.try_recv().is_err(), "burst coalesced into one follow-up");
}

#[test]
fn tick_worker_snapshots_match_inline_board_and_live_tokens() {
    let (dir, mut app) = app_with_board();
    let mut task = app
        .ops
        .create_task(NewTask {
            title: "Live background refresh".into(),
            agent_backend: Some("claude".into()),
            ..Default::default()
        })
        .unwrap();
    {
        let _lock = app.ops.storage.lock().unwrap();
        task.session = Some("ses-tick-worker".into());
        app.ops.storage.save_task(&task).unwrap();
        SessionManager::new(dir.path())
            .link_session(&task.id, "ses-tick-worker")
            .unwrap();
    }
    app.ops.move_task(&task.id, "in_progress", false).unwrap();
    app.reload_board().unwrap();
    app.settings.project_name = "Tick worker".into();
    insta::assert_snapshot!("tick_worker_before", render_snapshot(&mut app));
    let rx = background(&mut app);
    let added = app
        .ops
        .create_task(NewTask::titled("Arrived from disk"))
        .unwrap();
    fs::write(dir.path().join(".kanban/logs/ses-tick-worker.transcript.jsonl"),
        "{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":1200,\"output_tokens\":300,\"cache_read_input_tokens\":800},\"content\":[{\"type\":\"tool_use\",\"name\":\"TodoWrite\",\"input\":{\"todos\":[{\"content\":\"Read\",\"status\":\"completed\"},{\"content\":\"Implement\",\"status\":\"in_progress\"}]}}]}}\n"
    ).unwrap();
    assert!(app.request_tick_refresh(false));
    let result = ready(&rx);
    assert!(result.error.is_none(), "{:?}", result.error);
    app.apply_tick_result(result).unwrap();
    assert!(has_task(&app, &added.id));
    assert_eq!(app.session_progress[&task.id].todos(), Some((1, 2)));
    assert_eq!(app.task_tokens[&task.id].input, 1200);
    assert_eq!(app.task_tokens[&task.id].cache_read, 800);
    assert_eq!(app.task_tokens[&task.id].output, 300);
    let after = render_snapshot(&mut app);
    insta::assert_snapshot!("tick_worker_after", after);

    let mut inline = App::new(dir.path()).unwrap();
    inline.settings.project_name = "Tick worker".into();
    inline.status = "Board updated from disk".into();
    inline.refresh_session_progress();
    assert_eq!(render_snapshot(&mut inline), after);
}

#[test]
fn tick_worker_unchanged_fs_event_keeps_action_status() {
    let (_dir, mut app) = app_with_board();
    let rx = background(&mut app);
    app.ops
        .create_task(NewTask::titled("Moved by user"))
        .unwrap();
    app.refresh_after_action().unwrap();
    app.status = "Moved TASK-001 to In Progress".into();
    // The watcher's debounced event for the action's own writes.
    let generation = app.note_fs_changed();
    app.reload_debounced_change(generation).unwrap();
    assert!(app.tick_in_flight);
    let result = ready(&rx);
    assert!(result.board.is_none(), "nothing changed since the action");
    assert!(!result.tasks_changed);
    app.apply_tick_result(result).unwrap();
    assert_eq!(app.status, "Moved TASK-001 to In Progress");
}

#[test]
fn tick_worker_heartbeat_does_not_reload_open_detail() {
    let (dir, mut app) = app_with_board();
    let mut task = app.ops.create_task(NewTask::titled("Watched")).unwrap();
    {
        let _lock = app.ops.storage.lock().unwrap();
        task.session = Some("ses-heartbeat".into());
        app.ops.storage.save_task(&task).unwrap();
        SessionManager::new(dir.path())
            .link_session(&task.id, "ses-heartbeat")
            .unwrap();
    }
    app.reload_board().unwrap();
    app.open_focused_detail().unwrap();
    assert_eq!(app.detail.as_ref().unwrap().task_id, task.id);
    let state = app.detail_session_state();
    assert!(state.is_some());
    // load_detail would reset this: the task has no prompt dump.
    app.detail.as_mut().unwrap().has_prompt = true;
    let rx = background(&mut app);
    std::thread::sleep(Duration::from_millis(20));
    SessionManager::new(dir.path())
        .heartbeat("ses-heartbeat")
        .unwrap();
    let generation = app.note_fs_changed();
    app.reload_debounced_change(generation).unwrap();
    let result = ready(&rx);
    assert!(!result.tasks_changed);
    assert!(result.board.is_some(), "session stamp changed");
    app.apply_tick_result(result).unwrap();
    assert_eq!(app.detail_session_state(), state);
    assert!(
        app.detail.as_ref().unwrap().has_prompt,
        "heartbeat must not re-read the detail on the UI thread"
    );
}
