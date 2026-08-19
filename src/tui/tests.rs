use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::{Modifier, Style};
use ratatui_textarea::{TextArea, WrapMode};

use crate::core::operations::Operations;
use crate::core::project::ProjectStore;
use crate::core::session::SessionManager;
use crate::core::storage::{NewTask, Storage};
use crate::core::thread::ThreadManager;

use super::app::{
    App, DetailFocus, HitAction, Screen, UiAction, load_log_tail, normalize_command_key,
};
use super::board;
use super::dialogs::{DialogField, Modal, ModalButton, ModalState};
use super::event::LoopOutcome;
use super::theme::Theme;

fn app_with_board() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    // Point opencode at a nonexistent binary so option lists come from the
    // configured `models` fallback instead of the machine's live catalog.
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: false\nagents:\n  opencode:\n    command: /nonexistent/opencode-disabled-for-tests\n",
    )
    .expect("quiet config");
    let app = App::new(dir.path()).expect("create app");
    (dir, app)
}

#[test]
fn log_tail_rejects_invalid_session_id() {
    let (dir, _app) = app_with_board();
    std::fs::write(dir.path().join("outside.log"), "secret log contents").expect("outside log");

    assert_eq!(
        load_log_tail(dir.path(), "../outside"),
        vec!["(invalid session id)".to_string()]
    );
}

#[test]
fn task_description_soft_wraps_and_preserves_data_cursor_after_resize() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    let prose = "Soft wrapping keeps normal prose readable in a narrow terminal. ";
    let token = "unbroken".repeat(16);
    let description = format!("{prose}{token}");
    {
        let modal = app.modal.as_mut().expect("new task modal");
        modal.focus_field(DialogField::Description);
        modal.description.insert_str(&description);
        modal.description.input(key(KeyCode::Home));
    }

    let prose_view = render_at(&mut app, 72, 40);
    assert!(prose_view.contains("Soft wrapping"));
    app.modal
        .as_mut()
        .expect("modal")
        .description
        .input(key(KeyCode::End));

    let _ = render_at(&mut app, 160, 48);
    let logical_cursor = app.modal.as_ref().expect("modal").description.cursor();
    let wide_cursor = app
        .modal
        .as_ref()
        .expect("modal")
        .description
        .screen_cursor();
    let narrow = render_at(&mut app, 72, 40);
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(
        modal.description.lines(),
        std::slice::from_ref(&description)
    );
    assert_eq!(modal.description.cursor(), logical_cursor);
    assert!(modal.description.screen_cursor().row > wide_cursor.row);
    assert!(narrow.contains("unbroken"));
    assert!(!narrow.contains(&token), "long token must be glyph-wrapped");

    let narrow_cursor = modal.description.screen_cursor();
    let modal = app.modal.as_mut().expect("modal");
    modal.description.input(key(KeyCode::Up));
    assert_eq!(
        modal.description.screen_cursor().row + 1,
        narrow_cursor.row,
        "Up must move by one visual wrapped row"
    );
    assert_eq!(
        modal.description.lines(),
        std::slice::from_ref(&description)
    );
    modal.description.input(key(KeyCode::Down));
    assert_eq!(modal.description.cursor(), logical_cursor);
    modal
        .description
        .input(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
    assert!(modal.description.selection_range().is_some());
    assert_eq!(modal.description.lines(), [description]);
}

#[test]
fn task_description_height_is_bounded_and_other_editors_remain_unwrapped() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    let _ = render_at(&mut app, 120, 60);

    let description = modal_hitbox(&app, HitAction::ModalField(DialogField::Description));
    assert_eq!(description.height, 10);
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.description.wrap_mode(), WrapMode::WordOrGlyph);
    assert_eq!(modal.title.wrap_mode(), WrapMode::None);
    assert_eq!(modal.answer.wrap_mode(), WrapMode::None);
    assert_eq!(app.search.query.wrap_mode(), WrapMode::None);

    let add_message = ModalState::new(Modal::AddMessage {
        task_id: "TASK-001".to_string(),
    });
    assert_eq!(add_message.description.wrap_mode(), WrapMode::None);
}

#[test]
fn constrained_task_form_keeps_description_and_buttons_separate() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::Description);

    let _ = render_at(&mut app, 60, 18);
    let description = modal_hitbox(&app, HitAction::ModalField(DialogField::Description));
    assert!((5..=10).contains(&description.height));
    let save = modal_hitbox(&app, HitAction::ModalButton(ModalButton::Save));
    let cancel = modal_hitbox(&app, HitAction::ModalButton(ModalButton::Cancel));
    assert!(!overlaps(description, save));
    assert!(!overlaps(description, cancel));
    let _ = render_at(&mut app, 24, 8);
}

fn settings_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        r#"tui:
  name: Existing project
  theme: textual-light
auto_launch:
  enabled: false
  default_agent: opencode
  model: legacy-model-must-stay
  agent: legacy-agent-must-stay
agents:
  opencode:
    command: /nonexistent/opencode-disabled-for-tests
    model: openai/gpt-5.5
    models: [openai/gpt-5.5]
    effort: high
    agent: sisyphus
    agent_options: [sisyphus, prometheus]
  claude:
    command: claude
    model: sonnet
    models: [fable, sonnet]
    effort: medium
    efforts: [low, medium, high]
    agent: null
"#,
    )
    .expect("settings config");
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

fn ctrl_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

#[cfg(unix)]
fn make_sleeping_opencode(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let command = dir.join("slow-opencode");
    std::fs::write(&command, "#!/bin/sh\nsleep 2\nexit 0\n").expect("write fake opencode");
    let mut permissions = std::fs::metadata(&command)
        .expect("fake opencode metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&command, permissions).expect("chmod fake opencode");
    command
}

#[cfg(unix)]
fn make_marker_opencode(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let command = dir.join("marker-opencode");
    let marker = dir.join("opencode-started");
    std::fs::write(
        &command,
        format!("#!/bin/sh\ntouch {}\nsleep 2\nexit 0\n", marker.display()),
    )
    .expect("write marker opencode");
    let mut permissions = std::fs::metadata(&command)
        .expect("marker opencode metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&command, permissions).expect("chmod marker opencode");
    (command, marker)
}

#[cfg(unix)]
fn make_catalog_opencode(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let command = dir.join(format!(
        "catalog-opencode-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    let marker = dir.join("opencode-catalog-started");
    std::fs::write(
        &command,
        format!(
            "#!/bin/sh\ntouch {}\ncat <<'EOF'\nopenai/gpt-5.5\n{{\n  \"variants\": {{\n    \"high\": {{\n      \"reasoningEffort\": \"high\"\n    }}\n  }}\n}}\nopencode-go/minimax-m3\n{{\n  \"variants\": {{}}\n}}\nEOF\n",
            marker.display()
        ),
    )
    .expect("write catalog opencode");
    let mut permissions = std::fs::metadata(&command)
        .expect("catalog opencode metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&command, permissions).expect("chmod catalog opencode");
    (command, marker)
}

/// Card regions from the hitbox registry as `(column, card, area)` triples.
fn card_hits(app: &App) -> Vec<(usize, usize, ratatui::layout::Rect)> {
    app.hitboxes
        .iter()
        .filter_map(|hitbox| match hitbox.action {
            HitAction::FocusCard { column, card } => Some((column, card, hitbox.area)),
            _ => None,
        })
        .collect()
}

fn render_snapshot(app: &mut App) -> String {
    render_at(app, 96, 28)
}

fn render_at(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| board::ui(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    format!(
        "{}\n\n--- style runs ---\n{}",
        buffer_to_string(buffer),
        style_runs(buffer)
    )
}

fn modal_hitbox(app: &App, action: HitAction) -> ratatui::layout::Rect {
    app.hitboxes
        .iter()
        .find(|hitbox| hitbox.action == action)
        .expect("modal hitbox")
        .area
}

fn overlaps(left: ratatui::layout::Rect, right: ratatui::layout::Rect) -> bool {
    left.x < right.x.saturating_add(right.width)
        && right.x < left.x.saturating_add(left.width)
        && left.y < right.y.saturating_add(right.height)
        && right.y < left.y.saturating_add(left.height)
}

fn style_at(app: &mut App, width: u16, height: u16, x: u16, y: u16) -> Style {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| board::ui(frame, app)).expect("draw");
    terminal
        .backend()
        .buffer()
        .cell((x, y))
        .expect("cell")
        .style()
}

fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            let line = (0..area.width)
                .map(|x| buffer.cell((x, y)).expect("cell").symbol())
                .collect::<String>();
            normalize_elapsed(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_elapsed(line: String) -> String {
    let timestamp = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?")
        .expect("static timestamp regex");
    timestamp.replace_all(&line, "<timestamp>").into_owned()
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

#[cfg(unix)]
#[test]
fn new_task_dialog_does_not_wait_for_live_opencode_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    let command = make_sleeping_opencode(dir.path());
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        format!(
            "notifications:\n  enabled: false\nauto_launch:\n  enabled: false\n  default_agent: opencode\n  model: openai/gpt-5.5\n  models: [openai/gpt-5.5]\nagents:\n  opencode:\n    command: {}\n    model: openai/gpt-5.5\n    models: [openai/gpt-5.5]\n",
            command.display()
        ),
    )
    .expect("slow opencode config");

    let mut app = App::new(dir.path()).expect("create app");
    let started = Instant::now();
    app.handle_key(key(KeyCode::Char('n')))
        .expect("open new task");

    assert!(
        started.elapsed() < Duration::from_millis(100),
        "new task dialog waited for opencode catalog"
    );
    assert!(matches!(
        app.modal.as_ref().map(|modal| &modal.modal),
        Some(Modal::NewTask { .. })
    ));
}

#[cfg(unix)]
#[test]
fn new_task_dialog_does_not_start_live_opencode_catalog_probe() {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    let (command, marker) = make_marker_opencode(dir.path());
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        format!(
            "notifications:\n  enabled: false\nauto_launch:\n  enabled: false\n  default_agent: opencode\n  model: openai/gpt-5.5\n  models: [openai/gpt-5.5]\nagents:\n  opencode:\n    command: {}\n    model: openai/gpt-5.5\n    models: [openai/gpt-5.5]\n",
            command.display()
        ),
    )
    .expect("marker opencode config");

    let mut app = App::new(dir.path()).expect("create app");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !marker.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(marker.exists(), "startup did not start the catalog warmup");
    std::fs::remove_file(&marker).expect("clear startup marker");

    app.handle_key(key(KeyCode::Char('n')))
        .expect("open new task");
    std::thread::sleep(Duration::from_millis(150));

    assert!(
        !marker.exists(),
        "opening a new task started the live opencode catalog probe"
    );
}

#[cfg(unix)]
#[test]
fn startup_warms_live_opencode_catalog_without_blocking_dialogs() {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    let (command, marker) = make_catalog_opencode(dir.path());
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        format!(
            "notifications:\n  enabled: false\nauto_launch:\n  enabled: false\n  default_agent: opencode\n  model: openai/gpt-5.5\n  models: [fallback/model]\nagents:\n  opencode:\n    command: {}\n    model: openai/gpt-5.5\n    models: [fallback/model]\n",
            command.display()
        ),
    )
    .expect("catalog opencode config");

    let started = Instant::now();
    let mut app = App::new(dir.path()).expect("create app");
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "App::new waited for opencode catalog"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !marker.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(marker.exists(), "startup did not warm the opencode catalog");

    app.handle_key(key(KeyCode::Char('n')))
        .expect("open new task");
    app.tick().expect("refresh warm catalog");
    let values = app
        .modal
        .as_ref()
        .expect("modal")
        .model_options
        .iter()
        .map(|option| option.value.as_deref())
        .collect::<Vec<_>>();

    assert!(values.contains(&Some("openai/gpt-5.5")));
    assert!(values.contains(&Some("opencode-go/minimax-m3")));
    assert!(!values.contains(&Some("fallback/model")));
}

#[test]
fn new_task_dialog_uses_recent_models_cached_at_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    std::fs::write(dir.path().join(".kanban/recent_models"), "z-model\n")
        .expect("initial recent models");
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: false\n  default_agent: opencode\nagents:\n  opencode:\n    command: /nonexistent/opencode-disabled-for-tests\n    models: [a-model, m-model, z-model]\n",
    )
    .expect("config");

    let mut app = App::new(dir.path()).expect("create app");
    std::fs::write(dir.path().join(".kanban/recent_models"), "a-model\n")
        .expect("changed recent models");

    app.handle_key(key(KeyCode::Char('n')))
        .expect("open new task");
    let model_values = app
        .modal
        .as_ref()
        .expect("new-task modal")
        .model_options
        .iter()
        .filter_map(|option| option.value.as_deref())
        .collect::<Vec<_>>();

    assert_eq!(model_values.get(1), Some(&"z-model"));
}

#[test]
fn renders_detail_search_and_every_modal() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Enter)).expect("detail");
    insta::assert_snapshot!("detail_thread", render_snapshot(&mut app));

    app.handle_key(key(KeyCode::Char('q'))).expect("back");
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
    app.handle_key(key(KeyCode::Char('c'))).unwrap();
    insta::assert_snapshot!("modal_add_message", render_snapshot(&mut app));
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
    assert_eq!(modal.backend_options[0].value, None);
    assert_eq!(modal.backend_options[0].label, "Default backend (opencode)");
    assert_eq!(modal.backend_text(), None);
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
    assert_eq!(modal.backend_text().as_deref(), Some("opencode"));
    app.handle_key(key(KeyCode::Down)).expect("select claude");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.active_field(), DialogField::Backend);
    assert_eq!(modal.backend_text().as_deref(), Some("claude"));
    for claude_model in ["fable", "opus", "sonnet", "haiku"] {
        assert!(
            modal
                .model_options
                .iter()
                .any(|option| option.value.as_deref() == Some(claude_model)),
            "missing claude model {claude_model}"
        );
    }
    let claude_efforts: Vec<Option<&str>> = modal
        .effort_options
        .iter()
        .map(|option| option.value.as_deref())
        .collect();
    assert_eq!(
        claude_efforts,
        [
            None,
            Some("low"),
            Some("medium"),
            Some("high"),
            Some("xhigh"),
            Some("max")
        ]
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
    app.handle_key(key(KeyCode::Esc)).expect("discard prompt");
    app.handle_key(key(KeyCode::Char('y'))).expect("discard");

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
fn task_form_default_backend_inherits_settings_agent() {
    let (_dir, mut app) = populated_app();

    // Create a task without touching the backend selector: the task must
    // stay unset so the launch path resolves auto_launch.default_agent.
    app.handle_key(key(KeyCode::Char('n'))).expect("new");
    let modal = app.modal.as_mut().expect("modal");
    modal.title.insert_str("Follows settings");
    modal.field_index = modal.fields().len() - 2;
    app.handle_key(key(KeyCode::Enter)).expect("save task");
    let created_id = app.board.columns[0]
        .tasks
        .iter()
        .find(|task| task.title == "Follows settings")
        .map(|task| task.id.clone())
        .expect("created task");
    let task = app
        .ops
        .get_task(&created_id)
        .expect("load task")
        .expect("task present");
    assert_eq!(task.agent_backend, None);

    // Editing an unset task keeps Default selected and saving keeps it unset.
    app.focused_column = 0;
    app.focused_card = app.board.columns[0]
        .tasks
        .iter()
        .position(|task| task.id == created_id)
        .expect("created task index");
    app.handle_key(key(KeyCode::Char('e'))).expect("edit");
    let modal = app.modal.as_ref().expect("edit modal");
    assert_eq!(modal.backend_options[0].value, None);
    assert_eq!(modal.backend_text(), None);
    let modal = app.modal.as_mut().expect("edit modal");
    modal.field_index = modal.fields().len() - 2;
    app.handle_key(key(KeyCode::Enter)).expect("save edit");
    let task = app
        .ops
        .get_task(&created_id)
        .expect("reload")
        .expect("task");
    assert_eq!(task.agent_backend, None);

    // A task with a pinned backend can be switched back to Default, which
    // clears the pin instead of pinning the current default agent.
    app.focused_column = 2;
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Char('e')))
        .expect("edit claude task");
    let modal = app.modal.as_ref().expect("edit modal");
    assert_eq!(modal.backend_text().as_deref(), Some("claude"));
    app.handle_key(key(KeyCode::Tab)).expect("description");
    app.handle_key(key(KeyCode::Tab)).expect("backend");
    app.handle_key(key(KeyCode::Up)).expect("to opencode");
    app.handle_key(key(KeyCode::Up)).expect("to default");
    assert_eq!(app.modal.as_ref().expect("modal").backend_text(), None);
    let modal = app.modal.as_mut().expect("edit modal");
    modal.field_index = modal.fields().len() - 2;
    app.handle_key(key(KeyCode::Enter)).expect("save edit");
    let task = app.ops.get_task("TASK-002").expect("reload").expect("task");
    assert_eq!(task.agent_backend, None);
}

#[test]
fn settings_hotkey_navigates_fields_and_reloads_backend_defaults() {
    let (_dir, mut app) = settings_app();
    assert_eq!(app.settings.theme_name, "light", "legacy theme normalizes");

    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let modal = app.modal.as_ref().expect("settings modal");
    assert_eq!(modal.modal, Modal::Settings);
    assert_eq!(modal.active_field(), DialogField::Title);
    assert_eq!(modal.title_text(), "Existing project");
    assert_eq!(modal.backend_text().as_deref(), Some("opencode"));

    app.handle_key(key(KeyCode::Tab)).expect("backend field");
    app.handle_key(key(KeyCode::Down)).expect("choose claude");
    let modal = app.modal.as_ref().expect("settings modal");
    assert_eq!(modal.active_field(), DialogField::Backend);
    assert_eq!(modal.backend_text().as_deref(), Some("claude"));
    assert_eq!(modal.model_text().as_deref(), Some("sonnet"));
    assert_eq!(modal.effort_text().as_deref(), Some("medium"));
    assert_eq!(modal.agent_text(), None);
    assert_eq!(modal.agent_options[0].label, "No default agent");

    app.handle_key(key(KeyCode::Tab)).expect("model field");
    app.handle_key(key(KeyCode::Tab)).expect("effort field");
    assert_eq!(
        app.modal.as_ref().unwrap().active_field(),
        DialogField::Effort
    );
    assert_eq!(
        app.modal.as_ref().unwrap().effort_options[0].label,
        "No default effort"
    );

    app.handle_key(key(KeyCode::Esc))
        .expect("discard confirmation");
    assert!(app.modal.as_ref().unwrap().discard_confirm);
    app.handle_key(key(KeyCode::Char('n')))
        .expect("keep editing");
    assert!(app.modal.is_some());
    app.handle_key(key(KeyCode::Esc))
        .expect("discard confirmation again");
    app.handle_key(key(KeyCode::Char('y')))
        .expect("discard settings");
    assert!(app.modal.is_none());

    app.ops
        .create_task(NewTask::titled("Detail settings"))
        .expect("task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings from detail");
    assert!(matches!(
        app.modal.as_ref().map(|modal| &modal.modal),
        Some(Modal::Settings)
    ));
}

#[test]
fn settings_save_persists_effective_keys_clears_nulls_and_applies_theme() {
    let (_dir, mut app) = settings_app();
    assert_eq!(app.settings.task_sort, "task_number");
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    assert_eq!(
        app.modal
            .as_ref()
            .unwrap()
            .task_sort_options
            .iter()
            .filter_map(|option| option.value.as_deref())
            .collect::<Vec<_>>(),
        vec!["task_number", "updated_at_asc", "updated_at_desc"]
    );
    {
        let modal = app.modal.as_mut().expect("settings modal");
        modal.title = TextArea::new(vec!["Renamed project".to_string()]);
    }
    app.handle_key(key(KeyCode::Tab)).expect("backend");
    app.handle_key(key(KeyCode::Down)).expect("claude");
    app.handle_key(key(KeyCode::Tab)).expect("model");
    app.handle_key(key(KeyCode::Left)).expect("clear model");
    app.handle_key(key(KeyCode::Left)).expect("clear model");
    app.handle_key(key(KeyCode::Tab)).expect("effort");
    app.handle_key(key(KeyCode::Left)).expect("clear effort");
    app.handle_key(key(KeyCode::Left)).expect("clear effort");
    app.handle_key(key(KeyCode::Tab)).expect("agent");
    app.handle_key(key(KeyCode::Tab)).expect("theme");
    app.handle_key(key(KeyCode::Left)).expect("dark theme");
    app.handle_key(key(KeyCode::Tab)).expect("task sorting");
    app.handle_key(key(KeyCode::Right))
        .expect("updated ascending sorting");
    app.handle_key(key(KeyCode::Tab)).expect("save");
    app.handle_key(key(KeyCode::Enter)).expect("save settings");

    assert!(app.modal.is_none());
    assert_eq!(app.settings.project_name, "Renamed project");
    assert_eq!(app.settings.theme_name, "dark");
    assert_eq!(app.settings.task_sort, "updated_at_asc");
    assert_eq!(app.theme.bg, Theme::named("dark").bg);

    let config = app.ops.config.load().expect("cached saved config");
    assert_eq!(
        config.tui.get("name").and_then(|value| value.as_str()),
        Some("Renamed project")
    );
    assert_eq!(
        config.tui.get("theme").and_then(|value| value.as_str()),
        Some("dark")
    );
    assert_eq!(
        config.tui.get("task_sort").and_then(|value| value.as_str()),
        Some("updated_at_asc")
    );
    assert_eq!(
        config
            .auto_launch
            .get("default_agent")
            .and_then(|value| value.as_str()),
        Some("claude")
    );
    assert_eq!(
        config
            .auto_launch
            .get("model")
            .and_then(|value| value.as_str()),
        Some("legacy-model-must-stay")
    );
    assert_eq!(
        config
            .auto_launch
            .get("agent")
            .and_then(|value| value.as_str()),
        Some("legacy-agent-must-stay")
    );
    let claude = config
        .agents
        .get("claude")
        .and_then(|value| value.as_mapping())
        .unwrap();
    for key in ["model", "effort", "agent"] {
        assert!(
            claude.get(key).is_some_and(serde_yaml_ng::Value::is_null),
            "{key} must be null"
        );
    }
}

