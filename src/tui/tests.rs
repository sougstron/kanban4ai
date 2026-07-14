use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::core::operations::Operations;
use crate::core::session::SessionManager;
use crate::core::storage::{NewTask, Storage};
use crate::core::thread::ThreadManager;

use super::app::{App, Screen};
use super::board;
use super::dialogs::{DialogField, Modal};
use super::theme::Theme;

fn app_with_board() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: false\n",
    )
    .expect("quiet config");
    let app = App::new(dir.path()).expect("create app");
    (dir, app)
}

fn populated_app() -> (tempfile::TempDir, App) {
    let (dir, mut app) = app_with_board();
    let ops = Operations::new(dir.path());
    let first = ops
        .create_task(NewTask {
            title: "Question card".to_string(),
            description: "Needs an answer".to_string(),
            interactive: true,
            ..Default::default()
        })
        .expect("create question task");
    ops.ask_question(
        &first.id,
        "Choose a route?",
        "agent",
        vec!["Fast path".to_string(), "Safe path".to_string()],
    )
    .expect("ask question");
    let second = ops
        .create_task(NewTask {
            title: "Implement selectors".to_string(),
            description: "Dropdown work".to_string(),
            agent_backend: Some("claude".to_string()),
            ai_model: Some("sonnet".to_string()),
            ..Default::default()
        })
        .expect("create second task");
    ops.move_task(&second.id, "review", false)
        .expect("move task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    (dir, app)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn render_snapshot(app: &mut App) -> String {
    let backend = TestBackend::new(96, 28);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| board::ui(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    format!(
        "{}\n\n--- style runs ---\n{}",
        buffer_to_string(buffer),
        style_runs(buffer)
    )
}

fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            let line = (0..area.width)
                .map(|x| buffer.cell((x, y)).expect("cell").symbol())
                .collect::<String>();
            normalize_elapsed(line, area.width as usize)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_elapsed(line: String, width: usize) -> String {
    let timestamp = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?")
        .expect("static timestamp regex");
    let mut line = timestamp.replace_all(&line, "<timestamp>").into_owned();
    if let Some(index) = line.find("│ refreshed ") {
        line.truncate(index);
        line.push_str("│ refreshed <elapsed>");
        let display_width = unicode_width::UnicodeWidthStr::width(line.as_str());
        line.push_str(&" ".repeat(width.saturating_sub(display_width)));
    }
    line
}

fn style_runs(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut runs = Vec::new();
    for y in 0..area.height {
        let mut start = 0;
        let mut style = buffer.cell((0, y)).expect("cell").style();
        for x in 1..=area.width {
            let next = (x < area.width).then(|| buffer.cell((x, y)).expect("cell").style());
            if next != Some(style) {
                if style != ratatui::style::Style::default() {
                    runs.push(format!("{y}:{start}-{x} {style:?}"));
                }
                if let Some(next) = next {
                    start = x;
                    style = next;
                }
            }
        }
    }
    runs.join("\n")
}

#[test]
fn renders_empty_and_populated_boards_in_both_themes() {
    let (_dir, mut app) = app_with_board();
    insta::assert_snapshot!("empty_board_dark", render_snapshot(&mut app));

    app.theme = Theme::named("light");
    insta::assert_snapshot!("empty_board_light", render_snapshot(&mut app));

    let (_dir, mut app) = populated_app();
    insta::assert_snapshot!("populated_board", render_snapshot(&mut app));
}

#[test]
fn renders_detail_search_and_every_modal() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Enter)).expect("detail");
    insta::assert_snapshot!("detail_thread", render_snapshot(&mut app));

    app.handle_key(key(KeyCode::Esc)).expect("back");
    app.handle_key(key(KeyCode::Char('/')))
        .expect("search open");
    app.handle_key(key(KeyCode::Char('Q')))
        .expect("search input");
    insta::assert_snapshot!("search_active", render_snapshot(&mut app));
    app.handle_key(key(KeyCode::Esc)).expect("close search");

    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    insta::assert_snapshot!("modal_new", render_snapshot(&mut app));
    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('e'))).unwrap();
    insta::assert_snapshot!("modal_edit", render_snapshot(&mut app));
    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('m'))).unwrap();
    insta::assert_snapshot!("modal_move", render_snapshot(&mut app));
    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('d'))).unwrap();
    insta::assert_snapshot!("modal_delete", render_snapshot(&mut app));
    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('s'))).unwrap();
    insta::assert_snapshot!("modal_delegate", render_snapshot(&mut app));
    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('w'))).unwrap();
    insta::assert_snapshot!("modal_answer", render_snapshot(&mut app));
    app.handle_key(key(KeyCode::Esc)).unwrap();
}