#[test]
fn updated_sort_setting_applies_both_directions_to_every_column() {
    let (_dir, mut app) = settings_app();
    let mut expected_by_column = Vec::new();
    for status in ["todo", "in_progress", "review", "done"] {
        let older = app
            .ops
            .create_task(NewTask::titled(format!("Older {status}")))
            .unwrap();
        let newer = app
            .ops
            .create_task(NewTask::titled(format!("Newer {status}")))
            .unwrap();
        if status != "todo" {
            app.ops.move_task(&older.id, status, false).unwrap();
            app.ops.move_task(&newer.id, status, false).unwrap();
        }
        let mut older_task = app.ops.get_task(&older.id).unwrap().unwrap();
        older_task.updated_at = crate::core::timefmt::parse("2026-07-17T10:00:00").unwrap();
        app.ops.storage.save_task(&older_task).unwrap();
        let mut newer_task = app.ops.get_task(&newer.id).unwrap().unwrap();
        newer_task.updated_at = crate::core::timefmt::parse("2026-07-18T10:00:00").unwrap();
        app.ops.storage.save_task(&newer_task).unwrap();
        expected_by_column.push((older.id, newer.id));
    }

    let mut config = app.ops.config.load_fresh().unwrap();
    config.tui.insert(
        serde_yaml_ng::Value::String("task_sort".to_string()),
        serde_yaml_ng::Value::String("updated_at_asc".to_string()),
    );
    app.ops.config.save(&config).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    assert_eq!(app.board.columns.len(), expected_by_column.len());
    for (column, (older, newer)) in app.board.columns.iter().zip(&expected_by_column) {
        assert_eq!(column.tasks[0].id, *older);
        assert_eq!(column.tasks[1].id, *newer);
    }

    config.tui.insert(
        serde_yaml_ng::Value::String("task_sort".to_string()),
        serde_yaml_ng::Value::String("updated_at_desc".to_string()),
    );
    app.ops.config.save(&config).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    for (column, (older, newer)) in app.board.columns.iter().zip(&expected_by_column) {
        assert_eq!(column.tasks[0].id, *newer);
        assert_eq!(column.tasks[1].id, *older);
    }
}

#[test]
fn legacy_completion_sort_maps_to_updated_descending() {
    assert_eq!(
        super::app::normalize_task_sort("completion_date"),
        "updated_at_desc"
    );
}

#[test]
fn settings_preserves_custom_defaults_missing_from_selector_sources() {
    let (dir, mut app) = settings_app();
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        r#"tui:
  name: Custom defaults
  theme: dark
auto_launch:
  enabled: false
  default_agent: opencode
agents:
  opencode:
    command: /nonexistent/opencode-disabled-for-tests
    model: custom/model
    models: [listed/model]
    effort: custom-effort
    agent: custom-persona
    agent_options: [listed-persona]
"#,
    )
    .expect("external settings edit");

    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let modal = app.modal.as_ref().expect("settings modal");
    for (value, options) in [
        ("custom/model", &modal.model_options),
        ("custom-effort", &modal.effort_options),
        ("custom-persona", &modal.agent_options),
    ] {
        assert!(
            options
                .iter()
                .any(|option| option.value.as_deref() == Some(value)),
            "missing preserved option {value}"
        );
    }
    let save_field = app.modal.as_ref().unwrap().fields().len() - 2;
    app.modal.as_mut().unwrap().field_index = save_field;
    app.handle_key(key(KeyCode::Enter))
        .expect("save unchanged settings");

    let opencode = app
        .ops
        .config
        .load()
        .unwrap()
        .agents
        .get("opencode")
        .and_then(|value| value.as_mapping())
        .unwrap()
        .clone();
    assert_eq!(
        opencode.get("model").and_then(|value| value.as_str()),
        Some("custom/model")
    );
    assert_eq!(
        opencode.get("effort").and_then(|value| value.as_str()),
        Some("custom-effort")
    );
    assert_eq!(
        opencode.get("agent").and_then(|value| value.as_str()),
        Some("custom-persona")
    );
    let saved: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        &std::fs::read_to_string(dir.path().join(".kanban/config.yaml")).unwrap(),
    )
    .unwrap();
    let auto_launch = saved
        .as_mapping()
        .and_then(|mapping| mapping.get("auto_launch"))
        .and_then(|value| value.as_mapping())
        .unwrap();
    for key in ["model", "models", "agent"] {
        assert!(
            !auto_launch.contains_key(key),
            "settings must not serialize absent legacy auto_launch.{key}"
        );
    }
}

#[test]
fn settings_open_uses_fresh_external_config() {
    let (dir, mut app) = settings_app();
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        r#"tui:
  name: Fresh external project
  theme: textual-dark
auto_launch:
  enabled: false
  default_agent: claude
agents:
  claude:
    command: claude
    model: opus
    models: [sonnet, opus]
    effort: high
    efforts: [low, high]
    agent: null
"#,
    )
    .expect("external settings edit");

    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let modal = app.modal.as_ref().expect("settings modal");
    assert_eq!(modal.title_text(), "Fresh external project");
    assert_eq!(modal.backend_text().as_deref(), Some("claude"));
    assert_eq!(modal.model_text().as_deref(), Some("opus"));
    assert_eq!(modal.effort_text().as_deref(), Some("high"));
    assert_eq!(modal.theme_text().as_deref(), Some("dark"));
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        r#"tui:
  name: Fresh external project
  theme: textual-dark
auto_launch:
  enabled: false
  default_agent: claude
agents:
  claude:
    command: claude
    model: opus
    models: [sonnet, opus]
    effort: high
    efforts: [low, high]
    agent: null
external_integration:
  preserve_me: true
"#,
    )
    .expect("concurrent external edit");
    let save_field = app.modal.as_ref().unwrap().fields().len() - 2;
    app.modal.as_mut().unwrap().field_index = save_field;
    app.handle_key(key(KeyCode::Enter))
        .expect("save over fresh external edit");
    assert_eq!(
        app.ops
            .config
            .load()
            .unwrap()
            .extras
            .get("external_integration")
            .and_then(|value| value.get("preserve_me"))
            .and_then(|value| value.as_bool()),
        Some(true)
    );
}

#[test]
fn settings_symlink_refusal_preserves_modal_and_staged_values() {
    let (dir, mut app) = settings_app();
    let config_file = dir.path().join(".kanban/config.yaml");
    let target = dir.path().join("external-config.yaml");
    std::fs::rename(&config_file, &target).expect("move config target");
    std::os::unix::fs::symlink(&target, &config_file).expect("symlink config");

    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let modal = app.modal.as_mut().expect("settings modal");
    modal.title = TextArea::new(vec!["Staged project name".to_string()]);
    modal.field_index = modal.fields().len() - 2;
    app.handle_key(key(KeyCode::Enter))
        .expect("refuse symlink save");

    let modal = app.modal.as_ref().expect("settings stays open");
    assert_eq!(modal.title_text(), "Staged project name");
    assert!(
        modal
            .error
            .as_deref()
            .is_some_and(|error| error.contains("symlinked"))
    );
}

#[test]
fn clicking_selected_settings_backend_preserves_staged_values() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let backend_index = app.modal.as_ref().unwrap().backend_selected;
    app.modal.as_mut().unwrap().model = TextArea::new(vec!["staged/model".to_string()]);
    let _ = render_at(&mut app, 120, 32);
    let hit = app
        .hitboxes
        .iter()
        .find(|hitbox| matches!(
            hitbox.action,
            HitAction::ModalOption { field: DialogField::Backend, index } if index == backend_index
        ))
        .copied()
        .expect("selected backend option hitbox");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.area.x,
        row: hit.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("click selected backend");
    assert_eq!(
        app.modal.as_ref().unwrap().model_text().as_deref(),
        Some("staged/model")
    );
}

#[test]
fn settings_modal_remains_navigable_at_constrained_height() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    assert!(render_at(&mut app, 80, 16).contains("Project settings"));
    for _ in 0..5 {
        app.handle_key(key(KeyCode::Tab))
            .expect("next settings field");
    }
    assert_eq!(
        app.modal.as_ref().unwrap().active_field(),
        DialogField::Theme
    );
    assert!(render_at(&mut app, 80, 16).contains("Theme"));
}

#[test]
fn wide_board_status_bar_opens_settings_by_mouse() {
    let (_dir, mut app) = settings_app();
    let _ = render_at(&mut app, 240, 28);
    let settings_hit = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::Action(UiAction::OpenSettings))
        .copied()
        .expect("settings status-bar hitbox");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: settings_hit.area.x,
        row: settings_hit.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("open settings by mouse");
    assert!(matches!(
        app.modal.as_ref().map(|modal| &modal.modal),
        Some(Modal::Settings)
    ));
}

#[test]
fn opencode_model_options_order_default_then_recent_then_alphabetical() {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: false\nagents:\n  opencode:\n    command: /nonexistent/opencode-disabled-for-tests\n",
    )
    .expect("quiet config");
    // Two known recent models (the default among them is not repeated), plus
    // one that the model list no longer contains.
    std::fs::write(
        dir.path().join(".kanban/recent_models"),
        "opencode-go/minimax-m3\nopenai/gpt-5.5\nopencode-go/mimo-v2.5\ngone/model\n",
    )
    .expect("recent models");
    let mut app = App::new(dir.path()).expect("create app");

    app.handle_key(key(KeyCode::Char('n'))).expect("new");
    let modal = app.modal.as_ref().expect("modal");
    let values: Vec<Option<&str>> = modal
        .model_options
        .iter()
        .map(|option| option.value.as_deref())
        .collect();
    assert_eq!(
        values,
        [
            None,
            Some("openai/gpt-5.5"),
            Some("opencode-go/minimax-m3"),
            Some("opencode-go/mimo-v2.5"),
            Some("opencode-go/deepseek-v4-flash"),
            Some("opencode-go/kimi-k2.7-code"),
            Some("opencode/deepseek-v4-flash-free"),
        ]
    );
    // Without a live opencode catalog no variants are known, so the effort
    // selector only offers the backend default.
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.effort_options.len(), 1);
    assert!(modal.effort_options[0].value.is_none());
    app.handle_key(key(KeyCode::Esc)).expect("close");
}

#[test]
fn mouse_click_opens_card_detail_on_first_release() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (column, card, area) = card_hits(&app)[1];
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 1,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(click).expect("press");
    assert_eq!(app.focused_column, column);
    assert_eq!(app.focused_card, card);
    assert_eq!(app.screen, Screen::Board);
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        ..click
    })
    .expect("release first click");
    assert_eq!(app.screen, Screen::Detail);
}

#[test]
fn mouse_drag_selects_rendered_text_and_marks_it_for_copy() {
    let (_dir, mut app) = app_with_board();
    app.status = "select me".to_string();
    let _ = render_at(&mut app, 96, 28);
    let row = 27;

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 1,
        row,
        modifiers: KeyModifiers::NONE,
    })
    .expect("start selection");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: 9,
        row,
        modifiers: KeyModifiers::NONE,
    })
    .expect("extend selection");

    let selected_style = style_at(&mut app, 96, 28, 1, row);
    assert!(selected_style.add_modifier.contains(Modifier::REVERSED));
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 9,
        row,
        modifiers: KeyModifiers::NONE,
    })
    .expect("copy selection");
    assert_eq!(app.take_pending_copy().as_deref(), Some("select me"));
}

#[test]
fn selected_text_keeps_wide_unicode_cells_once() {
    let (_dir, mut app) = app_with_board();
    app.status = "界x".to_string();
    let _ = render_at(&mut app, 96, 28);
    let row = 27;

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.handle_mouse(MouseEvent {
            kind,
            column: if matches!(kind, MouseEventKind::Down(_)) {
                1
            } else {
                3
            },
            row,
            modifiers: KeyModifiers::NONE,
        })
        .expect("select unicode text");
    }

    assert_eq!(app.take_pending_copy().as_deref(), Some("界x"));
}

#[test]
fn drag_selects_card_text_without_moving_or_opening_it() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (column, card, area) = card_hits(&app)[0];
    let task_id = app.visible_tasks_for_column(column)[card].id.clone();
    let original_status = app.ops.get_task(&task_id).unwrap().unwrap().status;
    let row = area.y + 1;

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 2,
        row,
        modifiers: KeyModifiers::NONE,
    })
    .expect("start card text selection");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: area.x + 8,
        row,
        modifiers: KeyModifiers::NONE,
    })
    .expect("select card text");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: area.x + 8,
        row,
        modifiers: KeyModifiers::NONE,
    })
    .expect("copy card text");

    assert!(app.take_pending_copy().is_some());
    assert_eq!(app.screen, Screen::Board);
    assert!(app.dragging.is_none());
    assert_eq!(
        app.ops.get_task(&task_id).unwrap().unwrap().status,
        original_status
    );
}

#[test]
fn copied_notice_restores_previous_status_after_three_seconds() {
    let (_dir, mut app) = app_with_board();
    app.status = "previous status".to_string();

    app.finish_copy(Ok(()));
    assert_eq!(app.status, "Copied selected text to clipboard");
    app.finish_copy(Ok(()));
    app.expire_copy_notice_at(Instant::now() + Duration::from_secs(4));
    assert_eq!(app.status, "previous status");
}

#[test]
fn mouse_move_highlights_board_cards() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (column, card, area) = card_hits(&app)[1];

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: area.x + 1,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover card");

    assert!(app.is_hovered(HitAction::FocusCard { column, card }));
    let style = style_at(&mut app, 96, 28, area.x, area.y);
    assert_eq!(style.fg, Some(app.theme.focus));
}

#[test]
fn mouse_move_highlights_detail_buttons_and_answer_choices() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (column, card, _) = card_hits(&app)[0];
    app.focused_column = column;
    app.focused_card = card;
    app.handle_key(key(KeyCode::Enter)).expect("detail");

    let _ = render_snapshot(&mut app);
    let run_hit = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::Action(UiAction::Run))
        .copied()
        .expect("run button hitbox");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: run_hit.area.x + 2,
        row: run_hit.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover run button");
    let button_style = style_at(&mut app, 96, 28, run_hit.area.x + 2, run_hit.area.y);
    assert_eq!(button_style.fg, Some(app.theme.focus));
    assert!(button_style.add_modifier.contains(Modifier::BOLD));

    let _ = render_snapshot(&mut app);
    let answer_hit = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::DetailAnswerOption { index: 1 })
        .copied()
        .expect("answer option hitbox");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: answer_hit.area.x + 1,
        row: answer_hit.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover answer option");
    let answer_style = style_at(&mut app, 96, 28, answer_hit.area.x + 1, answer_hit.area.y);
    assert_eq!(answer_style.fg, Some(app.theme.focus));
    assert!(answer_style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn mouse_move_highlights_modal_fields_options_and_buttons() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");

    let _ = render_at(&mut app, 120, 32);
    let backend_hit = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::ModalField(DialogField::Backend))
        .copied()
        .expect("backend field hitbox");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: backend_hit.area.x + 1,
        row: backend_hit.area.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover backend field");
    let field_style = style_at(&mut app, 120, 32, backend_hit.area.x, backend_hit.area.y);
    assert_eq!(field_style.fg, Some(app.theme.focus));

    let _ = render_at(&mut app, 120, 32);
    let option_hit = app
        .hitboxes
        .iter()
        .find(|hitbox| {
            hitbox.action
                == HitAction::ModalOption {
                    field: DialogField::Backend,
                    index: 0,
                }
        })
        .copied()
        .expect("backend option hitbox");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: option_hit.area.x + 1,
        row: option_hit.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover backend option");
    let option_style = style_at(&mut app, 120, 32, option_hit.area.x + 1, option_hit.area.y);
    assert_eq!(option_style.fg, Some(app.theme.focus));
    assert!(option_style.add_modifier.contains(Modifier::BOLD));

    let _ = render_at(&mut app, 120, 32);
    let save_hit = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::ModalButton(ModalButton::Save))
        .copied()
        .expect("save button hitbox");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: save_hit.area.x + 2,
        row: save_hit.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover save button");
    let save_style = style_at(&mut app, 120, 32, save_hit.area.x + 2, save_hit.area.y);
    assert_eq!(save_style.fg, Some(app.theme.focus));
    assert!(save_style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn modal_and_help_block_mouse_click_through() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (_, _, area) = card_hits(&app)[1];
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 1,
        row: area.y + 1,
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
    app.ops.move_task(&task.id, "review", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).unwrap();

    // Thread is focused by default: plain typing must not reach the editor.
    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Thread);
    app.handle_key(key(KeyCode::Char('х'))).unwrap();
    assert_eq!(
        app.detail.as_ref().unwrap().review_edits.lines().join("\n"),
        "abcdef"
    );
    app.handle_key(key(KeyCode::Down)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().scroll, 1);

    // Tab focuses the editor (task is in Review, no open questions).
    app.handle_key(key(KeyCode::Tab)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Edits);
    app.handle_key(key(KeyCode::End)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().review_edits.cursor(), (0, 6));
    app.handle_key(key(KeyCode::Left)).unwrap();
    app.handle_key(key(KeyCode::Char('т'))).unwrap();
    assert_eq!(
        app.detail.as_ref().unwrap().review_edits.lines().join("\n"),
        "abcdeтf"
    );

    // Esc leaves the editor first, then closes the task detail from thread focus.
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert_eq!(app.screen, Screen::Detail);
    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Thread);
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert_eq!(app.screen, Screen::Board);
    assert!(app.detail.is_none());
}

#[test]
fn review_editor_ctrl_arrows_move_by_word_when_focused() {
    let (_dir, mut app) = open_focused_review_editor("hello world foo");

    app.handle_key(key(KeyCode::End)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().review_edits.cursor(), (0, 15));

    app.handle_key(ctrl_key(KeyCode::Left)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().review_edits.cursor(), (0, 12));
    app.handle_key(ctrl_key(KeyCode::Left)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().review_edits.cursor(), (0, 6));

    app.handle_key(ctrl_key(KeyCode::Right)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().review_edits.cursor(), (0, 12));
    assert_eq!(
        app.detail.as_ref().unwrap().review_edits.lines().join("\n"),
        "hello world foo"
    );
}

#[test]
fn review_editor_ctrl_backspace_and_delete_remove_words_when_focused() {
    let (_dir, mut app) = open_focused_review_editor("hello world foo");

    app.handle_key(key(KeyCode::End)).unwrap();
    app.handle_key(ctrl_key(KeyCode::Backspace)).unwrap();
    assert_eq!(
        app.detail.as_ref().unwrap().review_edits.lines().join("\n"),
        "hello world "
    );

    app.handle_key(key(KeyCode::Home)).unwrap();
    app.handle_key(ctrl_key(KeyCode::Delete)).unwrap();
    assert_eq!(
        app.detail.as_ref().unwrap().review_edits.lines().join("\n"),
        " world "
    );
}

#[test]
fn review_editor_ctrl_s_still_saves_after_word_hotkeys() {
    let (_dir, mut app) = open_focused_review_editor("hello world foo");

    app.handle_key(key(KeyCode::End)).unwrap();
    app.handle_key(ctrl_key(KeyCode::Backspace)).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .unwrap();

    let task_id = app.detail.as_ref().unwrap().task_id.clone();
    let saved = app.ops.get_task(&task_id).unwrap().unwrap();
    assert_eq!(saved.review_edits, "hello world ");
}

#[test]
fn clicking_review_editor_focuses_it_and_highlights_panel() {
    let (_dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Edit review")).unwrap();
    app.ops.set_review_edits(&task.id, "abcdef").unwrap();
    app.ops.move_task(&task.id, "review", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let initial = render_at(&mut app, 96, 28);
    assert!(
        initial.contains("Review edits · Tab to edit"),
        "initial render should advertise keyboard focus path:\n{initial}"
    );
    let edits = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::DetailEdits)
        .copied()
        .expect("review edits hitbox");

    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: edits.area.x + 1,
        row: edits.area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(click).unwrap();

    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Edits);
    let focused = render_at(&mut app, 96, 28);
    assert!(
        focused.contains("Review edits [focused]"),
        "focused render should show review editor highlight title:\n{focused}"
    );
}

/// Input-provenance — files read/written, URLs, MCP the agent actually consumed,
/// sourced from a harvested manifest — is kept out of the thread entirely and
/// shown only in the `v` popup, so the conversation panel stays clean.
#[test]
fn input_provenance_shows_in_v_popup_not_thread() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask {
            title: "Provenance card".to_string(),
            agent_backend: Some("claude".to_string()),
            ..Default::default()
        })
        .unwrap();
    ThreadManager::new(dir.path())
        .unwrap()
        .post(
            &task.id,
            crate::core::models::MessageRole::System,
            crate::core::models::MessageKind::AgentStep,
            "■ exit session=ses-prov code=0 outcome=Closed auto_resumes=0 \
             reads=1 writes=1 urls=1 mcp=1 → provenance: .kanban/provenance/ses-prov.yaml",
            None,
            Vec::new(),
            Some("kanban".to_string()),
        )
        .unwrap();
    crate::core::provenance::write_manifest(
        &app.ops.storage.provenance_dir,
        &crate::core::provenance::InputManifest {
            session_id: "ses-prov".to_string(),
            backend: "claude".to_string(),
            reads: vec!["src/core/operations.rs".to_string()],
            writes: vec!["src/core/config.rs".to_string()],
            urls: vec!["https://example.com/doc".to_string()],
            mcp: vec!["github:list_prs".to_string()],
            generated_at: "2026-07-21T00:00:00".to_string(),
            ..Default::default()
        },
    )
    .unwrap();

    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.clamp_focus();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    // The thread panel must not carry the provenance section any more.
    let thread = render_snapshot(&mut app);
    assert!(
        !thread.contains("Inputs (provenance)"),
        "provenance must not render in the thread panel:\n{thread}"
    );

    // It lives only in the `v` popup.
    app.handle_key(key(KeyCode::Char('v')))
        .expect("open inputs popup");
    assert_eq!(app.screen, Screen::TextView);
    insta::assert_snapshot!("detail_provenance", render_snapshot(&mut app));
}

/// The thread wraps its lines, so a narrow terminal renders one logical line
/// over several rows. End must still reach the thread's last row there.
#[test]
fn detail_thread_scrolls_to_last_line_in_a_narrow_terminal() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Wrapped thread"))
        .unwrap();
    let thread_manager = ThreadManager::new(dir.path()).unwrap();
    let long_body = "wrapped body ".repeat(16);
    for index in 0..12 {
        thread_manager
            .post(
                &task.id,
                crate::core::models::MessageRole::Agent,
                crate::core::models::MessageKind::Context,
                &format!("{long_body} marker{index}"),
                None,
                Vec::new(),
                Some("agent".to_string()),
            )
            .unwrap();
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.clamp_focus();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    for width in [40, 60, 96] {
        app.detail.as_mut().unwrap().scroll = 0;
        let _ = render_at(&mut app, width, 14);
        app.handle_key(key(KeyCode::End)).unwrap();
        let bottom = render_at(&mut app, width, 14);
        assert!(
            bottom.contains("marker11"),
            "End must reach the thread's last line at width {width}:\n{bottom}"
        );
    }
}

#[test]
fn mouse_wheel_scrolls_detail_thread_under_cursor() {
    let (dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Long thread")).unwrap();
    let thread_manager = ThreadManager::new(dir.path()).unwrap();
    for index in 0..24 {
        thread_manager
            .post(
                &task.id,
                crate::core::models::MessageRole::Agent,
                crate::core::models::MessageKind::Context,
                &format!("Context line {index}"),
                None,
                Vec::new(),
                Some("agent".to_string()),
            )
            .unwrap();
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.clamp_focus();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let _ = render_at(&mut app, 96, 20);
    let thread_area = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::DetailThread)
        .map(|hitbox| hitbox.area)
        .expect("thread hitbox");
    assert!(app.detail.as_ref().unwrap().max_scroll > 0);

    let scroll_down = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: thread_area.x + 1,
        row: thread_area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(scroll_down).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().scroll, 1);

    let scroll_up = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        ..scroll_down
    };
    app.handle_mouse(scroll_up).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().scroll, 0);
}

fn review_column(app: &App) -> usize {
    app.board
        .columns
        .iter()
        .position(|column| column.id == "review")
        .expect("review column")
}

fn in_progress_column(app: &App) -> usize {
    app.board
        .columns
        .iter()
        .position(|column| column.id == "in_progress")
        .expect("in_progress column")
}

fn open_focused_review_editor(text: &str) -> (tempfile::TempDir, App) {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Edit review"))
        .expect("create task");
    app.ops
        .set_review_edits(&task.id, text)
        .expect("set review edits");
    app.ops
        .move_task(&task.id, "review", false)
        .expect("move to review");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload board");
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.handle_key(key(KeyCode::Tab)).expect("focus editor");
    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Edits);
    (dir, app)
}

#[test]
fn review_edits_save_and_rerun_are_separate_actions() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Review this"))
        .expect("create task");
    app.ops
        .move_task(&task.id, "review", false)
        .expect("move to review");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.detail
        .as_mut()
        .expect("detail")
        .review_edits
        .insert_str("Please tighten validation");

    // Ctrl+S only persists the buffer; the task stays in Review.
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .expect("save");
    let saved = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(saved.status.as_str(), "review");
    assert_eq!(saved.review_edits, "Please tighten validation");
    assert_eq!(app.screen, Screen::Detail);

    // Ctrl+R folds the edits into the thread and re-runs the agent.
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .expect("rerun");
    let rerun = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(rerun.status.as_str(), "in_progress");
    assert!(rerun.review_edits.is_empty());
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
    assert_eq!(app.screen, Screen::Board);
    assert_eq!(app.focused_column, in_progress_column(&app));
    assert_eq!(
        app.focused_task().map(|focused| focused.id.as_str()),
        Some(task.id.as_str())
    );
}

#[test]
fn review_edits_rerun_saves_visible_buffer_first() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Review this"))
        .expect("create task");
    app.ops
        .move_task(&task.id, "review", false)
        .expect("move to review");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.detail
        .as_mut()
        .expect("detail")
        .review_edits
        .insert_str("Return Escape for closing the task detail");

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .expect("rerun");

    let rerun = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(rerun.status.as_str(), "in_progress");
    assert!(rerun.review_edits.is_empty());
    let edits = ThreadManager::new(dir.path())
        .unwrap()
        .messages_of_kind(&task.id, crate::core::models::MessageKind::ReviewEdit)
        .unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].body, "Return Escape for closing the task detail");
    assert_eq!(app.screen, Screen::Board);
    assert_eq!(app.focused_column, in_progress_column(&app));
    assert_eq!(
        app.focused_task().map(|focused| focused.id.as_str()),
        Some(task.id.as_str())
    );
}

#[test]
fn review_rerun_from_board_focuses_in_progress() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Rework me"))
        .expect("create task");
    app.ops
        .move_task(&task.id, "review", false)
        .expect("move to review");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = review_column(&app);
    app.focused_card = 0;

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .expect("rerun");
    assert!(
        app.take_full_redraw(),
        "rerun must request a full terminal redraw"
    );

    assert_eq!(app.screen, Screen::Board);
    assert_eq!(app.focused_column, in_progress_column(&app));
    assert_eq!(
        app.focused_task().map(|focused| focused.id.as_str()),
        Some(task.id.as_str())
    );
    assert_eq!(
        app.ops.get_task(&task.id).unwrap().unwrap().status.as_str(),
        "in_progress"
    );
}

#[test]
fn run_hotkey_starts_task_without_confirmation() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Run me"))
        .expect("create task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.clamp_focus();

    app.handle_key(key(KeyCode::Char('r'))).expect("run");
    assert!(
        app.take_full_redraw(),
        "run must request a full terminal redraw"
    );

    assert!(app.modal.is_none(), "run must not open any dialog");
    assert!(app.status.starts_with("Started"), "status: {}", app.status);
    let started = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(started.status.as_str(), "in_progress");
    let session_id = started.session.expect("session assigned");
    assert!(SessionManager::new(dir.path()).is_session_active(&session_id));

    // On In Progress the same key becomes Revoke and replaces the session.
    app.focused_column = app
        .board
        .columns
        .iter()
        .position(|column| column.id == "in_progress")
        .expect("in_progress column");
    app.focused_card = 0;
    SessionManager::new(dir.path())
        .mark_wait_exited(&session_id)
        .expect("agent process exited");
    app.handle_key(key(KeyCode::Char('r'))).expect("revoke");
    assert!(
        app.take_full_redraw(),
        "revoke must request a full terminal redraw"
    );
    assert!(app.status.contains("Revoked and woke"), "{}", app.status);
    let revoked = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_ne!(revoked.session.as_deref(), Some(session_id.as_str()));
    assert!(!SessionManager::new(dir.path()).is_session_active(&session_id));
}

#[test]
fn in_progress_detail_shows_revoke_instead_of_run() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Revoke me"))
        .expect("create task");
    app.ops
        .take_task(&task.id, "ses-revoke-detail", true)
        .unwrap()
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = app
        .board
        .columns
        .iter()
        .position(|column| column.id == "in_progress")
        .unwrap();
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("detail");

    let rendered = render_at(&mut app, 100, 24);

    assert!(rendered.contains("Revoke r"), "{rendered}");
    assert!(rendered.contains("Stop k"), "{rendered}");
    assert!(rendered.contains("r revoke"), "{rendered}");
    assert!(rendered.contains("k stop"), "{rendered}");
    assert!(
        app.hitboxes
            .iter()
            .any(|hitbox| hitbox.action == HitAction::Action(UiAction::Revoke))
    );
    assert!(
        app.hitboxes
            .iter()
            .any(|hitbox| hitbox.action == HitAction::Action(UiAction::Stop))
    );
    assert!(
        app.hitboxes
            .iter()
            .all(|hitbox| hitbox.action != HitAction::Action(UiAction::Run))
    );
}

#[test]
fn board_stop_hotkey_closes_session_without_relaunch() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Stop from board"))
        .expect("create task");
    app.ops
        .take_task(&task.id, "ses-board-stop", true)
        .unwrap()
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = app
        .board
        .columns
        .iter()
        .position(|column| column.id == "in_progress")
        .expect("in_progress column");
    app.focused_card = 0;

    app.handle_key(key(KeyCode::Char('k'))).expect("stop");
    assert!(matches!(
        app.modal.as_ref().expect("stop confirm").modal,
        Modal::KillSessionConfirm { .. }
    ));
    app.handle_key(key(KeyCode::Char('y')))
        .expect("confirm stop");
    assert!(
        app.status.starts_with("Stopped ses-board-stop"),
        "{}",
        app.status
    );

    let stopped = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stopped.status.as_str(), "in_progress");
    assert_eq!(stopped.session.as_deref(), Some("ses-board-stop"));
    assert!(!SessionManager::new(dir.path()).is_session_active("ses-board-stop"));
    assert_eq!(app.primary_run_action_for(&stopped), UiAction::Run);

    app.handle_key(key(KeyCode::Char('k'))).expect("idle stop");
    assert!(app.modal.is_none());
    assert_eq!(app.status, "No running session to stop");
}

#[test]
fn enter_launches_todo_task_from_detail() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Run from detail"))
        .expect("create task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.clamp_focus();

    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    assert_eq!(app.screen, Screen::Detail);
    assert_eq!(app.detail.as_ref().unwrap().task_id, task.id);

    app.handle_key(key(KeyCode::Enter))
        .expect("run from detail");

    assert!(app.modal.is_none(), "enter must not open any dialog");
    assert_eq!(app.screen, Screen::Board);
    assert!(app.status.starts_with("Started"), "status: {}", app.status);
    let started = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(started.status.as_str(), "in_progress");
    let session_id = started.session.expect("session assigned");
    assert!(SessionManager::new(dir.path()).is_session_active(&session_id));
}

#[test]
fn enter_is_inactive_on_non_todo_detail_task() {
    let (_dir, mut app) = populated_app();
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    let task_id = app.detail.as_ref().unwrap().task_id.clone();

    app.handle_key(key(KeyCode::Enter))
        .expect("ignore enter in detail");

    assert!(app.modal.is_none(), "enter must not open any dialog");
    assert_eq!(app.screen, Screen::Detail);
    assert_eq!(app.detail.as_ref().unwrap().task_id, task_id);
    let unchanged = app.ops.get_task(&task_id).unwrap().unwrap();
    assert_eq!(unchanged.status.as_str(), "review");
    assert!(unchanged.session.is_none());
}

#[test]
fn detail_hotkeys_operate_on_open_task() {
    let (_dir, mut app) = populated_app();
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    let task_id = app.detail.as_ref().unwrap().task_id.clone();

    // Edit opens the form for the detail task.
    app.handle_key(key(KeyCode::Char('e'))).expect("edit");
    assert_eq!(
        app.modal.as_ref().expect("edit modal").modal,
        Modal::EditTask {
            task_id: task_id.clone()
        }
    );
    app.handle_key(key(KeyCode::Esc)).expect("discard prompt");
    app.handle_key(key(KeyCode::Char('y'))).expect("discard");

    // Approve moves the Review task to Done without leaving the detail.
    app.handle_key(key(KeyCode::Char('y'))).expect("approve");
    assert_eq!(app.screen, Screen::Detail);
    let approved = app.ops.get_task(&task_id).unwrap().unwrap();
    assert_eq!(approved.status.as_str(), "done");

    // Approving again reports the task is no longer in Review.
    app.handle_key(key(KeyCode::Char('y')))
        .expect("approve again");
    assert!(app.status.contains("not in Review"), "{}", app.status);
}

#[test]
fn detail_answer_panel_submits_variant_answer() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    let task_id = app.detail.as_ref().unwrap().task_id.clone();

    app.handle_key(key(KeyCode::Tab)).expect("focus answer");
    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Answer);
    app.handle_key(key(KeyCode::Down))
        .expect("select variant 1");
    app.handle_key(key(KeyCode::Enter)).expect("submit");

    let answered = app.ops.get_task(&task_id).unwrap().unwrap();
    assert!(!answered.has_questions);
    let question = app
        .detail
        .as_ref()
        .unwrap()
        .messages
        .iter()
        .find(|message| message.body == "Choose a route?")
        .expect("question message")
        .clone();
    assert_eq!(question.answer.as_deref(), Some("Fast path"));
    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Thread);
}

#[test]
fn clicking_question_preview_opens_answer_panel() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let preview = app
        .hitboxes
        .iter()
        .find(|hitbox| matches!(hitbox.action, HitAction::OpenAnswer { .. }))
        .copied()
        .expect("question preview hitbox");
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: preview.area.x,
        row: preview.area.y,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(click).expect("click preview");
    assert_eq!(app.screen, Screen::Detail);
    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Answer);
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
fn fs_reload_preserves_unsaved_review_edits() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Do not clear review"))
        .expect("create task");
    app.ops
        .move_task(&task.id, "review", false)
        .expect("move to review");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.handle_key(key(KeyCode::Tab)).expect("focus edits");
    app.detail
        .as_mut()
        .expect("detail")
        .review_edits
        .insert_str("Please keep this while the board refreshes");

    ThreadManager::new(dir.path())
        .unwrap()
        .post(
            &task.id,
            crate::core::models::MessageRole::Agent,
            crate::core::models::MessageKind::Context,
            "background context update",
            None,
            vec![],
            None,
        )
        .unwrap();
    app.reload_if_changed().expect("refresh detail");

    let detail = app.detail.as_ref().expect("detail after reload");
    assert_eq!(detail.focus, DetailFocus::Edits);
    assert_eq!(
        detail.review_edits.lines().join("\n"),
        "Please keep this while the board refreshes"
    );
    assert_eq!(detail.messages.len(), 3);
    assert!(
        app.ops
            .get_task(&task.id)
            .unwrap()
            .unwrap()
            .review_edits
            .is_empty()
    );
}

#[test]
fn fs_reload_preserves_inline_answer_draft() {
    let (dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.handle_key(key(KeyCode::Tab)).expect("focus answer");
    let detail = app.detail.as_mut().expect("detail");
    detail
        .answer_input
        .insert_str("Keep this answer while the board refreshes");
    detail.variant_selected = 1;

    let task_id = detail.task_id.clone();
    ThreadManager::new(dir.path())
        .unwrap()
        .post(
            &task_id,
            crate::core::models::MessageRole::Agent,
            crate::core::models::MessageKind::Context,
            "background context update",
            None,
            vec![],
            None,
        )
        .unwrap();
    app.reload_if_changed().expect("refresh detail");

    let detail = app.detail.as_ref().expect("detail after reload");
    assert_eq!(detail.focus, DetailFocus::Answer);
    assert_eq!(
        detail.answer_input.lines().join("\n"),
        "Keep this answer while the board refreshes"
    );
    assert_eq!(detail.variant_selected, 1);
    assert!(
        detail
            .messages
            .iter()
            .any(|message| message.body == "background context update")
    );
}

#[test]
fn sessions_render_and_controls_use_cached_sessions() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Running"))
        .expect("create task");
    SessionManager::new(dir.path())
        .link_named_session(&task.id, "ses-tmux-live", "Persisted Running")
        .expect("link session");
    std::fs::remove_file(
        dir.path()
            .join(".kanban/tasks/todo")
            .join(format!("{}.md", task.id)),
    )
    .expect("remove task file");
    let log_file = dir.path().join(".kanban/logs/ses-tmux-live.log");
    std::fs::write(&log_file, "tokens: 17").expect("write initial token log");

    app.handle_key(key(KeyCode::Char('l')))
        .expect("open sessions");
    assert_eq!(app.active_sessions.len(), 1);
    SessionManager::new(dir.path()).unlink_session("ses-tmux-live");
    std::fs::write(&log_file, "tokens: 99").expect("overwrite token log");
    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("ses-tmux-live"));
    assert!(rendered.contains("Persisted Running"));
    assert!(rendered.contains("tokens: 17"));
    assert!(!rendered.contains("tokens: 99"));
    assert!(render_snapshot(&mut app).contains("ses-tmux-live"));
    app.handle_key(key(KeyCode::Down))
        .expect("navigate cached sessions");
    app.handle_key(key(KeyCode::Enter)).expect("open session");
    // The unified open finds no tmux host (and the record was unlinked), so it
    // falls back to following the session's log rather than queueing an attach.
    assert!(app.take_terminal_action().is_none());
    assert_eq!(app.screen, Screen::LogView);
}

#[test]
fn sessions_cache_refreshes_after_fingerprint_change() {
    let (dir, mut app) = app_with_board();
    let first = app
        .ops
        .create_task(NewTask::titled("First running"))
        .expect("create first task");
    SessionManager::new(dir.path())
        .link_session(&first.id, "ses-first")
        .expect("link first session");
    app.handle_key(key(KeyCode::Char('l')))
        .expect("open sessions");
    assert_eq!(app.active_sessions.len(), 1);

    let second = app
        .ops
        .create_task(NewTask::titled("Second running"))
        .expect("create second task");
    SessionManager::new(dir.path())
        .link_session(&second.id, "ses-second")
        .expect("link second session");
    app.reload_if_changed().expect("refresh sessions cache");

    assert_eq!(app.active_sessions.len(), 2);
    assert!(
        app.active_sessions
            .iter()
            .any(|active_session| active_session.session.id == "ses-second")
    );
}

#[test]
fn phase_five_search_popup_escape_clears_and_enter_preserves_filter() {
    let (_dir, mut app) = app_with_board();
    app.ops
        .create_task(NewTask::titled("Authentication gateway"))
        .expect("create task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");

    app.handle_key(key(KeyCode::Char('/')))
        .expect("open search");
    for character in "AUTH".chars() {
        app.handle_key(key(KeyCode::Char(character)))
            .expect("type search");
    }
    app.handle_key(key(KeyCode::Enter)).expect("keep filter");
    assert!(!app.search.active);
    assert_eq!(app.search.text(), "AUTH");
    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("Filter: \"AUTH\" · Esc clear"));
    insta::assert_snapshot!("phase_five_filter_indicator_and_highlight", rendered);

    app.handle_key(key(KeyCode::Esc))
        .expect("clear board filter");
    assert_eq!(app.screen, Screen::Board);
    assert!(app.search.text().is_empty());
    assert!(!app.should_quit);
    app.handle_key(key(KeyCode::Esc))
        .expect("ignore board escape");
    assert!(!app.should_quit);

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .expect("first ctrl-c prompts");
    assert!(!app.should_quit);
    assert_eq!(app.status, "Press ctrl + C again to close");
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .expect("second ctrl-c exits");
    assert!(app.should_quit);

    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('/')))
        .expect("open search");
    app.handle_key(key(KeyCode::Char('x')))
        .expect("type search");
    app.handle_key(key(KeyCode::Esc)).expect("discard search");
    assert!(!app.search.active);
    assert!(app.search.text().is_empty());

    app.search.query.insert_str("kept");
    app.handle_key(key(KeyCode::Char('q')))
        .expect("q does not quit from board");
    assert!(!app.should_quit);
    assert_eq!(app.status, "Press ctrl + C twice to close");
    assert_eq!(app.search.text(), "kept", "q must not clear the filter");
}