#[test]
fn task_form_move_and_answer_selectors_follow_keyboard_contract() {
    let (_dir, mut app) = populated_app();

    app.handle_key(key(KeyCode::Char('n'))).expect("new");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.active_field(), DialogField::Title);
    assert!(
        modal
            .backend_options
            .iter()
            .any(|option| option.value.as_deref() == Some("opencode"))
    );
    assert!(
        modal
            .model_options
            .iter()
            .any(|option| option.value.as_deref() == Some("openai/gpt-5.5"))
    );
    app.handle_key(key(KeyCode::Tab)).expect("description");
    app.handle_key(key(KeyCode::Tab)).expect("backend");
    app.handle_key(key(KeyCode::Down)).expect("select backend");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.active_field(), DialogField::Backend);
    assert_eq!(modal.backend_text().as_deref(), Some("claude"));
    assert!(
        modal
            .model_options
            .iter()
            .any(|option| option.value.as_deref() == Some("sonnet"))
    );
    assert!(
        modal
            .agent_options
            .iter()
            .all(|option| option.value.is_none())
    );
    app.handle_key(key(KeyCode::BackTab)).expect("backtab");
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::Description
    );
    app.handle_key(key(KeyCode::Esc)).expect("close");

    app.handle_key(key(KeyCode::Char('m'))).expect("move");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(
        modal.modal,
        Modal::MoveTask {
            task_id: "TASK-001".to_string()
        }
    );
    assert!(
        modal
            .status_options
            .iter()
            .any(|option| option.value.as_deref() == Some("review"))
    );
    app.handle_key(key(KeyCode::Esc)).expect("close");

    app.handle_key(key(KeyCode::Char('w'))).expect("answer");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.active_field(), DialogField::Question);
    app.handle_key(key(KeyCode::Tab)).expect("variants");
    app.handle_key(key(KeyCode::Down)).expect("first variant");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.answer_text(), "Fast path");
}

#[test]
fn mouse_click_focuses_card_and_second_click_opens_detail() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let hitbox = app.card_hitboxes[1];
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hitbox.area.x + 1,
        row: hitbox.area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(click).expect("focus");
    assert_eq!(app.focused_column, hitbox.column);
    assert_eq!(app.focused_card, hitbox.card);
    assert_eq!(app.screen, Screen::Board);
    app.handle_mouse(click).expect("detail");
    assert_eq!(app.screen, Screen::Detail);
}

#[test]
fn modal_and_help_block_mouse_click_through() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let hitbox = app.card_hitboxes[1];
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hitbox.area.x + 1,
        row: hitbox.area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    let original_focus = (app.focused_column, app.focused_card);
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    app.handle_mouse(click).unwrap();
    assert!(app.modal.is_some());
    assert_eq!((app.focused_column, app.focused_card), original_focus);
    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('?'))).unwrap();
    app.handle_mouse(click).unwrap();
    assert_eq!(app.screen, Screen::Help);
    assert_eq!((app.focused_column, app.focused_card), original_focus);
}

#[test]
fn review_editor_navigation_and_thread_focus_are_distinct() {
    let (_dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Edit review")).unwrap();
    app.ops.set_review_edits(&task.id, "abcdef").unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    app.handle_key(key(KeyCode::End)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().review_edits.cursor(), (0, 6));
    app.handle_key(key(KeyCode::Left)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().review_edits.cursor(), (0, 5));

    app.handle_key(key(KeyCode::Tab)).unwrap();
    let cursor = app.detail.as_ref().unwrap().review_edits.cursor();
    app.handle_key(key(KeyCode::Down)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().review_edits.cursor(), cursor);
    assert_eq!(app.detail.as_ref().unwrap().scroll, 1);
}

#[test]
fn review_edits_save_and_rerun_from_detail() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Review this"))
        .expect("create task");
    app.ops
        .move_task(&task.id, "review", false)
        .expect("move to review");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = app
        .board
        .columns
        .iter()
        .position(|column| column.id == "review")
        .expect("review column");
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.detail
        .as_mut()
        .expect("detail")
        .review_edits
        .insert_str("Please tighten validation");

    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .expect("save and rerun");

    let rerun = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(rerun.status.as_str(), "in_progress");
    assert!(rerun.review_edits.is_empty());
    assert_eq!(app.screen, Screen::Board);
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|message| message.body == "Please tighten validation")
    );
}

#[test]
fn stale_debounce_is_ignored_and_thread_change_refreshes_detail() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Refresh me"))
        .expect("create task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    let initial_messages = app.detail.as_ref().unwrap().messages.len();

    let stale = app.note_fs_changed();
    ThreadManager::new(dir.path())
        .unwrap()
        .post(
            &task.id,
            crate::core::models::MessageRole::Agent,
            crate::core::models::MessageKind::Context,
            "fresh context",
            None,
            vec![],
            None,
        )
        .unwrap();
    let current = app.note_fs_changed();
    app.reload_debounced_change(stale).expect("ignore stale");
    assert_eq!(
        app.detail.as_ref().unwrap().messages.len(),
        initial_messages
    );
    app.reload_debounced_change(current)
        .expect("refresh current");
    assert_eq!(
        app.detail.as_ref().unwrap().messages.len(),
        initial_messages + 1
    );
}

#[test]
fn sessions_enter_requests_terminal_attach() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Running"))
        .expect("create task");
    SessionManager::new(dir.path())
        .link_session(&task.id, "ses-tmux-live")
        .expect("link session");

    app.handle_key(key(KeyCode::Char('l')))
        .expect("open sessions");
    app.handle_key(key(KeyCode::Enter)).expect("request attach");
    assert_eq!(app.take_attach_request().as_deref(), Some("ses-tmux-live"));
}

#[test]
fn archive_enter_opens_selected_archived_task() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Archived detail"))
        .expect("create task");
    app.ops
        .move_task(&task.id, "archive", false)
        .expect("archive task");

    app.handle_key(key(KeyCode::Char('a')))
        .expect("open archive");
    app.handle_key(key(KeyCode::Enter))
        .expect("open archived detail");

    assert_eq!(app.screen, Screen::Detail);
    assert_eq!(app.detail.as_ref().unwrap().task_id, task.id);
}