#[test]
fn phase_five_archive_and_sessions_search_filter_and_clamp_selection() {
    let (dir, mut app) = app_with_board();
    let archive_match = app
        .ops
        .create_task(NewTask::titled("Archived needle"))
        .expect("create archived match");
    let archive_other = app
        .ops
        .create_task(NewTask::titled("Archived other"))
        .expect("create archived other");
    app.ops
        .move_task(&archive_match.id, "archive", false)
        .expect("archive match");
    app.ops
        .move_task(&archive_other.id, "archive", false)
        .expect("archive other");

    app.handle_key(key(KeyCode::Char('a')))
        .expect("open archive");
    app.archive_selected = 1;
    app.handle_key(key(KeyCode::Char('/')))
        .expect("activate archive search");
    for character in "needle".chars() {
        app.handle_key(key(KeyCode::Char(character)))
            .expect("filter archive");
    }
    assert_eq!(app.archive_selected, 0);
    app.handle_key(key(KeyCode::Enter))
        .expect("close archive search");
    let archive = render_snapshot(&mut app);
    assert!(archive.contains("Archived needle"));
    assert!(!archive.contains("Archived other"));
    app.handle_key(key(KeyCode::Esc))
        .expect("return from archive");
    assert_eq!(app.screen, Screen::Board);

    app.search = Default::default();
    let session_match = app
        .ops
        .create_task(NewTask::titled("Session needle"))
        .expect("create session match");
    let session_other = app
        .ops
        .create_task(NewTask::titled("Session other"))
        .expect("create session other");
    SessionManager::new(dir.path())
        .link_session(&session_match.id, "ses-needle")
        .expect("link matching session");
    SessionManager::new(dir.path())
        .link_session(&session_other.id, "ses-other")
        .expect("link other session");
    app.handle_key(key(KeyCode::Char('l')))
        .expect("open sessions");
    app.session_selected = 1;
    app.handle_key(key(KeyCode::Char('/')))
        .expect("activate sessions search");
    for character in "needle".chars() {
        app.handle_key(key(KeyCode::Char(character)))
            .expect("filter sessions");
    }
    assert_eq!(app.session_selected, 0);
    app.handle_key(key(KeyCode::Enter))
        .expect("close sessions search");
    let sessions = render_snapshot(&mut app);
    assert!(sessions.contains("ses-needle"));
    assert!(!sessions.contains("ses-other"));

    app.handle_key(key(KeyCode::Char('/')))
        .expect("open no-match search");
    for character in "absent".chars() {
        app.handle_key(key(KeyCode::Char(character)))
            .expect("filter to no sessions");
    }
    app.handle_key(key(KeyCode::Enter))
        .expect("close no-match search");
    assert!(render_snapshot(&mut app).contains("No sessions match filter"));
}

#[test]
fn phase_five_archive_cache_filters_ids_and_does_not_load_during_render() {
    let (dir, mut app) = app_with_board();
    let first = app
        .ops
        .create_task(NewTask::titled("Archive title without identifier"))
        .expect("create first archive task");
    let second = app
        .ops
        .create_task(NewTask::titled("Cached after file removal"))
        .expect("create second archive task");
    app.ops
        .move_task(&first.id, "archive", false)
        .expect("archive first task");
    app.ops
        .move_task(&second.id, "archive", false)
        .expect("archive second task");

    app.handle_key(key(KeyCode::Char('a')))
        .expect("open refreshed archive");
    app.search.query.insert_str(first.id.to_lowercase());
    assert_eq!(app.filtered_archived_tasks().len(), 1);
    assert_eq!(app.filtered_archived_tasks()[0].id, first.id);
    app.search = Default::default();

    let archived_file = dir
        .path()
        .join(".kanban/tasks/archive")
        .join(format!("{}.md", second.id));
    std::fs::remove_file(archived_file).expect("remove archived source after cache refresh");
    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("Cached after file removal"));
    assert_eq!(
        app.archived_tasks.len(),
        2,
        "render must use cached archive data"
    );
}

#[test]
fn phase_five_archive_refresh_clamps_selection_and_enter_uses_visible_cache_row() {
    let (_dir, mut app) = app_with_board();
    let first = app
        .ops
        .create_task(NewTask::titled("First remaining archive task"))
        .expect("create first archive task");
    let second = app
        .ops
        .create_task(NewTask::titled("Removed archive task"))
        .expect("create second archive task");
    app.ops
        .move_task(&first.id, "archive", false)
        .expect("archive first task");
    app.ops
        .move_task(&second.id, "archive", false)
        .expect("archive second task");
    app.handle_key(key(KeyCode::Char('a')))
        .expect("open archive");
    app.archive_selected = 1;

    app.ops
        .move_task(&second.id, "todo", false)
        .expect("external archive change");
    app.reload_if_changed().expect("refresh archive cache");
    assert_eq!(app.archived_tasks.len(), 1);
    assert_eq!(app.archive_selected, 0);
    assert_eq!(app.filtered_archived_tasks()[0].id, first.id);

    app.handle_key(key(KeyCode::Enter))
        .expect("open visible archived task");
    assert_eq!(app.screen, Screen::Detail);
    assert_eq!(app.detail.as_ref().expect("detail").task_id, first.id);
}

#[test]
fn phase_five_title_highlights_only_the_visible_truncated_text() {
    use ratatui::style::Color;

    let visible = super::card::truncate_display("Authentication overflow", 8);
    assert_eq!(visible, "Authent…");
    let spans = super::card::highlight_title_matches(&visible, "AUTH", Color::Yellow);
    assert_eq!(
        spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>(),
        visible
    );
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.style.fg == Some(Color::Yellow))
            .count(),
        1
    );
    assert!(
        super::card::highlight_title_matches(&visible, "overflow", Color::Yellow)
            .iter()
            .all(|span| span.style.fg != Some(Color::Yellow))
    );
}

#[test]
fn phase_five_unicode_filter_and_highlight_cover_the_same_original_title_span() {
    use ratatui::style::Color;

    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("İstanbul"))
        .expect("create Unicode title");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload board");
    app.search.query.insert_str("i");

    assert!(
        app.visible_tasks_for_column(0)
            .iter()
            .any(|visible| visible.id == task.id),
        "the Board filter must match the title"
    );
    let spans = super::card::highlight_title_matches("İstanbul", &app.search.text(), Color::Yellow);
    assert_eq!(spans[0].content.as_ref(), "İ");
    assert_eq!(spans[0].style.fg, Some(Color::Yellow));
}

#[test]
fn russian_layout_maps_commands_without_changing_text_input() {
    for (russian, latin) in [
        ('й', 'q'),
        ('т', 'n'),
        ('у', 'e'),
        ('ь', 'm'),
        ('ы', 's'),
        ('ц', 'w'),
        ('в', 'd'),
        ('к', 'r'),
        ('н', 'y'),
        ('и', 'b'),
        ('г', 'u'),
        ('ф', 'a'),
        ('д', 'l'),
        ('с', 'c'),
        ('е', 't'),
        ('м', 'v'),
        ('Ф', 'A'),
        ('К', 'R'),
        ('ч', 'x'),
        ('щ', 'o'),
        ('з', 'p'),
        ('З', 'P'),
        ('.', '/'),
        (',', '?'),
    ] {
        assert_eq!(
            normalize_command_key(key(KeyCode::Char(russian))).code,
            KeyCode::Char(latin)
        );
    }
    for (russian, latin) in [('с', 'c'), ('е', 't'), ('ы', 's'), ('м', 'v')] {
        assert_eq!(
            normalize_command_key(KeyEvent::new(KeyCode::Char(russian), KeyModifiers::CONTROL))
                .code,
            KeyCode::Char(latin)
        );
    }

    let (_dir, mut app) = app_with_board();
    app.ops
        .create_task(NewTask::titled("Russian commands"))
        .expect("create task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.handle_key(key(KeyCode::Char('д')))
        .expect("RU sessions hotkey");
    assert_eq!(app.screen, Screen::Sessions);
    app.handle_key(key(KeyCode::Char('й')))
        .expect("RU back hotkey");
    assert_eq!(app.screen, Screen::Board);
    app.handle_key(key(KeyCode::Char('т')))
        .expect("RU new-task hotkey");
    app.handle_key(key(KeyCode::Char('т')))
        .expect("title text input");
    assert_eq!(app.modal.as_ref().expect("new modal").title_text(), "т");
    app.handle_key(key(KeyCode::Esc)).expect("discard prompt");
    app.handle_key(key(KeyCode::Char('y'))).expect("discard");

    // Run via the Cyrillic alias: immediate, no confirmation dialog.
    app.handle_key(key(KeyCode::Char('к'))).expect("RU run");
    assert!(app.modal.is_none());
    assert!(app.status.starts_with("Started"), "{}", app.status);

    app.handle_key(key(KeyCode::Char('/')))
        .expect("open search");
    app.handle_key(key(KeyCode::Char('т')))
        .expect("search text input");
    assert_eq!(app.search.text(), "т");
}

#[test]
fn board_scrolls_only_after_focus_leaves_rendered_viewport() {
    let (_dir, mut app) = app_with_board();
    let _ = render_snapshot(&mut app);
    let capacity = app.visible_card_capacities[0];
    for index in 0..=capacity {
        app.ops
            .create_task(NewTask::titled(format!("Card {index}")))
            .expect("create card");
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.clamp_focus();
    let _ = render_snapshot(&mut app);

    for _ in 1..capacity {
        app.handle_key(key(KeyCode::Down))
            .expect("move within viewport");
        assert_eq!(app.column_offsets[0], 0);
    }
    app.handle_key(key(KeyCode::Down))
        .expect("move beyond viewport");
    assert_eq!(app.focused_card, capacity);
    assert_eq!(app.column_offsets[0], 1);
}

#[test]
fn board_capacity_matches_bordered_inner_viewport_at_height_25() {
    let (_dir, mut app) = app_with_board();
    for index in 0..6 {
        app.ops
            .create_task(NewTask::titled(format!("Boundary card {index}")))
            .expect("create card");
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    let _ = render_at(&mut app, 96, 25);

    let capacity = app.visible_card_capacities[0];
    let rendered_cards = card_hits(&app)
        .iter()
        .filter(|(column, _, _)| *column == 0)
        .count();
    assert_eq!(capacity, 5);
    assert_eq!(capacity, rendered_cards);

    for _ in 1..=capacity {
        app.handle_key(key(KeyCode::Down))
            .expect("move across boundary");
    }
    let _ = render_at(&mut app, 96, 25);
    assert!(
        card_hits(&app)
            .iter()
            .any(|(column, card, _)| *column == 0 && *card == app.focused_card)
    );
}

#[test]
fn mouse_wheel_scrolls_column_under_cursor_without_stealing_focus() {
    let (_dir, mut app) = app_with_board();
    let _ = render_snapshot(&mut app);
    let capacity = app.visible_card_capacities[0];
    for index in 0..capacity + 2 {
        app.ops
            .create_task(NewTask::titled(format!("Card {index}")))
            .expect("create card");
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.clamp_focus();
    app.handle_key(key(KeyCode::Right)).expect("focus column 1");
    assert_eq!(app.focused_column, 1);
    let _ = render_snapshot(&mut app);

    let (_, _, area) = card_hits(&app)
        .into_iter()
        .find(|(column, card, _)| *column == 0 && *card == 0)
        .expect("first card of column 0");
    let scroll_down = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: area.x + 1,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(scroll_down).expect("scroll down");
    assert_eq!(app.column_offsets[0], 1);
    assert_eq!(app.focused_column, 1, "wheel must not steal focus");

    let scroll_up = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        ..scroll_down
    };
    app.handle_mouse(scroll_up).expect("scroll up");
    assert_eq!(app.column_offsets[0], 0);
}

#[test]
fn mouse_wheel_on_focused_column_drags_focus_into_view() {
    let (_dir, mut app) = app_with_board();
    let _ = render_snapshot(&mut app);
    let capacity = app.visible_card_capacities[0];
    for index in 0..capacity + 2 {
        app.ops
            .create_task(NewTask::titled(format!("Card {index}")))
            .expect("create card");
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.clamp_focus();
    let _ = render_snapshot(&mut app);
    assert_eq!(app.focused_card, 0);

    let (_, _, area) = card_hits(&app)
        .into_iter()
        .find(|(column, card, _)| *column == 0 && *card == 0)
        .expect("first card of column 0");
    let scroll_down = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: area.x + 1,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(scroll_down).expect("scroll down");
    assert_eq!(app.column_offsets[0], 1);
    assert_eq!(
        app.focused_card, 1,
        "focus follows the viewport so the render clamp keeps the scroll"
    );
    let _ = render_snapshot(&mut app);
    assert_eq!(app.column_offsets[0], 1, "render must not undo the scroll");
}

#[test]
fn footer_does_not_render_refresh_elapsed_text() {
    let (_dir, mut app) = app_with_board();
    app.ops
        .create_task(NewTask::titled("External board change"))
        .expect("create task");
    app.reload_if_changed().expect("reload board");
    assert!(
        !render_snapshot(&mut app)
            .to_lowercase()
            .contains("refreshed")
    );
}

#[test]
fn sessions_keeps_global_ctrl_shortcuts_with_russian_aliases() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('l')))
        .expect("open sessions");

    let initial_theme = app.settings.theme_name.clone();
    app.handle_key(KeyEvent::new(KeyCode::Char('е'), KeyModifiers::CONTROL))
        .expect("switch theme from sessions");
    assert_ne!(app.settings.theme_name, initial_theme);
    assert_eq!(app.screen, Screen::Sessions);

    app.handle_key(KeyEvent::new(KeyCode::Char('с'), KeyModifiers::CONTROL))
        .expect("prompt from sessions");
    assert!(!app.should_quit);
    assert_eq!(app.status, "Press ctrl + C again to close");
    app.handle_key(KeyEvent::new(KeyCode::Char('с'), KeyModifiers::CONTROL))
        .expect("quit from sessions");
    assert!(app.should_quit);
}

#[test]
fn ctrl_c_exit_prompt_expires_after_three_seconds() {
    let (_dir, mut app) = app_with_board();

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .expect("first ctrl-c prompts");
    assert_eq!(app.status, "Press ctrl + C again to close");

    app.expire_ctrl_c_prompt_at(Instant::now() + Duration::from_secs(4));
    assert!(!app.should_quit);
    assert!(app.status.is_empty());

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .expect("expired prompt restarts");
    assert!(!app.should_quit);
    assert_eq!(app.status, "Press ctrl + C again to close");
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

#[test]
fn phase_three_headers_targeted_creation_and_bulk_confirmation_work() {
    let (_dir, mut app) = app_with_board();
    let in_progress = app
        .board
        .columns
        .iter()
        .position(|column| column.id == "in_progress")
        .expect("in progress column");
    app.focused_column = in_progress;
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    let modal = app.modal.as_mut().expect("new modal");
    assert_eq!(
        modal.modal,
        Modal::NewTask {
            target_status: Some("in_progress".to_string())
        }
    );
    modal.title.insert_str("Targeted");
    modal.field_index = modal.fields().len() - 2;
    app.handle_key(key(KeyCode::Enter))
        .expect("create targeted task");
    assert_eq!(
        app.ops
            .list_tasks(Some("in_progress"), None, "created", "asc")
            .unwrap()
            .len(),
        1
    );
    let task = app
        .ops
        .list_tasks(Some("in_progress"), None, "created", "asc")
        .unwrap()
        .remove(0);
    app.ops.move_task(&task.id, "review", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    app.handle_key(key(KeyCode::Char('b')))
        .expect("review-done dialog");
    assert!(matches!(
        app.modal.as_ref().expect("confirm modal").modal,
        Modal::BulkConfirm { .. }
    ));
    app.handle_key(key(KeyCode::Char('y'))).unwrap();
    assert!(app.status.contains("Marked 1 Review task(s) Done"));
    assert_eq!(
        app.ops
            .list_tasks(Some("done"), None, "created", "asc")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn phase_three_column_headers_render_name_and_count_only() {
    let (_dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Bulk target")).unwrap();
    app.ops.move_task(&task.id, "in_progress", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    let rendered = render_snapshot(&mut app);

    assert!(rendered.contains("In Progress (1)"));
    assert!(!rendered.contains("in_progress"));
    assert!(!rendered.contains("[+]"));
    assert!(!rendered.contains("⇒Review"));
}

#[test]
fn phase_three_header_question_and_drag_hitboxes_drive_board_state() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let question_hit = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::Action(UiAction::FocusQuestions))
        .copied()
        .expect("question count hitbox");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: question_hit.area.x,
        row: question_hit.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("focus first question");
    assert!(
        app.focused_task()
            .expect("focused question task")
            .has_questions
    );
    app.focused_column = 2;
    app.focused_card = 0;

    let source = card_hits(&app)[0];
    let source_task = app.visible_tasks_for_column(source.0)[source.1].id.clone();
    let target = app
        .hitboxes
        .iter()
        .find(|hitbox| matches!(hitbox.action, HitAction::ColumnFocus(2)))
        .copied()
        .expect("review column area");
    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: source.2.x + 1,
        row: source.2.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(down).expect("start drag");
    assert!(app.dragging.is_some());
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: target.area.x + 1,
        row: target.area.y + 10,
        modifiers: KeyModifiers::NONE,
    })
    .expect("drag over target");
    assert_eq!(app.dragging.as_ref().unwrap().target_column, Some(2));
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: target.area.x + 1,
        row: target.area.y + 10,
        modifiers: KeyModifiers::NONE,
    })
    .expect("drop");
    assert!(app.dragging.is_none());
    assert_eq!(
        app.ops
            .get_task(&source_task)
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "review"
    );
}

#[test]
fn drag_is_visualized_with_lifted_card_drop_target_and_status_hint() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (column, card, source_rect) = card_hits(&app)[0];
    app.focused_column = column;
    app.focused_card = card;
    let source_task = app.visible_tasks_for_column(column)[card].id.clone();
    let target_column = (0..app.board.columns.len())
        .find(|&index| index != column)
        .expect("a second column");
    let target_name = app.board.columns[target_column].name.clone();
    let target = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::ColumnFocus(target_column))
        .copied()
        .expect("target column area");

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: source_rect.x + 1,
        row: source_rect.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("start drag");
    assert!(app.dragging.is_some());
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: target.area.x + 1,
        row: target.area.y + 10,
        modifiers: KeyModifiers::NONE,
    })
    .expect("drag over target");
    assert_eq!(app.drop_target_column(), Some(target_column));

    let ok = app.theme.ok;
    let snapshot = render_snapshot(&mut app);
    assert!(
        snapshot.contains(&format!("Moving {source_task} → {target_name}")),
        "status bar should announce the pending move:\n{snapshot}"
    );

    // The destination column border reads as a bold green drop zone, distinct
    // from ordinary blue focus.
    let border = style_at(&mut app, 96, 28, target.area.x, target.area.y);
    assert_eq!(border.fg, Some(ok));
    assert!(border.add_modifier.contains(Modifier::BOLD));

    // The card in flight is inverted so its origin stays visible.
    let lifted = style_at(&mut app, 96, 28, source_rect.x + 1, source_rect.y + 1);
    assert!(
        lifted.add_modifier.contains(Modifier::REVERSED),
        "the dragged source card should render lifted"
    );
}

#[test]
fn phase_three_focused_card_can_be_dragged_without_opening_detail() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (column, card, source) = card_hits(&app)[0];
    app.focused_column = column;
    app.focused_card = card;
    let target_column = (column + 1) % app.board.columns.len();
    let target = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::ColumnFocus(target_column))
        .copied()
        .expect("target column");
    let task_id = app.visible_tasks_for_column(column)[card].id.clone();

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: source.x + 1,
        row: source.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("press focused card");
    assert_eq!(app.screen, Screen::Board);
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: target.area.x + 1,
        row: target.area.y + 2,
        modifiers: KeyModifiers::NONE,
    })
    .expect("drag focused card");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: target.area.x + 1,
        row: target.area.y + 2,
        modifiers: KeyModifiers::NONE,
    })
    .expect("drop focused card");

    assert_eq!(app.screen, Screen::Board);
    assert_eq!(
        app.ops.get_task(&task_id).unwrap().unwrap().status.as_str(),
        app.board.columns[target_column].id
    );
}

#[test]
fn phase_three_scroll_truncation_and_session_cache_render() {
    let (dir, mut app) = app_with_board();
    app.settings.max_tasks_per_column = 2;
    let mut running = app.ops.create_task(NewTask::titled("Running")).unwrap();
    SessionManager::new(dir.path())
        .link_session(&running.id, "ses-running")
        .unwrap();
    running.session = Some("ses-running".to_string());
    app.ops.storage.save_task(&running).unwrap();
    for number in 0..3 {
        app.ops
            .create_task(NewTask::titled(format!("Truncated {number}")))
            .unwrap();
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    let output = render_at(&mut app, 96, 18);
    assert!(output.contains("(2 of 4)"));
    assert!(output.contains("↓ 2 below"));
    insta::assert_snapshot!("phase_three_scroll_indicators", output);
    app.settings.max_tasks_per_column = 10;
    let output = render_at(&mut app, 96, 12);
    assert!(output.contains("↓"));
    assert!(output.contains("▶ running"));
    app.focused_card = 1;
    app.column_offsets[0] = 1;
    assert!(render_at(&mut app, 96, 12).contains("↑ 1 above"));
    assert_eq!(
        app.board.session_states.get(&running.id),
        Some(&crate::core::session::SessionState::Live)
    );
    SessionManager::new(dir.path())
        .crash_session("ses-running")
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    assert_eq!(
        app.board.session_states.get(&running.id),
        Some(&crate::core::session::SessionState::Crashed)
    );
    app.column_offsets[0] = 0;
    let crashed = render_at(&mut app, 96, 18);
    assert!(crashed.contains("✖ crashed"));
    insta::assert_snapshot!("phase_three_crashed_card", crashed);
}

#[test]
fn running_card_and_sessions_show_live_telemetry() {
    let (dir, mut app) = app_with_board();
    let mut running = app.ops.create_task(NewTask::titled("Telemetry")).unwrap();
    SessionManager::new(dir.path())
        .link_session(&running.id, "ses-tel")
        .unwrap();
    running.session = Some("ses-tel".to_string());
    running.agent_backend = Some("claude".to_string());
    app.ops.storage.save_task(&running).unwrap();
    // A claude transcript: 2/3 todos completed, usage totalling 12.4k, last
    // tool an Edit. The card and Sessions list both derive from this on tick.
    let transcript = dir.path().join(".kanban/logs/ses-tel.transcript.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(
        &transcript,
        concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":12000,"output_tokens":400},"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"completed"},{"content":"c","status":"pending"}]}},{"type":"tool_use","name":"Edit","input":{"file_path":"src/auth/mod.rs"}}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    assert_eq!(
        app.board.session_states.get(&running.id),
        Some(&crate::core::session::SessionState::Live)
    );
    app.tick().unwrap();

    let board = render_at(&mut app, 120, 18);
    assert!(board.contains("▓"), "card shows a progress bar:\n{board}");
    assert!(board.contains("2/3"), "card shows todo count:\n{board}");
    assert!(
        board.contains("12.4k"),
        "card shows humanized tokens:\n{board}"
    );
    assert!(
        board.contains("→ Edit src/auth/mod.rs"),
        "card shows last activity:\n{board}"
    );

    // The Sessions list surfaces the same signals.
    app.handle_key(key(KeyCode::Char('l'))).unwrap();
    let sessions = render_at(&mut app, 120, 18);
    assert!(
        sessions.contains("2/3"),
        "sessions row shows todos:\n{sessions}"
    );
    assert!(
        sessions.contains("→ Edit"),
        "sessions row shows activity:\n{sessions}"
    );
}

#[test]
fn phase_three_mark_review_done_reconfirms_when_source_set_changes() {
    let (_dir, mut app) = app_with_board();
    let first = app.ops.create_task(NewTask::titled("First")).unwrap();
    app.ops.move_task(&first.id, "review", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.handle_key(key(KeyCode::Char('b'))).unwrap();

    let second = app.ops.create_task(NewTask::titled("Second")).unwrap();
    app.ops.move_task(&second.id, "review", false).unwrap();
    app.handle_key(key(KeyCode::Char('y'))).unwrap();

    assert!(app.status.contains("Tasks changed"));
    assert!(matches!(
        app.modal.as_ref().map(|modal| &modal.modal),
        Some(Modal::BulkConfirm { task_ids, .. }) if task_ids.len() == 2
    ));
    assert_eq!(
        app.ops
            .list_tasks(Some("review"), None, "created", "asc")
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn phase_three_drop_outside_cancels_after_hovering_a_target() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (column, card, source) = card_hits(&app)[0];
    let task_id = app.visible_tasks_for_column(column)[card].id.clone();
    let original = app.ops.get_task(&task_id).unwrap().unwrap().status;
    let target_column = (column + 1) % app.board.columns.len();
    let target = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::ColumnFocus(target_column))
        .copied()
        .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: source.x + 1,
        row: source.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: target.area.x + 1,
        row: target.area.y + 2,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 500,
        row: 500,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();

    assert!(app.dragging.is_none());
    assert_eq!(
        app.ops.get_task(&task_id).unwrap().unwrap().status,
        original
    );
}

#[test]
fn phase_three_clamps_nonfocused_offsets_after_filtering() {
    let (_dir, mut app) = app_with_board();
    for title in ["Other 1", "Other 2", "Needle", "Other 3"] {
        let task = app.ops.create_task(NewTask::titled(title)).unwrap();
        app.ops.move_task(&task.id, "review", false).unwrap();
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    let review = review_column(&app);
    app.column_offsets[review] = 3;
    app.search.query.insert_str("Needle");
    app.clamp_focus();

    assert_eq!(app.column_offsets[review], 0);
    assert!(render_at(&mut app, 96, 18).contains("Needle"));
}

#[test]
fn phase_three_question_count_matches_filtered_and_capped_tasks() {
    let (_dir, mut app) = app_with_board();
    app.ops.create_task(NewTask::titled("Visible")).unwrap();
    let hidden = app.ops.create_task(NewTask::titled("Hidden")).unwrap();
    app.ops
        .ask_question(&hidden.id, "Hidden question?", "agent", vec![])
        .unwrap();
    app.settings.max_tasks_per_column = 1;
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let output = render_at(&mut app, 96, 18);
    assert!(!output.contains("? 1 questions"));
    app.dispatch(UiAction::FocusQuestions).unwrap();
    assert_eq!(app.status, "No tasks have open questions");
}

#[test]
fn phase_three_cached_live_session_expires_without_a_file_change() {
    let (dir, mut app) = app_with_board();
    let mut task = app.ops.create_task(NewTask::titled("Expiring")).unwrap();
    SessionManager::new(dir.path())
        .link_session(&task.id, "ses-expiring")
        .unwrap();
    task.session = Some("ses-expiring".to_string());
    app.ops.storage.save_task(&task).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    let deadline = app.board.session_deadlines[&task.id];

    app.expire_session_states_at(deadline + chrono::Duration::seconds(1));

    assert_eq!(
        app.board.session_states.get(&task.id),
        Some(&crate::core::session::SessionState::Crashed)
    );
    assert!(render_at(&mut app, 96, 18).contains("✖ crashed"));
}

#[test]
fn phase_three_waiting_session_shows_deadline_across_tui() {
    let (dir, mut app) = app_with_board();
    let mut task = app
        .ops
        .create_task(NewTask::titled("Waiting export"))
        .unwrap();
    task.status = crate::core::models::TaskStatus::InProgress;
    task.session = Some("ses-wait-card".to_string());
    app.ops.storage.save_task(&task).unwrap();
    let manager = SessionManager::new(dir.path());
    manager.link_session(&task.id, "ses-wait-card").unwrap();
    let deadline = crate::core::timefmt::now() + chrono::Duration::hours(1);
    manager
        .set_wait(
            "ses-wait-card",
            deadline,
            Some("analytics export".to_string()),
        )
        .unwrap();
    let expected = format!("until {}", deadline.format("%H:%M"));
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let board = render_at(&mut app, 120, 18);
    assert!(board.contains(&expected), "{board}");

    app.focused_column = 1;
    app.dispatch(UiAction::OpenDetail).unwrap();
    let detail = render_at(&mut app, 120, 24);
    assert!(
        detail.contains(&format!("Waiting until {}", deadline.format("%H:%M"))),
        "{detail}"
    );
    assert!(detail.contains("analytics export"), "{detail}");

    app.handle_key(key(KeyCode::Char('l'))).unwrap();
    let sessions = render_at(&mut app, 120, 18);
    assert!(sessions.contains("⏳"), "{sessions}");
    assert!(sessions.contains("ses-wait-card"), "{sessions}");
    assert!(sessions.contains(&expected), "{sessions}");
}

#[test]
fn phase_three_waiting_session_deadline_expiry_does_not_mark_crashed_locally() {
    let (dir, mut app) = app_with_board();
    let mut task = app
        .ops
        .create_task(NewTask::titled("Still waiting"))
        .unwrap();
    task.status = crate::core::models::TaskStatus::InProgress;
    task.session = Some("ses-wait-expire".to_string());
    app.ops.storage.save_task(&task).unwrap();
    let manager = SessionManager::new(dir.path());
    manager.link_session(&task.id, "ses-wait-expire").unwrap();
    let deadline = crate::core::timefmt::now() + chrono::Duration::seconds(1);
    manager
        .set_wait("ses-wait-expire", deadline, Some("still alive".to_string()))
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    app.expire_session_states_at(deadline + chrono::Duration::seconds(1));

    assert_eq!(
        app.board.session_states.get(&task.id),
        Some(&crate::core::session::SessionState::Waiting)
    );
    let board = render_at(&mut app, 120, 18);
    assert!(!board.contains("✖ crashed"), "{board}");
}

#[test]
fn phase_three_live_session_detail_does_not_show_wait_deadline() {
    let (dir, mut app) = app_with_board();
    let mut task = app
        .ops
        .create_task(NewTask::titled("Just running"))
        .unwrap();
    task.status = crate::core::models::TaskStatus::InProgress;
    task.session = Some("ses-live-detail".to_string());
    app.ops.storage.save_task(&task).unwrap();
    SessionManager::new(dir.path())
        .link_session(&task.id, "ses-live-detail")
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    app.focused_column = 1;
    app.dispatch(UiAction::OpenDetail).unwrap();
    let detail = render_at(&mut app, 120, 24);

    assert!(!detail.contains("Waiting until"), "{detail}");
    assert!(!detail.contains("blocked on a question"), "{detail}");
}

#[test]
fn phase_three_questioned_task_with_closed_session_is_not_marked_crashed() {
    let (dir, mut app) = app_with_board();
    let mut task = app
        .ops
        .create_task(NewTask {
            title: "Needs answer".to_string(),
            interactive: true,
            ..Default::default()
        })
        .unwrap();
    task.status = crate::core::models::TaskStatus::InProgress;
    task.session = Some("ses-question-closed".to_string());
    app.ops.storage.save_task(&task).unwrap();
    app.ops
        .ask_question(&task.id, "Please answer?", "agent", vec![])
        .unwrap();
    let manager = SessionManager::new(dir.path());
    manager
        .link_session(&task.id, "ses-question-closed")
        .unwrap();
    manager.close_session("ses-question-closed").unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let board = render_at(&mut app, 120, 18);

    assert!(board.contains("? Please answer?"), "{board}");
    assert!(!board.contains("✖ crashed"), "{board}");
    assert!(!board.contains("u recover"), "{board}");
}

#[test]
fn phase_three_closed_session_in_progress_is_idle_not_crashed() {
    let (dir, mut app) = app_with_board();
    let mut task = app
        .ops
        .create_task(NewTask::titled("Finished asking"))
        .unwrap();
    task.status = crate::core::models::TaskStatus::InProgress;
    task.session = Some("ses-closed".to_string());
    app.ops.storage.save_task(&task).unwrap();
    let manager = SessionManager::new(dir.path());
    manager.link_session(&task.id, "ses-closed").unwrap();
    manager.close_session("ses-closed").unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let board = render_at(&mut app, 120, 18);
    assert!(!board.contains("✖ crashed"), "{board}");
    assert!(!app.board.session_states.contains_key(&task.id));
    assert!(board.contains("r run"), "{board}");
    assert!(!board.contains("r revoke"), "{board}");

    app.focused_column = 1;
    app.dispatch(UiAction::OpenDetail).unwrap();
    let detail = render_at(&mut app, 120, 24);
    assert!(!detail.contains("press u / Recover"), "{detail}");
    assert!(!detail.contains("[ Recover u ]"), "{detail}");
    assert!(detail.contains("Run r"), "{detail}");
    assert!(!detail.contains("Revoke r"), "{detail}");

    app.handle_key(key(KeyCode::Char('r'))).unwrap();
    assert!(
        app.status.starts_with("Started"),
        "closed In Progress must run, not revoke: {}",
        app.status
    );
}

#[test]
fn phase_three_missing_session_file_in_progress_is_still_crashed() {
    let (_dir, mut app) = app_with_board();
    let mut task = app
        .ops
        .create_task(NewTask::titled("Lost session"))
        .unwrap();
    task.status = crate::core::models::TaskStatus::InProgress;
    task.session = Some("ses-gone".to_string());
    app.ops.storage.save_task(&task).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let board = render_at(&mut app, 120, 18);
    assert!(board.contains("✖ crashed · u recover"), "{board}");

    app.focused_column = 1;
    app.dispatch(UiAction::OpenDetail).unwrap();
    let detail = render_at(&mut app, 120, 24);
    assert!(detail.contains("press u / Recover"), "{detail}");
    assert!(detail.contains("[ Recover u ]"), "{detail}");
}

#[test]
fn phase_three_wide_unicode_headers_stay_minimal() {
    let (_dir, mut app) = app_with_board();
    app.board.columns[1].name = "進行中🙂🙂".to_string();
    let output = render_at(&mut app, 120, 18);
    assert!(output.contains("(0)"));
    assert!(!output.contains("[+]"));
    assert!(!output.contains("⇒Review"));
    assert!(!output.contains("in_progress"));
}

#[test]
fn phase_six_sessions_show_state_kill_confirm_and_log_view() {
    let (dir, mut app) = app_with_board();
    let live = app.ops.create_task(NewTask::titled("Live task")).unwrap();
    let crashed = app
        .ops
        .create_task(NewTask::titled("Crashed task"))
        .unwrap();
    let manager = SessionManager::new(dir.path());
    manager.link_session(&live.id, "ses-live").unwrap();
    manager.link_session(&crashed.id, "ses-lost").unwrap();
    manager.crash_session("ses-lost").unwrap();
    std::fs::create_dir_all(dir.path().join(".kanban/logs")).unwrap();
    std::fs::write(
        dir.path().join(".kanban/logs/ses-live.log"),
        "line one\nline two\ntokens: 5\n",
    )
    .unwrap();

    app.handle_key(key(KeyCode::Char('l'))).unwrap();
    assert_eq!(app.active_sessions.len(), 2);
    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("▶ ses-live"));
    assert!(rendered.contains("✖ ses-lost"));
    insta::assert_snapshot!("phase_six_sessions_states", rendered);

    // `v` opens a pager over the log tail; `q` returns to the list.
    app.session_selected = app
        .filtered_active_sessions()
        .iter()
        .position(|active| active.session.id == "ses-live")
        .unwrap();
    app.handle_key(key(KeyCode::Char('v'))).unwrap();
    assert_eq!(app.screen, Screen::LogView);
    let log = render_snapshot(&mut app);
    assert!(log.contains("line one"));
    insta::assert_snapshot!("phase_six_log_view", log);
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert_eq!(app.screen, Screen::Sessions);

    // `x` kills only after the confirmation.
    app.session_selected = app
        .filtered_active_sessions()
        .iter()
        .position(|active| active.session.id == "ses-live")
        .unwrap();
    app.handle_key(key(KeyCode::Char('x'))).unwrap();
    assert!(matches!(
        app.modal.as_ref().expect("kill confirm").modal,
        Modal::KillSessionConfirm { .. }
    ));
    app.handle_key(key(KeyCode::Char('y'))).unwrap();
    assert!(app.status.starts_with("Stopped ses-live"), "{}", app.status);
    assert!(
        app.filtered_active_sessions()
            .iter()
            .all(|active| active.session.id != "ses-live")
    );
}

#[test]
fn phase_six_session_task_detail_returns_to_sessions() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Session task"))
        .unwrap();
    SessionManager::new(dir.path())
        .link_session(&task.id, "ses-open")
        .unwrap();
    app.handle_key(key(KeyCode::Char('l'))).unwrap();
    app.handle_key(key(KeyCode::Char('o'))).unwrap();
    assert_eq!(app.screen, Screen::Detail);
    assert_eq!(app.detail.as_ref().unwrap().task_id, task.id);
    app.handle_key(key(KeyCode::Char('q'))).unwrap();
    assert_eq!(app.screen, Screen::Sessions);
    assert!(app.detail.is_none());
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert_eq!(app.screen, Screen::Board);
}

#[test]
fn phase_six_archive_restore_confirms_and_returns_task_to_todo() {
    let (_dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Restore me")).unwrap();
    app.ops.move_task(&task.id, "archive", false).unwrap();
    app.handle_key(key(KeyCode::Char('a'))).unwrap();
    app.handle_key(key(KeyCode::Char('u'))).unwrap();
    assert!(matches!(
        app.modal.as_ref().expect("restore confirm").modal,
        Modal::RestoreConfirm { .. }
    ));
    app.handle_key(key(KeyCode::Char('y'))).unwrap();
    assert!(app.status.contains("Restored"), "{}", app.status);
    assert_eq!(
        app.ops.get_task(&task.id).unwrap().unwrap().status.as_str(),
        "todo"
    );
    assert_eq!(app.screen, Screen::Archive);

    // The detail of an archived task offers only Restore/Delete; `u` restores
    // from there too, and Esc returns to the archive list.
    let second = app
        .ops
        .create_task(NewTask::titled("Archived detail"))
        .unwrap();
    app.ops.move_task(&second.id, "archive", false).unwrap();
    app.reload_if_changed().unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert_eq!(app.screen, Screen::Detail);
    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("[ Restore u ]"));
    assert!(!rendered.contains("[ ▶ Run r ]"));
    app.handle_key(key(KeyCode::Char('u'))).unwrap();
    app.handle_key(key(KeyCode::Char('y'))).unwrap();
    assert_eq!(
        app.ops
            .get_task(&second.id)
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "todo"
    );
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert_eq!(app.screen, Screen::Archive);
    assert!(app.detail.is_none());
}

#[test]
fn phase_seven_status_bar_is_contextual_and_clickable() {
    let (_dir, mut app) = app_with_board();
    let rendered = render_at(&mut app, 140, 18);
    assert!(rendered.contains("n new"));
    assert!(rendered.contains("b review done"));
    let help_hit = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::Action(UiAction::Help))
        .copied()
        .expect("help segment hitbox");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: help_hit.area.x + 1,
        row: help_hit.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    assert_eq!(app.screen, Screen::Help);
    app.handle_key(key(KeyCode::Char('?'))).unwrap();
    assert_eq!(app.screen, Screen::Board);

    app.handle_key(key(KeyCode::Char('l'))).unwrap();
    let sessions = render_snapshot(&mut app);
    assert!(sessions.contains("x kill"));
    assert!(
        app.hitboxes
            .iter()
            .any(|hitbox| hitbox.action == HitAction::Action(UiAction::ViewLog))
    );
    app.handle_key(key(KeyCode::Char('q'))).unwrap();

    // A narrow terminal drops low-priority segments instead of clipping.
    let narrow = render_at(&mut app, 48, 18);
    assert!(narrow.contains("r run"));
    assert!(!narrow.contains("b review done"));
}

#[test]
fn phase_seven_help_overlay_scrolls_and_toggles() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('?'))).unwrap();
    let rendered = render_at(&mut app, 80, 20);
    assert!(
        rendered.contains(concat!("kanban4ai TUI v", env!("CARGO_PKG_VERSION"))),
        "help overlay should show the current app version: {rendered}"
    );
    assert!(app.help_max_scroll > 0, "help must scroll at 80x20");
    app.handle_key(key(KeyCode::Down)).unwrap();
    assert_eq!(app.help_scroll, 1);
    app.handle_key(key(KeyCode::End)).unwrap();
    assert_eq!(app.help_scroll, app.help_max_scroll);
    app.handle_key(key(KeyCode::Char('?'))).unwrap();
    assert_eq!(app.screen, Screen::Board);
}

#[test]
fn phase_seven_first_click_on_keyboard_focused_card_opens_detail() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (column, card, area) = card_hits(&app)[0];
    assert_eq!((app.focused_column, app.focused_card), (column, card));
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 1,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    let release = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        ..click
    };
    app.handle_mouse(click).unwrap();
    assert_eq!(
        app.screen,
        Screen::Board,
        "press still allows drag cancellation"
    );
    app.handle_mouse(release).unwrap();
    assert_eq!(app.screen, Screen::Detail);
}

#[test]
fn phase_four_confirm_buttons_support_one_key_and_mouse() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Delete by click"))
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.handle_key(key(KeyCode::Char('d'))).unwrap();
    let output = render_at(&mut app, 80, 24);
    assert!(output.contains("[ Yes ]  [ No ]"));
    insta::assert_snapshot!("phase_four_confirm_buttons", output);
    let yes = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::ModalButton(ModalButton::Yes))
        .copied()
        .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: yes.area.x,
        row: yes.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    assert!(app.ops.get_task(&task.id).unwrap().is_none());
}

#[test]
fn phase_four_forms_scroll_validate_and_protect_dirty_input() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    for _ in 0..6 {
        app.handle_key(key(KeyCode::Tab)).unwrap();
    }
    let output = render_at(&mut app, 80, 24);
    assert!(output.contains("Chain to"));
    insta::assert_snapshot!("phase_four_form_scrolled_80x24", output);

    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    assert!(!app.modal.as_ref().unwrap().is_dirty());
    for _ in 0..8 {
        app.handle_key(key(KeyCode::Tab)).unwrap();
    }
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert_eq!(
        app.modal.as_ref().unwrap().error.as_deref(),
        Some("Task title cannot be empty")
    );
    assert_eq!(
        app.modal.as_ref().unwrap().active_field(),
        DialogField::Title
    );
    insta::assert_snapshot!("phase_four_inline_validation", render_at(&mut app, 80, 24));
    app.modal.as_mut().unwrap().focus_field(DialogField::Title);
    app.handle_key(key(KeyCode::Char('x'))).unwrap();
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert!(app.modal.as_ref().unwrap().discard_confirm);
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    assert_eq!(app.modal.as_ref().unwrap().title_text(), "x");
    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('y'))).unwrap();
    assert!(app.modal.is_none());
}

#[test]
fn phase_four_dirty_escape_preserves_whitespace_only_input() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    app.handle_key(key(KeyCode::Char(' '))).unwrap();
    assert!(app.modal.as_ref().unwrap().is_dirty());

    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert!(app.modal.as_ref().unwrap().discard_confirm);
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    assert_eq!(app.modal.as_ref().unwrap().title.lines(), [" "]);
}

#[test]
fn phase_four_scrolled_selector_click_uses_visible_option_index() {
    let (_dir, mut app) = populated_app();
    for title in ["Chain one", "Chain two", "Chain three", "Chain four"] {
        app.ops.create_task(NewTask::titled(title)).unwrap();
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    for _ in 0..6 {
        app.handle_key(key(KeyCode::Tab)).unwrap();
    }
    for _ in 0..4 {
        app.handle_key(key(KeyCode::Down)).unwrap();
    }
    assert_eq!(app.modal.as_ref().unwrap().chain_selected, 4);
    let expected = app.modal.as_ref().unwrap().chain_options[3].value.clone();

    let _ = render_at(&mut app, 80, 24);
    let first_visible = app
        .hitboxes
        .iter()
        .find(|hitbox| {
            hitbox.action
                == HitAction::ModalOption {
                    field: DialogField::ChainTo,
                    index: 3,
                }
        })
        .copied()
        .expect("first visible scrolled option");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: first_visible.area.x,
        row: first_visible.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    let modal = app.modal.as_ref().unwrap();
    assert_eq!(modal.chain_selected, 3);
    assert_eq!(modal.chain_text(), expected);
}

#[test]
fn phase_four_tall_form_expands_visible_task_selector_options() {
    let (_dir, mut app) = populated_app();
    for number in 0..8 {
        app.ops
            .create_task(NewTask::titled(format!("Tall chain {number}")))
            .unwrap();
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    for _ in 0..6 {
        app.handle_key(key(KeyCode::Tab)).unwrap();
    }
    let _ = render_at(&mut app, 100, 60);
    let visible_options = app
        .hitboxes
        .iter()
        .filter(|hitbox| {
            matches!(
                hitbox.action,
                HitAction::ModalOption {
                    field: DialogField::ChainTo,
                    ..
                }
            )
        })
        .count();
    assert!(visible_options > 2, "visible options: {visible_options}");
}

#[test]
fn phase_four_answer_reselection_and_boundary_keys_preserve_typed_answer() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('w'))).unwrap();
    app.handle_key(key(KeyCode::Tab)).unwrap();
    app.handle_key(key(KeyCode::Tab)).unwrap();
    for character in "typed answer".chars() {
        app.handle_key(key(KeyCode::Char(character))).unwrap();
    }
    app.modal.as_mut().unwrap().error = Some("keep this error".to_string());
    let _ = render_at(&mut app, 96, 28);

    for field in [
        DialogField::Variant,
        DialogField::Question,
        DialogField::Variant,
    ] {
        let hit = app
            .hitboxes
            .iter()
            .find(|hitbox| hitbox.action == HitAction::ModalOption { field, index: 0 })
            .copied()
            .unwrap();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.area.x,
            row: hit.area.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
        app.handle_key(key(KeyCode::Enter)).unwrap();
        app.handle_key(key(KeyCode::Up)).unwrap();
    }

    let modal = app.modal.as_ref().unwrap();
    assert_eq!(modal.answer_text(), "typed answer");
    assert_eq!(modal.error.as_deref(), Some("keep this error"));
}

#[test]
fn phase_four_modal_mouse_routes_fields_options_and_add_message_buttons() {
    let (_dir, mut app) = populated_app();
    let original_focus = (app.focused_column, app.focused_card);
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    let _ = render_at(&mut app, 96, 28);
    let option = app
        .hitboxes
        .iter()
        .find(|hitbox| {
            hitbox.action
                == HitAction::ModalOption {
                    field: DialogField::Backend,
                    index: 1,
                }
        })
        .copied()
        .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: option.area.x,
        row: option.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    assert_eq!(
        app.modal.as_ref().unwrap().backend_text().as_deref(),
        Some("opencode")
    );
    assert_eq!((app.focused_column, app.focused_card), original_focus);
    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('y'))).unwrap();

    app.handle_key(key(KeyCode::Char('c'))).unwrap();
    let output = render_at(&mut app, 96, 28);
    assert!(output.contains("[ Save ]  [ Cancel ]"));
    let save_hint = "(Ctrl + S)";
    let nav_hint = "use Tab or Shift + Tab to navigate";
    let hint_line = output
        .lines()
        .find(|line| line.contains(save_hint))
        .expect("save hint row");
    let save_hint_start = hint_line.find(save_hint).expect("save hint position");
    let nav_hint_start = hint_line.find(nav_hint).expect("navigation hint position");
    assert!(hint_line[..save_hint_start].ends_with('│'));
    assert!(hint_line[nav_hint_start + nav_hint.len()..].starts_with('│'));
    let save = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::ModalButton(ModalButton::Save))
        .copied()
        .unwrap();
    app.modal
        .as_mut()
        .unwrap()
        .description
        .insert_str("clicked save");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: save.area.x,
        row: save.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    assert!(app.modal.is_none());
}

/// A task with a recorded prompt dump and harvested input-provenance exposes the
/// two read-only viewer buttons; activating each opens a `TextView` pager over
/// the matching content and `q` returns to the detail screen. Provenance is not
/// in the thread — the `v` popup is its only home.
#[test]
fn prompt_and_inputs_buttons_open_read_only_viewers() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Viewer task"))
        .expect("create task");

    // A prompt dump is written per launched session; simulate one launch.
    let session_id = "ses-viewer-1";
    let mut stored = app.ops.get_task(&task.id).unwrap().unwrap();
    stored.session = Some(session_id.to_string());
    app.ops.storage.save_task(&stored).expect("save session");
    std::fs::write(
        dir.path()
            .join(".kanban/logs")
            .join(format!("{session_id}.prompt.txt")),
        "PROMPT-MARKER assembled body\nsecond line\n",
    )
    .expect("write prompt dump");

    // Provenance is referenced from an agent_step exit line and loaded by id.
    ThreadManager::new(dir.path())
        .unwrap()
        .post(
            &task.id,
            crate::core::models::MessageRole::System,
            crate::core::models::MessageKind::AgentStep,
            &format!(
                "■ exit session={session_id} code=0 outcome=Closed → \
                 provenance: .kanban/provenance/{session_id}.yaml"
            ),
            None,
            Vec::new(),
            Some("kanban".to_string()),
        )
        .unwrap();
    crate::core::provenance::write_manifest(
        &app.ops.storage.provenance_dir,
        &crate::core::provenance::InputManifest {
            session_id: session_id.to_string(),
            backend: "claude".to_string(),
            reads: vec!["src/INPUT-MARKER.rs".to_string()],
            generated_at: "2026-07-21T00:00:00".to_string(),
            ..Default::default()
        },
    )
    .expect("write manifest");

    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    assert_eq!(app.screen, Screen::Detail);

    let detail = app.detail.as_ref().expect("detail state");
    assert!(detail.has_prompt, "prompt dump should be detected");
    assert!(detail.has_provenance, "provenance should be detected");

    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("Prompt p"), "prompt button missing");
    assert!(rendered.contains("Inputs v"), "inputs button missing");

    // View inputs (provenance).
    app.handle_key(key(KeyCode::Char('v')))
        .expect("view inputs");
    assert_eq!(app.screen, Screen::TextView);
    let view = app.text_view.as_ref().expect("inputs view");
    assert!(view.title.contains("Inputs (provenance)"));
    assert!(view.lines.iter().any(|line| line.contains("INPUT-MARKER")));
    app.handle_key(key(KeyCode::Char('q')))
        .expect("back to detail");
    assert_eq!(app.screen, Screen::Detail);
    assert!(app.text_view.is_none());

    // View prompt.
    app.handle_key(key(KeyCode::Char('p')))
        .expect("view prompt");
    assert_eq!(app.screen, Screen::TextView);
    let view = app.text_view.as_ref().expect("prompt view");
    assert!(view.title.contains("Prompt"));
    assert!(view.lines.iter().any(|line| line.contains("PROMPT-MARKER")));

    insta::assert_snapshot!("prompt_view", render_snapshot(&mut app));

    app.handle_key(key(KeyCode::Esc))
        .expect("esc back to detail");
    assert_eq!(app.screen, Screen::Detail);
}

/// The prompt viewer wraps a line that is wider than the viewport onto the next
/// row instead of running past the right edge, so a token that sits beyond the
/// window width stays visible (matching the detail description panel).
#[test]
fn prompt_viewer_wraps_long_lines() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Wrapping task"))
        .expect("create task");

    let session_id = "ses-wrap-1";
    let mut stored = app.ops.get_task(&task.id).unwrap().unwrap();
    stored.session = Some(session_id.to_string());
    app.ops.storage.save_task(&stored).expect("save session");
    // A single logical line far wider than the 96-column snapshot viewport, with
    // a unique marker at the very end that only a wrapped render can show.
    let long_line = format!("{}END-WRAP-MARKER\n", "filler ".repeat(40));
    std::fs::write(
        dir.path()
            .join(".kanban/logs")
            .join(format!("{session_id}.prompt.txt")),
        long_line,
    )
    .expect("write prompt dump");

    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.handle_key(key(KeyCode::Char('p')))
        .expect("view prompt");
    assert_eq!(app.screen, Screen::TextView);

    let rendered = render_snapshot(&mut app);
    assert!(
        rendered.contains("END-WRAP-MARKER"),
        "long line should wrap so its tail stays visible"
    );
}

/// A task with neither a prompt dump nor provenance hides both viewer buttons.
#[test]
fn viewer_buttons_absent_without_prompt_or_provenance() {
    let (_dir, mut app) = app_with_board();
    app.ops
        .create_task(NewTask::titled("Bare task"))
        .expect("create task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.handle_key(key(KeyCode::Enter)).expect("open detail");

    let detail = app.detail.as_ref().expect("detail state");
    assert!(!detail.has_prompt);
    assert!(!detail.has_provenance);

    let rendered = render_snapshot(&mut app);
    assert!(!rendered.contains("Prompt p"));
    assert!(!rendered.contains("Inputs v"));

    // The keybindings no-op (with a status hint) rather than opening an
    // empty viewer.
    app.handle_key(key(KeyCode::Char('p'))).expect("prompt key");
    assert_eq!(app.screen, Screen::Detail);
    app.handle_key(key(KeyCode::Char('v'))).expect("inputs key");
    assert_eq!(app.screen, Screen::Detail);
}

/// A bracketed paste is one edit in the focused field: tabs and newlines stay
/// as text instead of hopping fields and pressing the focused button.
#[test]
fn bracketed_paste_lands_whole_in_the_focused_dialog_field() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("new task modal")
        .focus_field(DialogField::Description);

    app.handle_paste("83\tspeed gen 5\t338ab\n84\tprofit gen 9\t422ab")
        .expect("paste");

    let modal = app.modal.as_ref().expect("modal still open");
    assert_eq!(modal.active_field(), DialogField::Description);
    assert_eq!(
        modal.description.lines(),
        ["83\tspeed gen 5\t338ab", "84\tprofit gen 9\t422ab"]
    );
}

/// The title holds one line, so a pasted block is flattened rather than
/// truncated to whatever fragment followed the last newline.
#[test]
fn paste_into_single_line_field_keeps_every_line() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("new task modal")
        .focus_field(DialogField::Title);

    app.handle_paste("first line\r\nsecond line")
        .expect("paste");

    let modal = app.modal.as_ref().expect("modal still open");
    assert_eq!(modal.title.lines(), ["first line second line"]);
}

/// Escape sequences in pasted text never reach the terminal.
#[test]
fn paste_sanitizes_control_sequences() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("new task modal")
        .focus_field(DialogField::Description);

    app.handle_paste("safe\u{001b}[31mred").expect("paste");

    let modal = app.modal.as_ref().expect("modal still open");
    assert_eq!(modal.description.lines(), ["safe\u{fffd}[31mred"]);
}

/// On the board a paste is dropped instead of being replayed as shortcuts.
#[test]
fn paste_without_a_text_field_is_ignored() {
    let (_dir, mut app) = app_with_board();

    app.handle_paste("nnnq").expect("paste");

    assert!(app.modal.is_none());
    assert_eq!(app.screen, Screen::Board);
    assert!(app.status.contains("Nothing pasted"));
}

/// The detail answer box takes pasted text on the current line.
#[test]
fn paste_fills_the_detail_answer_box() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Question holder"))
        .expect("create task");
    app.ops
        .ask_question(&task.id, "Which one?", "agent", vec![])
        .expect("ask");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.handle_key(key(KeyCode::Tab)).expect("focus answer");
    assert_eq!(
        app.detail.as_ref().expect("detail state").focus,
        DetailFocus::Answer
    );

    app.handle_paste("use\nthe second one").expect("paste");

    let detail = app.detail.as_ref().expect("detail state");
    assert_eq!(detail.answer_input.lines(), ["use the second one"]);
}

fn write_task_file(root: &std::path::Path, status: &str, id: &str) {
    let dir = root.join(".kanban/tasks").join(status);
    std::fs::create_dir_all(&dir).expect("task status dir");
    std::fs::write(
        dir.join(format!("{id}.md")),
        format!("---\nid: {id}\ntitle: {id}\nstatus: {status}\n---\n"),
    )
    .expect("task file");
}

/// Like [`write_task_file`] but with extra frontmatter lines, so a task can
/// carry `review_unseen: true` or `has_questions: true` for the projects-list
/// flags scan.
fn write_task_file_with_flags(root: &std::path::Path, status: &str, id: &str, extra: &str) {
    let dir = root.join(".kanban/tasks").join(status);
    std::fs::create_dir_all(&dir).expect("task status dir");
    std::fs::write(
        dir.join(format!("{id}.md")),
        format!("---\nid: {id}\ntitle: {id}\nstatus: {status}\n{extra}---\n"),
    )
    .expect("task file");
}

/// Stamp a project's `created_at` so list ordering by age is deterministic.
fn set_project_created_at(data_root: &std::path::Path, stamp: &str) {
    let file = data_root.join("project.yaml");
    let raw = std::fs::read_to_string(&file).expect("project.yaml");
    let updated = raw
        .lines()
        .map(|line| {
            if line.starts_with("created_at:") {
                format!("created_at: '{stamp}'")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&file, format!("{updated}\n")).expect("rewrite project.yaml");
}

/// A live session file, so the row's Agents count (`▶N`) is nonzero.
fn write_active_session(data_root: &std::path::Path, id: &str) {
    let dir = data_root.join(".kanban/sessions");
    std::fs::create_dir_all(&dir).expect("sessions dir");
    std::fs::write(
        dir.join(format!("{id}.yaml")),
        format!(
            "id: {id}\ntask_id: TASK-001\nstarted_at: '2026-08-17T10:00:00'\nstatus: active\nlast_seen: '2026-08-17T10:00:00'\n"
        ),
    )
    .expect("session file");
}

fn projects_app(
    work: &std::path::Path,
    create_cwd: Option<std::path::PathBuf>,
) -> (tempfile::TempDir, App) {
    let store_dir = tempfile::tempdir().expect("store");
    std::fs::create_dir_all(work).expect("work dir");
    let store = ProjectStore::at(store_dir.path());
    let added = store.add(work, Some("Demo Board")).expect("add project");
    write_task_file(&added.project.data_root, "todo", "TASK-001");
    write_task_file(&added.project.data_root, "in_progress", "TASK-002");
    let app = App::projects_at(store, None, create_cwd).expect("projects app");
    (store_dir, app)
}

/// A projects screen opened from a board (via `P`), so a return project
/// lies behind the list.
fn projects_app_with_return(work: &std::path::Path) -> (tempfile::TempDir, App) {
    let store_dir = tempfile::tempdir().expect("store");
    std::fs::create_dir_all(work).expect("work dir");
    let store = ProjectStore::at(store_dir.path());
    let added = store.add(work, Some("Demo Board")).expect("add project");
    let app = App::projects_at(store, Some(added.project.clone()), None).expect("projects app");
    (store_dir, app)
}

#[test]
fn projects_screen_lists_rows_and_create_cwd() {
    let work = std::path::PathBuf::from("/tmp/k4ai-snap-work");
    let cwd = std::path::PathBuf::from("/tmp/k4ai-snap-cwd");
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).expect("cwd");
    let (_store, mut app) = projects_app(&work, Some(cwd.clone()));
    assert_eq!(app.screen, Screen::Projects);
    assert_eq!(app.visible_project_items().len(), 2);
    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("Demo Board"), "{rendered}");
    assert!(rendered.contains("Create project for"), "{rendered}");
    assert!(
        rendered.contains("To Do  Doing  Review  Done"),
        "{rendered}"
    );
    insta::assert_snapshot!("projects_list", rendered);
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&cwd);
}

/// Column of `needle` in `line`, counted in characters so a row can be
/// compared against the header line it sits under.
fn column_of(line: &str, needle: &str) -> usize {
    let byte = line
        .find(needle)
        .unwrap_or_else(|| panic!("missing {needle} in: {line}"));
    line[..byte].chars().count()
}

/// The character the row shows in the last column of a right-aligned header.
fn cell_under(header: &str, row: &str, label: &str) -> char {
    let end = column_of(header, label) + label.chars().count();
    row.chars()
        .nth(end - 1)
        .unwrap_or_else(|| panic!("row is shorter than the {label} column: {row}"))
}

#[test]
fn projects_table_keeps_its_columns_across_mixed_rows() {
    let store_dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(store_dir.path());
    let root = std::path::PathBuf::from("/tmp/k4ai-table");
    let _ = std::fs::remove_dir_all(&root);

    // A busy board: two unreviewed tasks, an open question and a live agent.
    let busy = root.join("busy-service");
    std::fs::create_dir_all(&busy).expect("work dir");
    let added = store.add(&busy, Some("Busy Service")).expect("add busy");
    for id in ["TASK-001", "TASK-002", "TASK-003"] {
        write_task_file(&added.project.data_root, "done", id);
    }
    let review = added.project.data_root.join(".kanban/tasks/review");
    std::fs::create_dir_all(&review).expect("review dir");
    std::fs::write(
        review.join("TASK-004.md"),
        "---\nid: TASK-004\ntitle: TASK-004\nstatus: review\nreview_unseen: true\n---\n",
    )
    .expect("unseen review");
    let in_progress = added.project.data_root.join(".kanban/tasks/in_progress");
    std::fs::create_dir_all(&in_progress).expect("in_progress dir");
    std::fs::write(
        in_progress.join("TASK-005.md"),
        "---\nid: TASK-005\ntitle: TASK-005\nstatus: in_progress\nhas_questions: true\n---\n",
    )
    .expect("questioned task");
    std::fs::create_dir_all(added.project.data_root.join(".kanban/sessions")).expect("sessions");
    std::fs::write(
        added.project.data_root.join(".kanban/sessions/ses-a.yaml"),
        "id: ses-a\ntask_id: TASK-005\nstatus: active\nstarted_at: '2026-08-14T11:00:00'\nlast_seen: '2026-08-14T11:00:00'\n",
    )
    .expect("session");

    // A quiet board, and one whose folder was deleted after registration.
    let quiet = root.join("quiet");
    std::fs::create_dir_all(&quiet).expect("work dir");
    store.add(&quiet, Some("Quiet")).expect("add quiet");
    let gone = root.join("moved-away");
    std::fs::create_dir_all(&gone).expect("work dir");
    store
        .add(&gone, Some("A very long project name that will not fit"))
        .expect("add gone");
    std::fs::remove_dir_all(&gone).expect("drop work dir");

    let mut app = App::projects_at(store, None, None).expect("projects app");
    app.project_selected = 1;
    let rendered = render_at(&mut app, 100, 20);
    insta::assert_snapshot!("projects_table", rendered);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn projects_table_drops_trailing_columns_on_a_narrow_terminal() {
    let work = std::path::PathBuf::from("/tmp/k4ai-narrow-work");
    let _ = std::fs::remove_dir_all(&work);
    let (_store, mut app) = projects_app(&work, None);

    let wide = rendered_lines(&mut app, 100, 12).join("\n");
    assert!(
        wide.contains("Last opened") && wide.contains("Agents"),
        "{wide}"
    );

    let narrow = rendered_lines(&mut app, 70, 12).join("\n");
    assert!(narrow.contains("To Do"), "counts stay: {narrow}");
    assert!(
        narrow.contains("Agents"),
        "agents still fit at 70: {narrow}"
    );
    assert!(!narrow.contains("Last opened"), "{narrow}");

    let tiny = rendered_lines(&mut app, 50, 12).join("\n");
    assert!(tiny.contains("To Do") && tiny.contains("Done"), "{tiny}");
    assert!(!tiny.contains("Agents"), "{tiny}");
    assert!(tiny.contains("Demo Board"), "{tiny}");

    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn mouse_move_preselects_a_project_row_without_taking_the_selection() {
    let store_dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(store_dir.path());
    let root = tempfile::tempdir().expect("work root");
    for index in 1..=2 {
        let work = root.path().join(format!("p{index:02}"));
        std::fs::create_dir_all(&work).expect("work dir");
        store
            .add(&work, Some(&format!("Project {index:02}")))
            .expect("add project");
    }
    let mut app = App::projects_at(store, None, None).expect("projects app");
    let _ = render_at(&mut app, 100, 16);

    let hovered_row = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::FocusProject { index: 1 })
        .copied()
        .expect("second project hitbox")
        .area;
    let selected_row = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::FocusProject { index: 0 })
        .copied()
        .expect("first project hitbox")
        .area;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: hovered_row.x + 1,
        row: hovered_row.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover project row");

    assert!(app.is_hovered(HitAction::FocusProject { index: 1 }));
    // Preselection paints a row, it does not move the keyboard selection.
    assert_eq!(app.project_selected, 0);
    let hover_style = style_at(&mut app, 100, 16, hovered_row.x, hovered_row.y);
    let selected_style = style_at(&mut app, 100, 16, selected_row.x, selected_row.y);
    assert_eq!(hover_style.bg, Some(app.theme.hover));
    assert_eq!(selected_style.bg, Some(app.theme.border));
    assert_ne!(app.theme.hover, app.theme.border);
    assert_ne!(app.theme.hover, app.theme.bg);
}

/// The status-bar button hands the selected project's work folder to the
/// configured file manager, with the folder appended to the command.
#[test]
fn the_open_folder_button_hands_the_work_folder_to_the_file_manager() {
    let store_dir = tempfile::tempdir().expect("store");
    let root = tempfile::tempdir().expect("work root");
    let work = root.path().join("opened");
    std::fs::create_dir_all(&work).expect("work dir");
    let marker = root.path().join("marker");
    let store = ProjectStore::at(store_dir.path());
    store.add(&work, Some("Opened")).expect("add project");
    // A stand-in for the desktop opener: it records the path it was handed
    // instead of putting a window on the developer's screen.
    let mut config = store.load_global_config().expect("global config");
    config.tui.insert(
        serde_yaml_ng::Value::String("file_manager".to_string()),
        serde_yaml_ng::Value::String(format!("sh -c 'printf %s \"$0\" > {}'", marker.display())),
    );
    store.save_global_config(&config).expect("save config");

    let mut app =
        App::projects_at(ProjectStore::at(store_dir.path()), None, None).expect("projects app");
    let rendered = render_at(&mut app, 120, 16);
    let button = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::Action(UiAction::OpenProjectFolder))
        .copied()
        .expect("open-folder button")
        .area;
    assert!(rendered.contains("o folder"), "{rendered}");

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: button.x,
        row: button.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("press open-folder");
    assert!(
        app.status.starts_with("Opened "),
        "unexpected status: {}",
        app.status
    );

    // The opener is spawned detached, so wait for the path it recorded.
    let mut recorded = None;
    for _ in 0..200 {
        if let Ok(content) = std::fs::read_to_string(&marker) {
            recorded = Some(content);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(recorded.as_deref(), Some(work.to_string_lossy().as_ref()));
}

#[test]
fn opening_a_missing_project_folder_reports_it_instead_of_launching() {
    let store_dir = tempfile::tempdir().expect("store");
    let root = tempfile::tempdir().expect("work root");
    let work = root.path().join("gone");
    std::fs::create_dir_all(&work).expect("work dir");
    let store = ProjectStore::at(store_dir.path());
    store.add(&work, Some("Gone")).expect("add project");
    std::fs::remove_dir_all(&work).expect("remove work dir");

    let mut app = App::projects_at(store, None, None).expect("projects app");
    app.handle_key(key(KeyCode::Char('o'))).expect("press o");
    assert!(
        app.status.contains("Folder is missing"),
        "unexpected status: {}",
        app.status
    );
}

#[test]
fn projects_list_scrolls_the_selection_into_view_and_keeps_hitboxes_on_it() {
    let store_dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(store_dir.path());
    let root = tempfile::tempdir().expect("work root");
    for index in 1..=10 {
        let work = root.path().join(format!("p{index:02}"));
        std::fs::create_dir_all(&work).expect("work dir");
        store
            .add(&work, Some(&format!("Project {index:02}")))
            .expect("add project");
    }
    let mut app = App::projects_at(store, None, None).expect("projects app");

    let top = rendered_lines(&mut app, 100, 16).join("\n");
    assert!(top.contains("Project 01"), "{top}");
    assert!(!top.contains("Project 10"), "{top}");

    for _ in 0..9 {
        app.handle_key(key(KeyCode::Down)).expect("down");
    }
    let lines = rendered_lines(&mut app, 100, 16);
    let scrolled = lines.join("\n");
    assert!(scrolled.contains("Project 10"), "{scrolled}");
    assert!(!scrolled.contains("Project 01"), "{scrolled}");

    // Every row the mouse can hit sits on the project it claims to be.
    let items = app.visible_project_items();
    let hits = app
        .hitboxes
        .iter()
        .filter_map(|hitbox| match hitbox.action {
            HitAction::FocusProject { index } => Some((index, hitbox.area.y)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!hits.is_empty(), "{scrolled}");
    for (index, y) in hits {
        let super::projects::ProjectListItem::Project(row) = &items[index] else {
            panic!("hitbox {index} is not a project row");
        };
        assert!(
            lines[y as usize].contains(&row.display_name),
            "hitbox for {} is on the wrong line: {}",
            row.display_name,
            lines[y as usize]
        );
    }
}

/// Register `work` under its folder name and give the board a settings name.
fn project_with_board_name(
    store_dir: &std::path::Path,
    work: &std::path::Path,
    board_name: &str,
) -> ProjectStore {
    std::fs::create_dir_all(work).expect("work dir");
    let store = ProjectStore::at(store_dir);
    let added = store.add(work, None).expect("add project");
    let kanban = added.project.data_root.join(".kanban");
    std::fs::create_dir_all(&kanban).expect("kanban dir");
    std::fs::write(
        kanban.join("config.yaml"),
        format!("tui:\n  name: {board_name}\n  theme: textual-dark\n"),
    )
    .expect("board config");
    store
}

#[test]
fn projects_list_names_a_project_from_its_board_settings() {
    let store_dir = tempfile::tempdir().expect("store");
    let work = tempfile::tempdir().expect("work");
    let store = project_with_board_name(store_dir.path(), work.path(), "Ledger");
    let mut app = App::projects_at(store, None, None).expect("projects app");

    let folder = work
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("folder name");
    assert_eq!(
        app.projects[0].project.name, folder,
        "registry keeps the folder name"
    );
    assert_eq!(app.projects[0].display_name, "Ledger");

    let rendered = render_at(&mut app, 96, 12);
    assert!(rendered.contains("Ledger"), "{rendered}");
}

#[test]
fn renaming_a_project_also_renames_it_in_the_board_settings() {
    let store_dir = tempfile::tempdir().expect("store");
    let work = tempfile::tempdir().expect("work");
    let store = project_with_board_name(store_dir.path(), work.path(), "Ledger");
    let mut app = App::projects_at(store, None, None).expect("projects app");

    app.handle_key(key(KeyCode::Char('r'))).expect("rename");
    let modal = app.modal.as_mut().expect("rename modal");
    assert_eq!(
        modal.title_text(),
        "Ledger",
        "the dialog offers the shown name"
    );
    modal.title = TextArea::new(vec!["Ledger Book".to_string()]);
    app.handle_key(key(KeyCode::Tab)).expect("focus save");
    app.handle_key(key(KeyCode::Enter)).expect("save rename");
    assert!(app.modal.is_none(), "{}", app.status);

    assert_eq!(app.projects[0].display_name, "Ledger Book");
    assert_eq!(app.projects[0].project.name, "Ledger Book");
    let config = std::fs::read_to_string(
        app.projects[0]
            .project
            .data_root
            .join(".kanban/config.yaml"),
    )
    .expect("board config");
    assert!(config.contains("name: Ledger Book"), "{config}");
    // A rename has no business adding keys the board never had.
    assert!(!config.contains("auto_launch"), "{config}");
}

#[test]
fn project_row_lines_its_counts_up_under_the_column_headers() {
    let work = std::path::PathBuf::from("/tmp/k4ai-status-work");
    let _ = std::fs::remove_dir_all(&work);
    let (_store, mut app) = projects_app(&work, None);
    let lines = rendered_lines(&mut app, 96, 12);
    let row_at = lines
        .iter()
        .position(|line| line.contains("Demo Board"))
        .unwrap_or_else(|| panic!("missing project row: {lines:?}"));
    let header = lines
        .iter()
        .find(|line| line.contains("To Do"))
        .unwrap_or_else(|| panic!("missing table header: {lines:?}"));
    let row = &lines[row_at];

    assert_eq!(cell_under(header, row, "To Do"), '1');
    assert_eq!(cell_under(header, row, "Doing"), '1');
    assert_eq!(cell_under(header, row, "Review"), '0');
    assert_eq!(cell_under(header, row, "Done"), '0');
    assert!(
        column_of(row, "Demo Board") < column_of(header, "To Do"),
        "the name column comes first, got: {row}"
    );

    // The folder sits on the row's second line, under the name.
    let folder = &lines[row_at + 1];
    assert_eq!(
        column_of(folder, "/tmp/k4ai-status-work"),
        column_of(row, "Demo Board"),
        "the folder should start in the name column, got: {folder}"
    );
    assert_eq!(
        column_of(header, "Project"),
        column_of(row, "Demo Board"),
        "the name should start under its header, got: {row}"
    );
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn project_row_places_running_and_unreviewed_status_next_to_the_name() {
    let work = std::path::PathBuf::from("/tmp/k4ai-status-run");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("work dir");
    let store_dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(store_dir.path());
    let added = store.add(&work, Some("Demo Board")).expect("add project");
    write_task_file(&added.project.data_root, "todo", "TASK-001");
    write_task_file(&added.project.data_root, "in_progress", "TASK-002");
    let review_dir = added.project.data_root.join(".kanban/tasks/review");
    std::fs::create_dir_all(&review_dir).expect("review dir");
    std::fs::write(
        review_dir.join("TASK-003.md"),
        "---\nid: TASK-003\ntitle: TASK-003\nstatus: review\nreview_unseen: true\n---\n",
    )
    .expect("unseen review");
    std::fs::create_dir_all(added.project.data_root.join(".kanban/sessions")).expect("sessions");
    std::fs::write(
        added
            .project
            .data_root
            .join(".kanban/sessions/ses-test.yaml"),
        "id: ses-test\ntask_id: TASK-002\nstatus: active\nstarted_at: '2026-08-14T11:00:00'\nlast_seen: '2026-08-14T11:00:00'\n",
    )
    .expect("session");
    let mut app = App::projects_at(store, None, None).expect("projects app");
    let lines = rendered_lines(&mut app, 96, 12);
    let row_at = lines
        .iter()
        .position(|line| line.contains("Demo Board"))
        .unwrap_or_else(|| panic!("missing project row: {lines:?}"));
    let header = lines
        .iter()
        .find(|line| line.contains("To Do"))
        .unwrap_or_else(|| panic!("missing table header: {lines:?}"));
    let row = &lines[row_at];

    let name_at = column_of(row, "Demo Board");
    let unseen_at = row.chars().position(|ch| ch == '●').expect("unreviewed");
    let running_at = row.chars().position(|ch| ch == '▶').expect("running");
    assert!(
        unseen_at < name_at && name_at < column_of(header, "To Do") && running_at > name_at,
        "unreviewed work marks the row and running agents sit in their own column, got: {row}"
    );
    assert_eq!(cell_under(header, row, "Review"), '1');
    assert_eq!(
        cell_under(header, row, "Agents"),
        '1',
        "▶1 is right-aligned"
    );
    assert!(
        lines[row_at + 1].contains("/tmp/k4ai-status-run"),
        "the folder sits on the row's second line, got: {:?}",
        lines[row_at + 1]
    );
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn scan_counts_includes_open_questions_from_any_column() {
    let root = tempfile::tempdir().expect("root");
    write_task_file(root.path(), "todo", "TASK-001");
    let in_progress = root.path().join(".kanban/tasks/in_progress");
    std::fs::create_dir_all(&in_progress).expect("in_progress dir");
    std::fs::write(
        in_progress.join("TASK-002.md"),
        "---\nid: TASK-002\ntitle: TASK-002\nstatus: in_progress\nhas_questions: true\n---\n",
    )
    .expect("questioned task");
    let review = root.path().join(".kanban/tasks/review");
    std::fs::create_dir_all(&review).expect("review dir");
    std::fs::write(
        review.join("TASK-003.md"),
        "---\nid: TASK-003\ntitle: TASK-003\nstatus: review\nreview_unseen: true\n---\n",
    )
    .expect("unseen review");

    let counts = super::projects::scan_counts(root.path());
    assert_eq!(counts.todo, 1);
    assert_eq!(counts.in_progress, 1);
    assert_eq!(counts.review, 1);
    assert_eq!(counts.questions, 1);
    assert_eq!(counts.review_unseen, 1);
}

#[test]
fn project_row_shows_yellow_question_mark_when_any_task_has_a_question() {
    let work = std::path::PathBuf::from("/tmp/k4ai-status-ask");
    let cwd = std::path::PathBuf::from("/tmp/k4ai-status-ask-cwd");
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&work).expect("work dir");
    let store_dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(store_dir.path());
    let added = store.add(&work, Some("Demo Board")).expect("add project");
    write_task_file(&added.project.data_root, "todo", "TASK-001");
    let in_progress = added.project.data_root.join(".kanban/tasks/in_progress");
    std::fs::create_dir_all(&in_progress).expect("in_progress dir");
    std::fs::write(
        in_progress.join("TASK-002.md"),
        "---\nid: TASK-002\ntitle: TASK-002\nstatus: in_progress\nhas_questions: true\n---\n",
    )
    .expect("questioned task");
    let mut app = App::projects_at(store, None, Some(cwd.clone())).expect("projects app");
    let rendered = render_at(&mut app, 96, 12);
    let lines = rendered
        .split("\n\n--- style runs ---")
        .next()
        .expect("frame text")
        .lines()
        .collect::<Vec<_>>();
    let row_y = lines
        .iter()
        .position(|line| line.contains("Demo Board"))
        .unwrap_or_else(|| panic!("missing project row: {rendered}"));
    let line = lines[row_y];
    let question_at = line
        .chars()
        .position(|ch| ch == '?')
        .expect("question mark");
    let name_col = line
        .find("Demo Board")
        .map(|index| line[..index].chars().count())
        .expect("project name");
    assert!(
        question_at < name_col,
        "question mark should sit before the project name, got: {line}"
    );
    let style = style_at(&mut app, 96, 12, question_at as u16, row_y as u16);
    assert_eq!(
        style.fg,
        Some(Theme::named("dark").warn),
        "question mark should be yellow, got: {style:?}"
    );
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn projects_enter_on_create_cwd_registers_without_a_dialog() {
    let work = tempfile::tempdir().expect("work");
    let cwd = tempfile::tempdir().expect("cwd");
    let (_store, mut app) = projects_app(work.path(), Some(cwd.path().to_path_buf()));
    app.project_selected = 0;
    app.handle_key(key(KeyCode::Enter)).expect("create cwd");
    match app.take_loop_outcome() {
        Some(LoopOutcome::OpenProject(project)) => {
            assert_eq!(project.work_path, cwd.path());
        }
        other => panic!("expected OpenProject, got {other:?}"),
    }
}

#[test]
fn projects_enter_opens_the_selected_project() {
    let work = tempfile::tempdir().expect("work");
    let (_store, mut app) = projects_app(work.path(), None);
    app.handle_key(key(KeyCode::Enter)).expect("open");
    match app.take_loop_outcome() {
        Some(LoopOutcome::OpenProject(project)) => {
            assert_eq!(project.name, "Demo Board");
        }
        other => panic!("expected OpenProject, got {other:?}"),
    }
}

#[test]
fn projects_q_quits_even_when_a_board_lies_behind_the_list() {
    let work = tempfile::tempdir().expect("work");
    let (_store, mut app) = projects_app_with_return(work.path());
    app.handle_key(key(KeyCode::Char('q'))).expect("q");
    assert!(app.should_quit, "q must quit the TUI, not reopen the board");
    assert!(matches!(app.take_loop_outcome(), Some(LoopOutcome::Quit)));
}

#[test]
fn projects_escape_returns_to_the_board_behind_the_list() {
    let work = tempfile::tempdir().expect("work");
    let (_store, mut app) = projects_app_with_return(work.path());
    app.handle_key(key(KeyCode::Esc)).expect("Esc");
    assert!(!app.should_quit);
    match app.take_loop_outcome() {
        Some(LoopOutcome::OpenProject(project)) => {
            assert_eq!(project.name, "Demo Board");
        }
        other => panic!("expected OpenProject, got {other:?}"),
    }
}

#[test]
fn projects_q_quits_when_the_list_is_the_entry_screen() {
    let work = tempfile::tempdir().expect("work");
    let (_store, mut app) = projects_app(work.path(), None);
    app.handle_key(key(KeyCode::Char('q'))).expect("q");
    assert!(app.should_quit);
    assert!(matches!(app.take_loop_outcome(), Some(LoopOutcome::Quit)));
}

#[test]
fn board_uppercase_p_switches_to_the_projects_list() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('P'))).expect("P");
    match app.take_loop_outcome() {
        Some(LoopOutcome::ShowProjects { return_to }) => assert!(return_to.is_none()),
        other => panic!("expected ShowProjects, got {other:?}"),
    }
}

#[test]
fn board_russian_uppercase_ze_switches_to_the_projects_list() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('З'))).expect("RU P");
    match app.take_loop_outcome() {
        Some(LoopOutcome::ShowProjects { return_to }) => assert!(return_to.is_none()),
        other => panic!("expected ShowProjects, got {other:?}"),
    }
}

#[test]
fn board_escape_stays_on_board_when_escape_to_projects_is_off() {
    let (_dir, mut app) = app_with_board();
    assert!(!app.settings.escape_to_projects);
    app.handle_key(key(KeyCode::Esc)).expect("Esc");
    assert_eq!(app.screen, Screen::Board);
    assert!(app.take_loop_outcome().is_none());
}

#[test]
fn board_escape_opens_projects_when_setting_is_on() {
    let (_dir, mut app) = app_with_board();
    app.settings.escape_to_projects = true;
    app.handle_key(key(KeyCode::Esc)).expect("Esc");
    match app.take_loop_outcome() {
        Some(LoopOutcome::ShowProjects { return_to }) => assert!(return_to.is_none()),
        other => panic!("expected ShowProjects, got {other:?}"),
    }
}

#[test]
fn board_escape_clears_search_before_opening_projects() {
    let (_dir, mut app) = app_with_board();
    app.settings.escape_to_projects = true;
    app.handle_key(key(KeyCode::Char('/'))).expect("search");
    app.handle_key(key(KeyCode::Char('x'))).expect("query");
    assert!(!app.search.text().is_empty());
    app.handle_key(key(KeyCode::Esc)).expect("clear search");
    assert!(app.search.text().is_empty());
    assert_eq!(app.screen, Screen::Board);
    assert!(app.take_loop_outcome().is_none());
}

#[test]
fn project_settings_dialog_no_longer_carries_the_escape_toggle() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let modal = app.modal.as_ref().expect("settings modal");
    assert_eq!(modal.modal, Modal::Settings);
    assert!(
        !modal.fields().contains(&DialogField::EscapeToProjects),
        "the toggle moved to the global settings dialog: {:?}",
        modal.fields()
    );
}

#[test]
fn projects_screen_global_settings_toggle_persists_to_the_store() {
    let work = std::path::PathBuf::from("/tmp/k4ai-glob-settings");
    let _ = std::fs::remove_dir_all(&work);
    let (store_dir, mut app) = projects_app(&work, None);
    assert!(!app.settings.escape_to_projects);

    app.handle_key(key(KeyCode::Char('s')))
        .expect("open global settings");
    let modal = app.modal.as_ref().expect("global settings modal");
    assert_eq!(modal.modal, Modal::GlobalSettings);
    assert_eq!(modal.active_field(), DialogField::EscapeToProjects);
    assert!(!modal.escape_to_projects);

    app.handle_key(key(KeyCode::Char(' '))).expect("toggle");
    assert!(
        app.modal
            .as_ref()
            .expect("global settings modal")
            .escape_to_projects
    );
    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("Global settings"), "{rendered}");
    assert!(
        rendered.contains("Esc from board opens projects"),
        "{rendered}"
    );
    assert!(rendered.contains("☑"), "{rendered}");
    app.handle_key(key(KeyCode::Tab)).expect("focus sort");
    app.handle_key(key(KeyCode::Tab)).expect("save");
    app.handle_key(key(KeyCode::Enter)).expect("save settings");

    assert!(app.modal.is_none());
    assert!(app.settings.escape_to_projects);
    assert_eq!(app.status, "Global settings saved");
    let store = ProjectStore::at(store_dir.path());
    let config = store.load_global_config().expect("saved global config");
    assert!(config.escape_to_projects());

    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn projects_screen_reflects_the_saved_global_escape_setting() {
    let work = std::path::PathBuf::from("/tmp/k4ai-glob-preload");
    let _ = std::fs::remove_dir_all(&work);
    let store_dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(store_dir.path());
    store
        .save_global_config(&{
            let mut config = crate::core::global::GlobalConfig::default();
            config.set_escape_to_projects(true);
            config
        })
        .expect("seed global config");
    std::fs::create_dir_all(&work).expect("work dir");
    let added = store.add(&work, Some("Demo Board")).expect("add project");
    write_task_file(&added.project.data_root, "todo", "TASK-001");

    let app = App::projects_at(store, None, None).expect("projects app");
    assert!(app.settings.escape_to_projects);

    let _ = std::fs::remove_dir_all(&work);
}

/// Registry names of the visible project rows, in list order.
fn visible_project_names(app: &App) -> Vec<String> {
    app.visible_project_items()
        .iter()
        .filter_map(|item| match item {
            super::projects::ProjectListItem::Project(row) => Some(row.project.name.clone()),
            _ => None,
        })
        .collect()
}

/// Alpha (oldest, quiet), Beta (newest, one running agent), Gamma (middle
/// age, one unseen Review task): the three orderings each pick a different
/// winner.
fn sorted_projects_store() -> (tempfile::TempDir, ProjectStore) {
    let store_dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(store_dir.path());
    let alpha_work = tempfile::tempdir().expect("alpha work");
    let beta_work = tempfile::tempdir().expect("beta work");
    let gamma_work = tempfile::tempdir().expect("gamma work");
    let alpha = store
        .add(alpha_work.path(), Some("Alpha"))
        .expect("add alpha");
    let beta = store.add(beta_work.path(), Some("Beta")).expect("add beta");
    let gamma = store
        .add(gamma_work.path(), Some("Gamma"))
        .expect("add gamma");
    set_project_created_at(&alpha.project.data_root, "2026-01-01T10:00:00");
    set_project_created_at(&beta.project.data_root, "2026-03-01T10:00:00");
    set_project_created_at(&gamma.project.data_root, "2026-02-01T10:00:00");
    write_active_session(&beta.project.data_root, "ses-pi-sort-live");
    write_task_file_with_flags(
        &gamma.project.data_root,
        "review",
        "TASK-001",
        "review_unseen: true\n",
    );
    (store_dir, store)
}

#[test]
fn projects_screen_orders_rows_by_the_sort_setting() {
    let (_store_dir, store) = sorted_projects_store();
    let mut app = App::projects_at(store, None, None).expect("projects app");

    // Default: alphabetical.
    assert_eq!(app.settings.project_sort, "name");
    assert_eq!(visible_project_names(&app), ["Alpha", "Beta", "Gamma"]);

    // Newest first: Beta (Mar) before Gamma (Feb) before Alpha (Jan).
    app.settings.project_sort = "newest".to_string();
    assert_eq!(visible_project_names(&app), ["Beta", "Gamma", "Alpha"]);

    // Smart: unread Gamma first, running Beta second, quiet Alpha last.
    app.settings.project_sort = "smart".to_string();
    assert_eq!(visible_project_names(&app), ["Gamma", "Beta", "Alpha"]);

    // Unknown values fall back to the default order.
    app.settings.project_sort = "bogus".to_string();
    assert_eq!(visible_project_names(&app), ["Alpha", "Beta", "Gamma"]);
}

#[test]
fn projects_screen_sort_applies_to_the_filtered_list() {
    let store_dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(store_dir.path());
    let apple_work = tempfile::tempdir().expect("apple work");
    let birch_work = tempfile::tempdir().expect("birch work");
    let cedar_work = tempfile::tempdir().expect("cedar work");
    let apple = store
        .add(apple_work.path(), Some("AppleQXK"))
        .expect("add apple");
    let birch = store
        .add(birch_work.path(), Some("BirchQXK"))
        .expect("add birch");
    let cedar = store
        .add(cedar_work.path(), Some("Cedar"))
        .expect("add cedar");
    set_project_created_at(&apple.project.data_root, "2026-01-01T10:00:00");
    set_project_created_at(&birch.project.data_root, "2026-03-01T10:00:00");
    set_project_created_at(&cedar.project.data_root, "2026-02-01T10:00:00");
    let mut app = App::projects_at(store, None, None).expect("projects app");
    app.settings.project_sort = "newest".to_string();
    // Distinctive token so a tempfile path cannot accidentally match.
    app.search.query.insert_str("QXK");
    assert_eq!(visible_project_names(&app), ["BirchQXK", "AppleQXK"]);
    let _ = (store_dir, apple_work, birch_work, cedar_work);
}

#[test]
fn projects_screen_sort_keeps_create_cwd_pinned() {
    let (_store_dir, store) = sorted_projects_store();
    let cwd = tempfile::tempdir().expect("cwd");
    let mut app =
        App::projects_at(store, None, Some(cwd.path().to_path_buf())).expect("projects app");
    app.settings.project_sort = "newest".to_string();
    let items = app.visible_project_items();
    assert!(
        matches!(
            items.first(),
            Some(super::projects::ProjectListItem::CreateCwd { .. })
        ),
        "create-cwd stays above the sorted rows: {items:?}"
    );
    assert_eq!(visible_project_names(&app), ["Beta", "Gamma", "Alpha"]);
}

#[test]
fn projects_screen_smart_sort_prefers_unread_over_running() {
    let (_store_dir, store) = sorted_projects_store();
    // Beta (running, newest) also gets unseen review work: unread still wins.
    let beta_row = store
        .list()
        .expect("list")
        .into_iter()
        .find(|p| p.name == "Beta")
        .expect("beta");
    write_task_file_with_flags(
        &beta_row.data_root,
        "review",
        "TASK-009",
        "review_unseen: true\n",
    );
    let mut app = App::projects_at(store, None, None).expect("projects app");
    app.settings.project_sort = "smart".to_string();
    // Both unread; the tie-break is newest first, so Beta outranks Gamma.
    assert_eq!(visible_project_names(&app), ["Beta", "Gamma", "Alpha"]);
}

#[test]
fn projects_screen_smart_sort_counts_open_questions_as_unread() {
    let (_store_dir, store) = sorted_projects_store();
    let alpha_row = store
        .list()
        .expect("list")
        .into_iter()
        .find(|p| p.name == "Alpha")
        .expect("alpha");
    write_task_file_with_flags(
        &alpha_row.data_root,
        "todo",
        "TASK-007",
        "has_questions: true\n",
    );
    let mut app = App::projects_at(store, None, None).expect("projects app");
    app.settings.project_sort = "smart".to_string();
    // Alpha (questions) and Gamma (unseen review) share the unread tier;
    // newest unread wins, and both still outrank running Beta.
    assert_eq!(visible_project_names(&app), ["Gamma", "Alpha", "Beta"]);
}

#[test]
fn projects_screen_reflects_the_saved_global_project_sort() {
    let (_store_dir, store) = sorted_projects_store();
    store
        .save_global_config(&{
            let mut config = crate::core::global::GlobalConfig::default();
            config.set_project_sort("newest");
            config
        })
        .expect("seed global config");
    let app = App::projects_at(store, None, None).expect("projects app");
    assert_eq!(app.settings.project_sort, "newest");
    assert_eq!(visible_project_names(&app), ["Beta", "Gamma", "Alpha"]);
}

#[test]
fn projects_screen_global_settings_project_sort_persists_to_the_store() {
    let work = std::path::PathBuf::from("/tmp/k4ai-glob-sort");
    let _ = std::fs::remove_dir_all(&work);
    let (store_dir, mut app) = projects_app(&work, None);
    assert_eq!(app.settings.project_sort, "name");

    app.handle_key(key(KeyCode::Char('s')))
        .expect("open global settings");
    let modal = app.modal.as_ref().expect("global settings modal");
    assert_eq!(modal.active_field(), DialogField::EscapeToProjects);
    assert!(modal.fields().contains(&DialogField::ProjectSort));
    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("Project sorting"), "{rendered}");
    assert!(rendered.contains("By name"), "{rendered}");

    // Tab onto the selector, then pick the third option (Smart).
    app.handle_key(key(KeyCode::Tab)).expect("focus sort");
    app.handle_key(key(KeyCode::Down)).expect("newest");
    app.handle_key(key(KeyCode::Down)).expect("smart");
    assert_eq!(
        app.modal.as_ref().expect("modal").project_sort_text(),
        Some("smart".to_string())
    );
    app.handle_key(key(KeyCode::Tab)).expect("save");
    app.handle_key(key(KeyCode::Enter)).expect("save settings");

    assert!(app.modal.is_none());
    assert_eq!(app.settings.project_sort, "smart");
    assert_eq!(app.status, "Global settings saved");
    let store = ProjectStore::at(store_dir.path());
    assert_eq!(store.load_global_config().unwrap().project_sort(), "smart");

    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn board_app_ignores_stale_per_project_escape_to_projects() {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        "tui:\n  escape_to_projects: true\nnotifications:\n  enabled: false\nauto_launch:\n  enabled: false\n",
    )
    .expect("stale per-project key");

    let app = App::new(dir.path()).expect("create app");
    assert!(
        !app.settings.escape_to_projects,
        "the per-project key must be ignored after the move to store-wide settings"
    );
}

#[test]
fn projects_delete_dialog_unregisters_by_default() {
    let work = std::path::PathBuf::from("/tmp/k4ai-snap-work");
    let _ = std::fs::remove_dir_all(&work);
    let (_store, mut app) = projects_app(&work, None);
    app.project_selected = 0;
    app.handle_key(key(KeyCode::Char('d'))).expect("delete");
    let modal = app.modal.as_ref().expect("delete modal");
    assert!(matches!(modal.modal, Modal::DeleteProject { .. }));
    assert!(!modal.purge_data);
    let rendered = render_snapshot(&mut app);
    assert!(rendered.contains("Unregister"), "{rendered}");
    assert!(rendered.contains("also delete board data"), "{rendered}");
    insta::assert_snapshot!("projects_delete", rendered);
    let _ = std::fs::remove_dir_all(&work);
}

/// A limits snapshot pinned relative to `now` so the rendered countdowns are
/// stable regardless of when the test runs.
fn limits_fixture() -> std::sync::Arc<crate::core::limits::LimitsSnapshot> {
    use crate::core::limits::{LimitWindow, LimitsSnapshot, ProviderLimits, ProviderState};

    let now = chrono::Utc::now().timestamp();
    let window = |label: &str, remaining: f64, resets_in: i64| LimitWindow {
        label: label.to_string(),
        remaining_percent: remaining,
        resets_at: Some(now + resets_in),
    };
    std::sync::Arc::new(LimitsSnapshot {
        fetched_at: now,
        providers: vec![
            ProviderLimits {
                provider: "claude".to_string(),
                state: ProviderState::Ready,
                windows: vec![
                    window("5h", 66.0, 3 * 3600 + 1830),
                    window("7d", 95.0, 6 * 86_400 + 11 * 3600 + 30),
                ],
                observed_at: None,
            },
            ProviderLimits {
                provider: "codex".to_string(),
                state: ProviderState::Ready,
                windows: vec![window("mon", 75.0, 18 * 86_400 + 3600)],
                observed_at: Some(now - 7 * 86_400 - 3600),
            },
            ProviderLimits {
                provider: "grok".to_string(),
                state: ProviderState::SignedOut,
                windows: Vec::new(),
                observed_at: None,
            },
        ],
    })
}

fn rendered_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
    render_at(app, width, height)
        .split("\n\n--- style runs ---")
        .next()
        .expect("frame text")
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn limits_row_sits_above_the_status_bar_and_lists_every_provider() {
    let (_dir, mut app) = populated_app();
    app.limits = Some(limits_fixture());

    let lines = rendered_lines(&mut app, 120, 28);
    let row = &lines[lines.len() - 2];
    let status = &lines[lines.len() - 1];

    assert!(
        row.contains("✳ claude 5h 66% ↻3h30m · 7d 95% ↻6d11h"),
        "{row}"
    );
    assert!(row.contains("✺ codex mon 75% ↻18d (7d old)"), "{row}");
    assert!(row.contains("✕ grok signed out"), "{row}");
    // The status bar keeps the last line, and the board keeps its columns.
    assert!(status.contains("n new"), "{status}");
    assert!(lines[0].contains("To Do"), "{}", lines[0]);
    assert!(
        lines.iter().any(|line| line.contains("Question card")),
        "{lines:?}"
    );
}

#[test]
fn limits_row_drops_reset_times_then_names_as_the_terminal_narrows() {
    let (_dir, mut app) = populated_app();
    app.limits = Some(limits_fixture());

    let medium = rendered_lines(&mut app, 70, 20);
    let medium_row = medium[medium.len() - 2].clone();
    let narrow = rendered_lines(&mut app, 30, 20);
    let narrow_row = narrow[narrow.len() - 2].clone();

    assert!(
        medium_row.contains("claude 5h 66% · 7d 95%"),
        "{medium_row}"
    );
    assert!(!medium_row.contains('↻'), "{medium_row}");
    // Too narrow even for names: icons and percentages only, and the providers
    // that no longer fit are dropped from the right.
    assert!(narrow_row.contains("✳ 66% · 95%"), "{narrow_row}");
    assert!(!narrow_row.contains("claude"), "{narrow_row}");
    assert!(!narrow_row.contains('✕'), "{narrow_row}");
}

/// A window whose reset time has passed holds a percentage for a period that
/// is over — the row drops it instead of freezing yesterday's number, and says
/// so when nothing current is left.
#[test]
fn limits_row_drops_windows_that_have_already_reset() {
    use crate::core::limits::{LimitWindow, LimitsSnapshot, ProviderLimits, ProviderState};

    let (_dir, mut app) = populated_app();
    let now = chrono::Utc::now().timestamp();
    let window = |label: &str, remaining: f64, resets_at: i64| LimitWindow {
        label: label.to_string(),
        remaining_percent: remaining,
        resets_at: Some(resets_at),
    };
    app.limits = Some(std::sync::Arc::new(LimitsSnapshot {
        fetched_at: now,
        providers: vec![
            ProviderLimits {
                provider: "claude".to_string(),
                state: ProviderState::Ready,
                windows: vec![
                    window("5h", 1.0, now - 3_600),
                    window("7d", 95.0, now + 6 * 86_400),
                ],
                observed_at: None,
            },
            ProviderLimits {
                provider: "codex".to_string(),
                state: ProviderState::Ready,
                windows: vec![window("5h", 40.0, now - 60)],
                observed_at: Some(now - 86_400),
            },
        ],
    }));

    let lines = rendered_lines(&mut app, 120, 28);
    let row = &lines[lines.len() - 2];

    assert!(row.contains("✳ claude 7d 95% ↻6d"), "{row}");
    assert!(!row.contains("1%"), "{row}");
    assert!(row.contains("✺ codex stale"), "{row}");
}

#[test]
fn limits_row_is_absent_without_a_snapshot_when_disabled_and_off_screen() {
    let (_dir, mut app) = populated_app();

    let bare = rendered_lines(&mut app, 120, 28);
    assert!(!bare[bare.len() - 2].contains("claude"), "{bare:?}");
    assert!(bare[bare.len() - 1].contains("n new"), "{bare:?}");

    app.limits = Some(limits_fixture());
    app.settings.show_limits = false;
    let disabled = rendered_lines(&mut app, 120, 28);
    assert!(
        !disabled[disabled.len() - 2].contains("claude"),
        "{disabled:?}"
    );

    app.settings.show_limits = true;
    app.screen = Screen::Sessions;
    let sessions = rendered_lines(&mut app, 120, 28);
    assert!(
        !sessions[sessions.len() - 2].contains("claude"),
        "{sessions:?}"
    );
}

#[test]
fn limits_row_renders_on_the_projects_screen() {
    let work = tempfile::tempdir().expect("work");
    let (_store, mut app) = projects_app(work.path(), None);
    app.limits = Some(limits_fixture());

    let lines = rendered_lines(&mut app, 120, 24);
    let row = &lines[lines.len() - 2];

    assert_eq!(app.screen, Screen::Projects);
    assert!(row.contains("✳ claude 5h 66%"), "{row}");
    assert!(lines[lines.len() - 1].contains("Enter open"), "{lines:?}");
}

#[test]
fn limits_row_registers_refresh_hitboxes_on_claude_codex_and_grok() {
    let (_dir, mut app) = populated_app();
    app.limits = Some(limits_fixture());

    let lines = rendered_lines(&mut app, 120, 28);
    let row_index = lines.len() - 2;
    let row_y = row_index as u16;
    let row_text = &lines[row_index];

    let refresh_hit = |provider: &'static str| {
        app.hitboxes.iter().find(|hitbox| {
            hitbox.area.y == row_y
                && hitbox.action == HitAction::Action(UiAction::RefreshLimits(provider))
        })
    };
    let claude_hit = refresh_hit("claude").expect("claude hitbox");
    let codex_hit = refresh_hit("codex").expect("codex hitbox");
    let grok_hit = refresh_hit("grok").expect("grok hitbox");
    assert!(
        refresh_hit("zai").is_none() && refresh_hit("synthetic").is_none(),
        "zai and synthetic stay display-only"
    );

    // Each hitbox covers its provider's own text on the rendered row.
    let covers = |hitbox: &super::app::Hitbox, text: &str| {
        let byte = row_text.find(text).expect("provider text");
        let column = unicode_width::UnicodeWidthStr::width(&row_text[..byte]) as u16;
        hitbox.area.x <= column && column < hitbox.area.x + hitbox.area.width
    };
    assert!(covers(claude_hit, "claude"), "{claude_hit:?} vs {row_text}");
    assert!(covers(codex_hit, "codex"), "{codex_hit:?} vs {row_text}");
    assert!(covers(grok_hit, "grok"), "{grok_hit:?} vs {row_text}");
}

fn click_limits_segment(app: &mut App, provider: &'static str) {
    let hit = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::Action(UiAction::RefreshLimits(provider)))
        .copied()
        .unwrap_or_else(|| panic!("{provider} hitbox"));
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.area.x,
        row: hit.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("click");
}

#[test]
fn clicking_codex_limits_segment_reports_a_refresh_in_the_status() {
    let (_dir, mut app) = populated_app();
    app.limits = Some(limits_fixture());
    let _ = render_at(&mut app, 120, 28);

    click_limits_segment(&mut app, "codex");

    assert!(
        app.status.contains("Refreshing codex limits"),
        "{}",
        app.status
    );
}

#[test]
fn clicking_claude_limits_segment_reports_a_refresh_in_the_status() {
    let (_dir, mut app) = populated_app();
    app.limits = Some(limits_fixture());
    let _ = render_at(&mut app, 120, 28);

    click_limits_segment(&mut app, "claude");

    assert!(
        app.status.contains("Refreshing claude limits"),
        "{}",
        app.status
    );
}

#[test]
fn limits_refresh_status_returns_to_ready_after_update() {
    crate::core::limits::force_provider_refresh_in_flight(false);
    let (_dir, mut app) = populated_app();
    app.limits = Some(limits_fixture());
    let _ = render_at(&mut app, 120, 28);

    crate::core::limits::force_provider_refresh_in_flight(true);
    click_limits_segment(&mut app, "grok");
    assert_eq!(app.status, "Refreshing grok limits…");

    app.tick().expect("tick while refreshing");
    assert_eq!(app.status, "Refreshing grok limits…");

    crate::core::limits::force_provider_refresh_in_flight(false);
    app.tick().expect("tick after refresh");
    assert_eq!(app.status, "grok limits updated");

    app.expire_limits_status_at(Instant::now() + Duration::from_secs(4));
    assert_eq!(app.status, "TUI ready");

    click_limits_segment(&mut app, "codex");
    assert!(
        app.status.contains("Refreshing codex limits"),
        "{}",
        app.status
    );
    app.tick().expect("tick when already complete");
    assert_eq!(app.status, "codex limits updated");
}

#[test]
fn review_editor_focus_status_returns_to_ready() {
    let (_dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Edit review")).unwrap();
    app.ops.set_review_edits(&task.id, "abcdef").unwrap();
    app.ops.move_task(&task.id, "review", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).unwrap();
    app.handle_key(key(KeyCode::Tab)).unwrap();

    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Edits);
    assert_eq!(app.status, "Review editor focused");

    app.tick().expect("arm notice timer");
    assert_eq!(app.status, "Review editor focused");

    app.expire_transient_status_at(Instant::now() + Duration::from_secs(4));
    assert_eq!(app.status, "TUI ready");
    assert_eq!(app.detail.as_ref().unwrap().focus, DetailFocus::Edits);
}

#[test]
fn action_status_returns_to_ready_after_notice_window() {
    let (_dir, mut app) = app_with_board();
    app.status = "Created TASK-001".to_string();
    app.tick().expect("arm notice timer");
    assert_eq!(app.status, "Created TASK-001");

    app.expire_transient_status_at(Instant::now() + Duration::from_secs(4));
    assert_eq!(app.status, "TUI ready");
}

#[test]
fn projects_idle_status_does_not_expire() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let store = ProjectStore::at(store_dir.path());
    let mut app = App::projects_at(store, None, None).expect("projects app");
    assert_eq!(app.status, "Projects");
    app.expire_transient_status_at(Instant::now() + Duration::from_secs(4));
    assert_eq!(app.status, "Projects");
}

#[test]
fn limits_progress_status_does_not_expire_while_refreshing() {
    crate::core::limits::force_provider_refresh_in_flight(true);
    let (_dir, mut app) = populated_app();
    app.limits = Some(limits_fixture());
    let _ = render_at(&mut app, 120, 28);
    click_limits_segment(&mut app, "grok");
    assert_eq!(app.status, "Refreshing grok limits…");

    app.expire_transient_status_at(Instant::now() + Duration::from_secs(4));
    assert_eq!(app.status, "Refreshing grok limits…");
    crate::core::limits::force_provider_refresh_in_flight(false);
}
