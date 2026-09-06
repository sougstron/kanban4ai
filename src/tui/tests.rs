use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::{Modifier, Style};
use ratatui_textarea::{TextArea, WrapMode};

use crate::core::models::{IntegrationState, RunPhase, TaskStatus};
use crate::core::operations::Operations;
use crate::core::project::ProjectStore;
use crate::core::session::SessionManager;
use crate::core::storage::{NewTask, Storage};
use crate::core::thread::ThreadManager;

use super::app::{
    App, DetailFocus, HitAction, Screen, UiAction, load_log_tail, normalize_command_key,
};
use super::board;
use super::dialogs::{
    AgentSlot, DialogField, Modal, ModalButton, ModalState, SelectOption, SettingsTab,
};
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
    // Tall enough that the description can reach its cap even after the
    // filterable Model and Chain-to selectors claim their filter rows.
    let _ = render_at(&mut app, 120, 80);

    let description = modal_hitbox(&app, HitAction::ModalField(DialogField::Description));
    assert_eq!(description.height, 15);
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
    assert!((5..=15).contains(&description.height));
    let save = modal_hitbox(&app, HitAction::ModalButton(ModalButton::Save));
    let cancel = modal_hitbox(&app, HitAction::ModalButton(ModalButton::Cancel));
    assert!(!overlaps(description, save));
    assert!(!overlaps(description, cancel));
    let _ = render_at(&mut app, 24, 8);
}

/// The chain selector stays compact (filter + "No chain" minimum, hard cap
/// of 8) while the description takes the spare rows up to 15.
#[test]
fn chain_selector_height_clamps_and_description_grows_taller() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    let _ = render_at(&mut app, 120, 80);
    let chain = modal_hitbox(&app, HitAction::ModalField(DialogField::ChainTo));
    assert_eq!(
        chain.height, 4,
        "only the filter and the No chain entry remain"
    );
    drop(app);

    let (_dir, mut app) = plain_tasks_app(6);
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    let _ = render_at(&mut app, 120, 80);
    let chain = modal_hitbox(&app, HitAction::ModalField(DialogField::ChainTo));
    assert_eq!(chain.height, 8, "many chain candidates must cap at 8 rows");
    let description = modal_hitbox(&app, HitAction::ModalField(DialogField::Description));
    assert_eq!(
        description.height, 15,
        "spare rows must grow the description to its cap"
    );
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

/// A board of `count` plain todo cards in one column, for selection tests
/// that need several unremarkable cards (no questions, no badges).
fn plain_tasks_app(count: usize) -> (tempfile::TempDir, App) {
    let (dir, mut app) = app_with_board();
    let ops = Operations::new(dir.path());
    for n in 1..=count {
        ops.create_task(NewTask {
            title: format!("Plain task {n}"),
            ..Default::default()
        })
        .expect("create plain task");
    }
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    (dir, app)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn alt_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
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
    if !timestamp.is_match(&line) {
        return line;
    }
    let width = line.chars().count();
    let replaced = timestamp.replace_all(&line, "<timestamp>").into_owned();
    let last = replaced.chars().last().unwrap_or(' ');
    if matches!(last, '│' | '┤' | '┐' | '┘' | '█') {
        let border_len = last.len_utf8();
        let without = replaced[..replaced.len() - border_len].trim_end();
        let pad = width.saturating_sub(without.chars().count() + 1);
        format!("{without}{}{last}", " ".repeat(pad))
    } else {
        let pad = width.saturating_sub(replaced.chars().count());
        format!("{replaced}{}", " ".repeat(pad))
    }
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

fn review_card_border(app: &mut App) -> Style {
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload board");
    let _ = render_at(app, 96, 28);
    let (_, _, area) = card_hits(app)
        .into_iter()
        .find(|(column, _, _)| *column == 2)
        .expect("review card");
    style_at(app, 96, 28, area.x, area.y)
}

fn agent_review_task(app: &App, title: &str) -> String {
    let task = app.ops.create_task(NewTask::titled(title)).unwrap();
    app.ops.move_task(&task.id, "review", true).unwrap();
    assert!(
        app.ops.get_task(&task.id).unwrap().unwrap().review_unseen,
        "agent completion marks the card unseen"
    );
    task.id
}

#[test]
fn unseen_review_cards_use_the_yellow_notifier_border() {
    let (_dir, mut app) = app_with_board();
    agent_review_task(&app, "Agent finished this");
    app.focused_column = 0;
    app.focused_card = 0;
    let style = review_card_border(&mut app);
    assert_eq!(
        style.fg,
        Some(app.theme.warn),
        "an unread Review card should use the yellow notifier border"
    );
}

#[test]
fn seen_review_cards_drop_the_yellow_notifier_border() {
    let (_dir, mut app) = app_with_board();
    let task_id = agent_review_task(&app, "Agent finished this");
    app.ops.mark_review_seen(&task_id).unwrap();
    assert!(
        !app.ops.get_task(&task_id).unwrap().unwrap().review_unseen,
        "opening detail clears the unseen marker"
    );
    app.focused_column = 0;
    app.focused_card = 0;
    let style = review_card_border(&mut app);
    assert_eq!(
        style.fg,
        Some(app.theme.border),
        "a read Review card should drop the yellow notifier"
    );
}

#[test]
fn focused_unseen_review_card_stays_yellow() {
    let (_dir, mut app) = app_with_board();
    agent_review_task(&app, "Still unread");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload board");
    app.focused_column = 2;
    app.focused_card = 0;
    let style = review_card_border(&mut app);
    assert_eq!(
        style.fg,
        Some(app.theme.warn),
        "keyboard/hover focus must not hide the unread Review notifier"
    );
}

#[test]
fn opening_review_detail_clears_the_unseen_notifier() {
    let (_dir, mut app) = app_with_board();
    let task_id = agent_review_task(&app, "Open me");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload board");
    app.focused_column = 2;
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    assert_eq!(app.screen, Screen::Detail);
    assert!(
        !app.ops.get_task(&task_id).unwrap().unwrap().review_unseen,
        "opening detail is the human-read signal"
    );
    app.handle_key(key(KeyCode::Char('q')))
        .expect("close detail");
    app.focused_column = 0;
    app.focused_card = 0;
    let style = review_card_border(&mut app);
    assert_eq!(
        style.fg,
        Some(app.theme.border),
        "after the human reads the card, the yellow notifier goes out"
    );
}

/// The badge answers "which board am I looking at", and the guessing happens
/// while reading tasks, so it has to hold on every screen a task can be read
/// from — not just the board.
#[test]
fn project_badge_names_the_open_project_on_every_board_screen() {
    let (_dir, mut app) = populated_app();
    app.settings.project_name = "Kanban".to_string();

    let board = rendered_lines(&mut app, 160, 28);
    assert!(board[0].contains("▸ Kanban"), "{}", board[0]);

    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    assert_eq!(app.screen, Screen::Detail);
    let detail = rendered_lines(&mut app, 160, 28);
    assert!(detail[0].contains("▸ Kanban"), "{}", detail[0]);

    for screen in [Screen::Sessions, Screen::Archive] {
        app.screen = screen;
        let lines = rendered_lines(&mut app, 160, 28);
        assert!(lines[0].contains("▸ Kanban"), "{screen:?}: {}", lines[0]);
    }
}

/// The badge shares its row with the last block's own title, so it gives way
/// rather than colliding: full name, then truncated, then gone.
#[test]
fn project_badge_degrades_instead_of_colliding_with_a_column_title() {
    let (_dir, mut app) = populated_app();
    app.settings.project_name = "Long Project Name".to_string();

    let wide = rendered_lines(&mut app, 160, 28);
    assert!(wide[0].contains("▸ Long Project Name"), "{}", wide[0]);

    let medium = rendered_lines(&mut app, 96, 28);
    assert!(medium[0].contains("▸ Long P…"), "{}", medium[0]);
    assert!(medium[0].contains("Done (0)"), "{}", medium[0]);

    // Too little room left for a readable label: the frame stays clean.
    let narrow = rendered_lines(&mut app, 60, 28);
    assert!(!narrow[0].contains('▸'), "{}", narrow[0]);
    assert!(narrow[0].contains("Done"), "{}", narrow[0]);
}

/// The badge sits inside the last column's area, so it has to be hit-tested
/// ahead of that column or a click on it would only move focus.
#[test]
fn project_badge_click_target_wins_over_the_column_underneath() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);

    let badge = &app.hitboxes[0];
    assert_eq!(badge.action, HitAction::Action(UiAction::OpenProjects));
    let last = app.board.columns.len() - 1;
    let column = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::ColumnFocus(last))
        .expect("column hitbox");
    assert!(overlaps(badge.area, column.area));
}

/// The projects list has no open project; its placeholder name must not be
/// rendered as if it were one.
#[test]
fn projects_list_carries_no_project_badge() {
    let work = tempfile::tempdir().expect("work");
    let (_store, mut app) = projects_app(work.path(), None);

    let lines = rendered_lines(&mut app, 120, 24);
    assert_eq!(app.screen, Screen::Projects);
    assert!(!lines[0].contains('▸'), "{}", lines[0]);
}

/// The title is written inside an OSC escape and read in a tab bar, so it is
/// one line of printable text or nothing.
#[test]
fn window_title_names_the_project_without_leaking_control_characters() {
    let (_dir, mut app) = app_with_board();
    app.settings.project_name = "Kanban".to_string();
    assert_eq!(app.window_title(), "Kanban — kanban4ai");

    app.settings.project_name = "bad\u{7}name\nsecond".to_string();
    let title = app.window_title();
    assert!(!title.chars().any(char::is_control), "{title}");
    assert_eq!(title, "bad name second — kanban4ai");

    app.settings.project_name = "   ".to_string();
    assert_eq!(app.window_title(), "kanban4ai");

    app.settings.project_name = "N".repeat(200);
    let long = app.window_title();
    assert!(long.starts_with(&format!("{}…", "N".repeat(63))), "{long}");

    let work = tempfile::tempdir().expect("work");
    let (_store, projects) = projects_app(work.path(), None);
    assert!(!projects.has_board());
    assert_eq!(projects.window_title(), "kanban4ai");
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
    app.handle_key(key(KeyCode::Tab)).expect("agent settings");
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
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
    app.handle_key(ctrl_key(KeyCode::Char('s')))
        .expect("stage popup");
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
fn task_parent_form_opens_nested_agent_settings_without_interactive_field() {
    // Given a new-task parent form.
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(
        modal.fields(),
        [
            DialogField::Title,
            DialogField::Description,
            DialogField::AgentSettings,
            DialogField::ChainTo,
            DialogField::UseOrchestrator,
            DialogField::UseDesigner,
            DialogField::UseReviewer,
            DialogField::Confirm,
            DialogField::Cancel,
        ]
    );

    // When the launcher is activated.
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");

    // Then the primary picker owns focus while the parent remains open.
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.agent_popup_slot(), Some(AgentSlot::Primary));
    assert_eq!(modal.active_field(), DialogField::Backend);
}

#[test]
fn agent_settings_cancel_restores_values_selection_and_parent_view() {
    // Given a task form with its launcher focused at a known parent scroll.
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    {
        let modal = app.modal.as_mut().expect("modal");
        modal.focus_field(DialogField::AgentSettings);
        modal.form_scroll = 2;
    }
    let before = {
        let modal = app.modal.as_ref().expect("modal");
        (
            modal.backend_text(),
            modal.model_text(),
            modal.effort_text(),
            modal.agent_text(),
            modal.backend_selected,
            modal.model_selected,
            modal.effort_selected,
            modal.agent_selected,
        )
    };
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
    app.handle_key(key(KeyCode::Down)).expect("change backend");

    // When Esc cancels the popup.
    app.handle_key(key(KeyCode::Esc)).expect("cancel popup");

    // Then the exact picker state and parent viewport are restored.
    let modal = app.modal.as_ref().expect("parent remains open");
    assert_eq!(modal.agent_popup_slot(), None);
    assert_eq!(modal.active_field(), DialogField::AgentSettings);
    assert_eq!(modal.form_scroll, 2);
    assert_eq!(
        (
            modal.backend_text(),
            modal.model_text(),
            modal.effort_text(),
            modal.agent_text(),
            modal.backend_selected,
            modal.model_selected,
            modal.effort_selected,
            modal.agent_selected,
        ),
        before
    );
}

#[test]
fn agent_settings_save_stages_changes_and_returns_to_parent() {
    // Given an open primary-agent popup on a new task.
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
    app.handle_key(key(KeyCode::Down)).expect("choose backend");
    let staged_backend = app.modal.as_ref().expect("modal").backend_text();

    // When the popup is saved with Ctrl+S.
    app.handle_key(ctrl_key(KeyCode::Char('s')))
        .expect("save popup");

    // Then only the popup closes and its values remain staged in the parent.
    let modal = app.modal.as_ref().expect("parent remains open");
    assert_eq!(modal.agent_popup_slot(), None);
    assert_eq!(modal.active_field(), DialogField::AgentSettings);
    assert_eq!(modal.backend_text(), staged_backend);
    assert!(
        app.board
            .columns
            .iter()
            .flat_map(|column| column.tasks.iter())
            .all(|task| !task.title.is_empty())
    );
}

#[test]
fn settings_agent_cancel_restores_staged_values_after_backend_change() {
    // Given staged primary settings that differ from the persisted defaults.
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    app.modal
        .as_mut()
        .expect("settings")
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
    app.modal.as_mut().expect("popup").model = TextArea::new(vec!["staged/model".into()]);
    app.handle_key(ctrl_key(KeyCode::Char('s')))
        .expect("stage popup");
    app.handle_key(key(KeyCode::Enter)).expect("reopen popup");
    app.handle_key(key(KeyCode::Down)).expect("change backend");

    // When Esc cancels the second visit.
    app.handle_key(key(KeyCode::Esc)).expect("cancel popup");

    // Then the staged value from the opening snapshot survives.
    assert_eq!(
        app.modal
            .as_ref()
            .expect("settings")
            .model_text()
            .as_deref(),
        Some("staged/model")
    );
}

#[test]
fn agent_settings_save_clears_visit_filter() {
    // Given a popup with a backend filter typed during this visit.
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
    app.handle_key(key(KeyCode::Char('o'))).expect("filter");
    assert_eq!(app.modal.as_ref().expect("popup").backend_filter, "o");

    // When the popup is saved and opened again.
    app.handle_key(ctrl_key(KeyCode::Char('s')))
        .expect("save popup");
    app.handle_key(key(KeyCode::Enter)).expect("reopen popup");

    // Then the visit-local filter is gone.
    assert!(app.modal.as_ref().expect("popup").backend_filter.is_empty());
}

#[test]
fn role_agent_options_show_hover_feedback() {
    // Given the designer popup rendered with clickable options.
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    app.modal
        .as_mut()
        .expect("settings")
        .focus_field(DialogField::DesignerAgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
    let _ = render_at(&mut app, 120, 40);
    let option = modal_hitbox(
        &app,
        HitAction::ModalOption {
            field: DialogField::DesignerBackend,
            index: 0,
        },
    );

    // When the pointer moves over that option.
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: option.x,
        row: option.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover option");

    // Then the option receives the same bold focus treatment as primary options.
    let hovered = style_at(&mut app, 120, 40, option.x, option.y);
    assert!(hovered.add_modifier.contains(Modifier::BOLD));
    assert_eq!(hovered.fg, Some(app.theme.focus));
}

#[test]
fn agent_launcher_summary_sanitizes_terminal_controls() {
    // Given a stored backend value containing a terminal escape.
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal.as_mut().expect("modal").backend = TextArea::new(vec!["bad\u{001b}name".into()]);

    // When the parent form renders its launcher summary.
    let rendered = render_at(&mut app, 100, 32);

    // Then the escape is replaced before reaching the terminal buffer.
    assert!(!rendered.contains('\u{001b}'));
    assert!(rendered.contains("bad�name"));
}

#[test]
fn agent_popup_render_exposes_only_popup_hitboxes() {
    // Given a rendered parent form and then its nested popup.
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");

    // When the frame registers hitboxes.
    let rendered = render_at(&mut app, 100, 40);

    // Then popup controls are clickable and the dimmed parent is not.
    assert!(rendered.contains("Primary agent settings"), "{rendered}");
    insta::assert_snapshot!("agent_popup_primary", rendered);
    assert!(
        app.hitboxes
            .iter()
            .any(|hitbox| { hitbox.action == HitAction::ModalField(DialogField::Backend) })
    );
    assert!(
        !app.hitboxes
            .iter()
            .any(|hitbox| { hitbox.action == HitAction::ModalField(DialogField::AgentSettings) })
    );
}

#[test]
fn project_settings_has_separate_role_agent_launchers() {
    // Given project settings.
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");

    // Then every tab carries its own launcher and the flat role fields are
    // gone: the primary launcher lives on Common, the role launchers on
    // their own tabs.
    let modal = app.modal.as_ref().expect("settings");
    assert!(
        modal.fields().contains(&DialogField::AgentSettings),
        "missing AgentSettings on the Common tab"
    );
    for (tab, field) in [
        (SettingsTab::Designer, DialogField::DesignerAgentSettings),
        (SettingsTab::Reviewer, DialogField::ReviewerAgentSettings),
    ] {
        let modal = app.modal.as_mut().expect("settings");
        modal.set_settings_tab(tab);
        let modal = app.modal.as_ref().expect("settings");
        assert!(modal.fields().contains(&field), "missing {field:?}");
        assert!(
            !modal.fields().contains(&DialogField::DesignerBackend),
            "flat DesignerBackend must stay gone"
        );
        assert!(
            !modal.fields().contains(&DialogField::ReviewerBackend),
            "flat ReviewerBackend must stay gone"
        );
    }
}

/// Every settings tab shows exactly its own page; Save/Cancel render under
/// all four because they persist the whole dialog.
#[test]
fn settings_tabs_split_fields_and_keep_buttons_on_every_tab() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    for (tab, contains, excludes) in [
        (
            SettingsTab::Common,
            &[
                DialogField::Title,
                DialogField::AgentSettings,
                DialogField::IsolationStatus,
            ][..],
            &[DialogField::DesignerEnabled, DialogField::ReviewerEnabled][..],
        ),
        (
            SettingsTab::Designer,
            &[
                DialogField::DesignerEnabled,
                DialogField::DesignerAgentSettings,
            ][..],
            &[DialogField::Title, DialogField::ReviewerEnabled][..],
        ),
        (
            SettingsTab::Reviewer,
            &[
                DialogField::ReviewerEnabled,
                DialogField::ReviewerOnChanges,
                DialogField::ReviewerMaxRounds,
            ][..],
            &[DialogField::Title, DialogField::DesignerEnabled][..],
        ),
        (
            SettingsTab::Executor,
            &[
                DialogField::ExecutorMiddle1,
                DialogField::ExecutorCheap3,
                DialogField::ExecutorWeekThreshold,
                DialogField::ExecutorFiveHourThreshold,
            ][..],
            &[DialogField::Title, DialogField::ReviewerEnabled][..],
        ),
    ] {
        app.modal.as_mut().expect("settings").set_settings_tab(tab);
        let modal = app.modal.as_ref().expect("settings");
        let fields = modal.fields();
        for field in contains {
            assert!(fields.contains(field), "{tab:?} must show {field:?}");
        }
        for field in excludes {
            assert!(!fields.contains(field), "{tab:?} must hide {field:?}");
        }
        assert!(
            fields.contains(&DialogField::Confirm) && fields.contains(&DialogField::Cancel),
            "{tab:?} must offer Save/Cancel"
        );
    }
}

/// Left/Right walk the tab strip from plain fields (checkboxes, the
/// isolation row) but never steal the arrows from a text caret or a
/// filtered selector. Tab/BackTab keep cycling inside the active tab.
#[test]
fn settings_arrows_switch_tabs_unless_the_field_owns_them() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");

    // From Title (text caret): arrows stay in the field.
    app.handle_key(key(KeyCode::Right))
        .expect("caret right in title");
    assert_eq!(
        app.modal.as_ref().unwrap().settings_tab,
        SettingsTab::Common
    );

    // From the QueueEnabled checkbox: Right lands on Designer.
    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::QueueEnabled);
    app.handle_key(key(KeyCode::Right)).expect("next tab");
    assert_eq!(
        app.modal.as_ref().unwrap().settings_tab,
        SettingsTab::Designer
    );

    // From IsolationStatus (Common): Right walks Common→Designer.
    let modal = app.modal.as_mut().unwrap();
    modal.set_settings_tab(SettingsTab::Common);
    modal.focus_field(DialogField::IsolationStatus);
    app.handle_key(key(KeyCode::Right)).expect("next tab again");
    assert_eq!(
        app.modal.as_ref().unwrap().settings_tab,
        SettingsTab::Designer
    );

    // MaxRunningPerBackend is a textarea: its arrows must not switch tabs.
    let modal = app.modal.as_mut().unwrap();
    modal.set_settings_tab(SettingsTab::Common);
    modal.focus_field(DialogField::MaxRunningPerBackend);
    app.handle_key(key(KeyCode::Left)).expect("caret left");
    app.handle_key(key(KeyCode::Right)).expect("caret right");
    assert_eq!(
        app.modal.as_ref().unwrap().settings_tab,
        SettingsTab::Common
    );
}

/// Tab navigation wraps around both ends of the strip, while Tab/BackTab
/// never leave the visible page.
#[test]
fn settings_tab_arrows_wrap_and_tab_stays_in_page() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");

    // Left from the first tab wraps to the last one.
    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::QueueEnabled);
    app.handle_key(key(KeyCode::Left)).expect("wrap left");
    assert_eq!(
        app.modal.as_ref().unwrap().settings_tab,
        SettingsTab::Executor
    );
    // Right from the last tab wraps back to the first. The Executor page
    // opens on a slot selector that owns the arrows, so Confirm — which
    // surrendered them to the tab strip — is the vantage point here.
    let modal = app.modal.as_mut().unwrap();
    modal.focus_field(DialogField::Confirm);
    app.handle_key(key(KeyCode::Right)).expect("wrap right");
    assert_eq!(
        app.modal.as_ref().unwrap().settings_tab,
        SettingsTab::Common
    );

    // Tab walks the Executor page to its own Cancel, then wraps within it.
    let modal = app.modal.as_mut().unwrap();
    modal.set_settings_tab(SettingsTab::Executor);
    assert_eq!(modal.active_field(), DialogField::ExecutorMiddle1);
    for _ in 0..(modal.fields().len() - 1) {
        app.handle_key(key(KeyCode::Tab)).expect("next field");
    }
    assert_eq!(
        app.modal.as_ref().unwrap().active_field(),
        DialogField::Cancel
    );
    assert_eq!(
        app.modal.as_ref().unwrap().settings_tab,
        SettingsTab::Executor
    );
    app.handle_key(key(KeyCode::Tab)).expect("wrap in page");
    assert_eq!(
        app.modal.as_ref().unwrap().active_field(),
        DialogField::ExecutorMiddle1
    );
}

/// Tab labels are clickable, and a click on a field of the active tab still
/// focuses that field.
#[test]
fn settings_tab_click_switches_tabs_and_field_click_focuses() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let _ = render_at(&mut app, 120, 32);
    let tab_hit = modal_hitbox(&app, HitAction::ModalTab(SettingsTab::Executor));
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: tab_hit.x,
        row: tab_hit.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("click Executor tab");
    assert_eq!(
        app.modal.as_ref().unwrap().settings_tab,
        SettingsTab::Executor
    );

    let _ = render_at(&mut app, 120, 32);
    let field_hit = modal_hitbox(&app, HitAction::ModalField(DialogField::ExecutorMiddle1));
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: field_hit.x,
        row: field_hit.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("click Middle 1st slot");
    assert_eq!(
        app.modal.as_ref().unwrap().active_field(),
        DialogField::ExecutorMiddle1
    );
}

/// A validation error names a field that may live on a hidden tab: the
/// dialog must surface that tab with the field focused.
#[test]
fn settings_validation_error_focuses_the_field_tab() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    {
        let modal = app.modal.as_mut().expect("settings");
        modal.max_running_total = TextArea::new(vec!["not-a-number".to_string()]);
        modal.set_settings_tab(SettingsTab::Executor);
        modal.field_index = modal.fields().len() - 2;
    }
    app.handle_key(key(KeyCode::Enter)).expect("save rejected");
    let modal = app.modal.as_ref().expect("stays open");
    assert_eq!(modal.settings_tab, SettingsTab::Common);
    assert_eq!(modal.active_field(), DialogField::MaxRunningTotal);
    assert!(modal.error.is_some(), "{:?}", modal.error);
}

/// Edits made on different tabs all live in the one dialog state: they
/// survive a tab round-trip and a single Save persists them together.
#[test]
fn settings_edits_survive_tab_round_trips_and_one_save_persists_all() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    {
        let modal = app.modal.as_mut().expect("settings");
        modal.set_settings_tab(SettingsTab::Designer);
        modal.designer_enabled = true;
    }
    {
        let modal = app.modal.as_mut().expect("settings");
        modal.set_settings_tab(SettingsTab::Executor);
        let sonnet = modal
            .executor_slot_options
            .iter()
            .position(|option| option.value.as_deref() == Some("claude/sonnet"))
            .expect("claude/sonnet option");
        modal.executor_selected[3] = sonnet;
        modal.executor_week_threshold = TextArea::new(vec!["9".to_string()]);
    }

    // A round-trip through Common and back must not drop either edit.
    {
        let modal = app.modal.as_mut().expect("settings");
        modal.set_settings_tab(SettingsTab::Common);
        modal.set_settings_tab(SettingsTab::Executor);
    }
    let modal = app.modal.as_ref().expect("settings");
    assert!(modal.designer_enabled);
    assert_eq!(modal.executor_week_threshold.lines(), ["9"]);

    // Save from the Executor tab persists both tabs' edits.
    let modal = app.modal.as_mut().expect("settings");
    modal.field_index = modal.fields().len() - 2;
    app.handle_key(key(KeyCode::Enter)).expect("save settings");
    assert!(app.modal.is_none(), "save should close the dialog");
    let saved = app.ops.config.load_fresh().expect("reload");
    let orch = crate::core::config::OrchestrationSettings::from_mapping(&saved.orchestration);
    assert!(orch.designer.enabled);
    assert_eq!(
        orch.executors
            .cheap
            .first()
            .and_then(|candidate| candidate.backend.as_deref()),
        Some("claude")
    );
    assert_eq!(
        orch.executors
            .cheap
            .first()
            .and_then(|candidate| candidate.model.as_deref()),
        Some("sonnet")
    );
    assert_eq!(orch.executors.thresholds.week_percent, 9.0);
}

/// The four tabs, rendered: the strip, the rule and each tab's page. The
/// limits cache is pinned first so the Executor tab's quota annotations are
/// identical on every machine.
#[test]
fn settings_tab_pages_render() {
    let now = crate::core::limits::now_secs_for_tests();
    let window = |label: &str, remaining: f64| crate::core::limits::LimitWindow {
        label: label.to_string(),
        remaining_percent: remaining,
        resets_at: Some(now + 3_600),
        rolling: false,
    };
    crate::core::limits::set_cached_snapshot_for_tests(crate::core::limits::LimitsSnapshot {
        fetched_at: now,
        providers: vec![
            crate::core::limits::ProviderLimits {
                provider: "claude".to_string(),
                state: crate::core::limits::ProviderState::Ready,
                windows: vec![window("5h", 66.0), window("7d", 95.0)],
                observed_at: None,
            },
            crate::core::limits::ProviderLimits {
                provider: "codex".to_string(),
                state: crate::core::limits::ProviderState::Ready,
                windows: vec![window("5h", 1.0), window("7d", 2.0)],
                observed_at: None,
            },
        ],
    });
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    insta::assert_snapshot!("settings_tab_common", render_at(&mut app, 80, 24));
    let modal = app.modal.as_mut().unwrap();
    modal.set_settings_tab(SettingsTab::Designer);
    insta::assert_snapshot!("settings_tab_designer", render_at(&mut app, 80, 24));
    let modal = app.modal.as_mut().unwrap();
    modal.set_settings_tab(SettingsTab::Reviewer);
    insta::assert_snapshot!("settings_tab_reviewer", render_at(&mut app, 80, 24));
    let modal = app.modal.as_mut().unwrap();
    modal.set_settings_tab(SettingsTab::Executor);
    insta::assert_snapshot!("settings_tab_executor", render_at(&mut app, 80, 24));
}

/// Slot options carry the live provider numbers from the cached limits
/// snapshot, with an `(out of quota)` mark where the gate rejects a pair.
#[test]
fn settings_executor_tab_annotates_slots_with_live_quota() {
    let now = crate::core::limits::now_secs_for_tests();
    let window = |label: &str, remaining: f64| crate::core::limits::LimitWindow {
        label: label.to_string(),
        remaining_percent: remaining,
        resets_at: Some(now + 3_600),
        rolling: false,
    };
    crate::core::limits::set_cached_snapshot_for_tests(crate::core::limits::LimitsSnapshot {
        fetched_at: now,
        providers: vec![
            crate::core::limits::ProviderLimits {
                provider: "claude".to_string(),
                state: crate::core::limits::ProviderState::Ready,
                windows: vec![window("5h", 66.0), window("7d", 95.0)],
                observed_at: None,
            },
            crate::core::limits::ProviderLimits {
                provider: "codex".to_string(),
                state: crate::core::limits::ProviderState::Ready,
                windows: vec![window("5h", 1.0), window("7d", 2.0)],
                observed_at: None,
            },
        ],
    });

    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let modal = app.modal.as_mut().expect("settings");
    modal.set_settings_tab(SettingsTab::Executor);
    // Only the first slot's first options fit the frame; filter it down to
    // the claude pairs so their annotations are on screen.
    app.handle_key(key(KeyCode::Char('c'))).expect("filter");
    app.handle_key(key(KeyCode::Char('l'))).expect("filter");
    app.handle_key(key(KeyCode::Char('a'))).expect("filter");
    let rendered = render_at(&mut app, 120, 40);
    assert!(
        rendered.contains("claude/sonnet  5h 66%  7d 95%"),
        "{rendered}"
    );
    let modal = app.modal.as_mut().expect("settings");
    modal.executor_filters[0].clear();
    let rendered = render_at(&mut app, 120, 40);
    assert!(rendered.contains("(out of quota)"), "{rendered}");
}

#[test]
fn editing_task_does_not_overwrite_persisted_interactive_state() {
    // Given an edit dialog opened before another writer enables interactive.
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('e'))).expect("edit task");
    let mut task = app
        .ops
        .get_task("TASK-001")
        .expect("load task")
        .expect("task");
    task.interactive = true;
    app.ops
        .storage
        .save_task(&task)
        .expect("persist concurrent flag");

    // When the TUI edits and saves another field.
    app.modal
        .as_mut()
        .expect("modal")
        .title
        .insert_str(" updated");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::Confirm);
    app.handle_key(key(KeyCode::Enter)).expect("save edit");

    // Then the hidden compatibility field is preserved.
    let saved = app.ops.get_task("TASK-001").expect("reload").expect("task");
    assert!(saved.interactive);
}
#[test]
fn task_form_default_backend_inherits_settings_agent() {
    let (_dir, mut app) = populated_app();

    // Create a task without touching the backend selector: save snapshots
    // auto_launch.default_agent and that backend's configured model.
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
    assert_eq!(task.agent_backend.as_deref(), Some("opencode"));
    assert_eq!(task.ai_model.as_deref(), Some("openai/gpt-5.5"));
    assert!(!task.interactive, "new TUI tasks stay non-interactive");

    // Re-saving Default snapshots the same current board defaults.
    app.focused_column = 0;
    app.focused_card = app.board.columns[0]
        .tasks
        .iter()
        .position(|task| task.id == created_id)
        .expect("created task index");
    app.handle_key(key(KeyCode::Char('e'))).expect("edit");
    let modal = app.modal.as_ref().expect("edit modal");
    assert_eq!(modal.backend_options[0].value, None);
    assert_eq!(modal.backend_text().as_deref(), Some("opencode"));
    let modal = app.modal.as_mut().expect("edit modal");
    modal.field_index = modal.fields().len() - 2;
    app.handle_key(key(KeyCode::Enter)).expect("save edit");
    let task = app
        .ops
        .get_task(&created_id)
        .expect("reload")
        .expect("task");
    assert_eq!(task.agent_backend.as_deref(), Some("opencode"));
    assert_eq!(task.ai_model.as_deref(), Some("openai/gpt-5.5"));

    // A task with a pinned backend can be switched back to Default, which
    // snapshots the current default agent instead of leaving the field empty.
    app.focused_column = 2;
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Char('e')))
        .expect("edit claude task");
    let modal = app.modal.as_ref().expect("edit modal");
    assert_eq!(modal.backend_text().as_deref(), Some("claude"));
    app.handle_key(key(KeyCode::Tab)).expect("description");
    app.handle_key(key(KeyCode::Tab)).expect("agent settings");
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
    app.handle_key(key(KeyCode::Up)).expect("to opencode");
    app.handle_key(key(KeyCode::Up)).expect("to default");
    assert_eq!(app.modal.as_ref().expect("modal").backend_text(), None);
    app.handle_key(ctrl_key(KeyCode::Char('s')))
        .expect("stage popup");
    let modal = app.modal.as_mut().expect("edit modal");
    modal.field_index = modal.fields().len() - 2;
    app.handle_key(key(KeyCode::Enter)).expect("save edit");
    let task = app.ops.get_task("TASK-002").expect("reload").expect("task");
    assert_eq!(task.agent_backend.as_deref(), Some("opencode"));
}

/// Enter, Shift+Enter, and Alt+Enter all break lines inside the description.
/// Terminals (and tmux without extended-keys) deliver Shift+Enter as a bare
/// Enter, so the field must treat that the same as the modified chords. Tab
/// is what leaves the field.
#[test]
fn description_enter_shift_and_alt_enter_break_lines() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::Description);
    app.handle_key(key(KeyCode::Char('l'))).expect("type");
    app.handle_key(key(KeyCode::Char('i'))).expect("type");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
        .expect("shift newline");
    app.handle_key(key(KeyCode::Char('2'))).expect("type");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
        .expect("alt newline");
    app.handle_key(key(KeyCode::Char('3'))).expect("type");
    app.handle_key(key(KeyCode::Enter)).expect("plain newline");
    app.handle_key(key(KeyCode::Char('4'))).expect("type");
    assert_eq!(
        app.modal.as_ref().expect("modal").description.lines(),
        ["li", "2", "3", "4"]
    );
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::Description
    );

    app.handle_key(key(KeyCode::Tab)).expect("leave field");
    assert_ne!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::Description
    );
}

#[test]
fn description_ctrl_delete_and_backspace_remove_words() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    let modal = app.modal.as_mut().expect("modal");
    modal.focus_field(DialogField::Description);
    modal.description.insert_str("hello world foo");

    app.handle_key(key(KeyCode::End)).expect("end");
    app.handle_key(ctrl_key(KeyCode::Backspace))
        .expect("ctrl-backspace");
    assert_eq!(
        app.modal.as_ref().expect("modal").description.lines(),
        ["hello world "]
    );

    app.handle_key(key(KeyCode::Home)).expect("home");
    app.handle_key(ctrl_key(KeyCode::Delete))
        .expect("ctrl-delete");
    assert_eq!(
        app.modal.as_ref().expect("modal").description.lines(),
        [" world "]
    );
}

#[test]
fn description_ctrl_delete_removes_next_word_after_wrap_render() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    let modal = app.modal.as_mut().expect("modal");
    modal.focus_field(DialogField::Description);
    modal.description.insert_str("hello world foo");
    let _ = render_at(&mut app, 72, 40);

    app.handle_key(key(KeyCode::Home)).expect("home");
    app.handle_key(ctrl_key(KeyCode::Delete))
        .expect("ctrl-delete");
    assert_eq!(
        app.modal.as_ref().expect("modal").description.lines(),
        [" world foo"]
    );

    app.handle_key(key(KeyCode::Home)).expect("home");
    app.handle_key(alt_key(KeyCode::Delete))
        .expect("alt-delete");
    assert_eq!(
        app.modal.as_ref().expect("modal").description.lines(),
        [" foo"]
    );
}

/// Up on the first row of a multiline field jumps to the line start and Down
/// on the last row to the line end, instead of stalling at the boundary.
#[test]
fn multiline_up_at_top_and_down_at_bottom_reach_line_edges() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("new task modal")
        .focus_field(DialogField::Description);
    {
        let modal = app.modal.as_mut().expect("modal");
        modal.description = TextArea::from(["first", "second", "third"]);
    }

    app.handle_key(key(KeyCode::Down)).expect("down");
    app.handle_key(key(KeyCode::Down)).expect("down");
    let cursor = app.modal.as_ref().expect("modal").description.cursor();
    assert_eq!((cursor.0, cursor.1), (2, 0));
    app.handle_key(key(KeyCode::Down)).expect("down at bottom");
    let cursor = app.modal.as_ref().expect("modal").description.cursor();
    assert_eq!(
        (cursor.0, cursor.1),
        (2, 5),
        "Down on the last row must land on the line end"
    );

    app.handle_key(key(KeyCode::Up)).expect("up");
    app.handle_key(key(KeyCode::Up)).expect("up");
    let cursor = app.modal.as_ref().expect("modal").description.cursor();
    assert_eq!((cursor.0, cursor.1), (0, 5));
    app.handle_key(key(KeyCode::Up)).expect("up at top");
    let cursor = app.modal.as_ref().expect("modal").description.cursor();
    assert_eq!(
        (cursor.0, cursor.1),
        (0, 0),
        "Up on the first row must land on the line start"
    );
}

/// The review editor shares the multiline boundary rescue.
#[test]
fn review_editor_up_down_reach_line_edges_too() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Edge arrows"))
        .expect("task");
    app.ops.set_review_edits(&task.id, "one\ntwo").unwrap();
    app.ops.move_task(&task.id, "review", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.handle_key(key(KeyCode::Tab)).expect("focus editor");
    app.handle_key(key(KeyCode::End))
        .expect("end of first line");
    let cursor = app.detail.as_ref().expect("detail").review_edits.cursor();
    assert_eq!((cursor.0, cursor.1), (0, 3));

    app.handle_key(key(KeyCode::Down))
        .expect("down to last row");
    app.handle_key(key(KeyCode::Home)).expect("line start");
    app.handle_key(key(KeyCode::Down)).expect("down at bottom");
    let cursor = app.detail.as_ref().expect("detail").review_edits.cursor();
    assert_eq!(
        (cursor.0, cursor.1),
        (1, 3),
        "Down on the last row must land on the line end"
    );

    app.handle_key(key(KeyCode::Up)).expect("up one row");
    let cursor = app.detail.as_ref().expect("detail").review_edits.cursor();
    assert_eq!((cursor.0, cursor.1), (0, 3));
    app.handle_key(key(KeyCode::Up)).expect("up at top");
    let cursor = app.detail.as_ref().expect("detail").review_edits.cursor();
    assert_eq!(
        (cursor.0, cursor.1),
        (0, 0),
        "Up on the first row must land on the line start"
    );
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

    app.handle_key(key(KeyCode::Tab)).expect("agent settings");
    app.handle_key(key(KeyCode::Enter))
        .expect("open agent settings");
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

    app.handle_key(ctrl_key(KeyCode::Char('s')))
        .expect("stage agent settings");
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
        vec![
            "task_number",
            "task_number_desc",
            "updated_at_asc",
            "updated_at_desc"
        ]
    );
    {
        let modal = app.modal.as_mut().expect("settings modal");
        modal.title = TextArea::new(vec!["Renamed project".to_string()]);
    }
    app.handle_key(key(KeyCode::Tab)).expect("agent settings");
    app.handle_key(key(KeyCode::Enter))
        .expect("open agent settings");
    app.handle_key(key(KeyCode::Down)).expect("claude");
    app.handle_key(key(KeyCode::Tab)).expect("model");
    app.handle_key(key(KeyCode::Left)).expect("clear model");
    app.handle_key(key(KeyCode::Left)).expect("clear model");
    app.handle_key(key(KeyCode::Tab)).expect("effort");
    app.handle_key(key(KeyCode::Left)).expect("clear effort");
    app.handle_key(key(KeyCode::Left)).expect("clear effort");
    app.handle_key(key(KeyCode::Tab)).expect("agent");
    app.handle_key(ctrl_key(KeyCode::Char('s')))
        .expect("stage agent settings");
    app.handle_key(key(KeyCode::Tab)).expect("theme");
    app.handle_key(key(KeyCode::Up)).expect("dark theme");
    app.handle_key(key(KeyCode::Tab)).expect("task sorting");
    app.handle_key(key(KeyCode::Down))
        .expect("task number down");
    app.handle_key(key(KeyCode::Down))
        .expect("updated ascending sorting");
    let save_field = app.modal.as_ref().unwrap().fields().len() - 2;
    app.modal.as_mut().unwrap().field_index = save_field;
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
fn task_number_sort_applies_both_directions_to_every_column() {
    let (_dir, mut app) = settings_app();
    let mut expected_by_column = Vec::new();
    for status in ["todo", "in_progress", "review", "done"] {
        // Tasks are created in ascending id order within each column.
        let older = app
            .ops
            .create_task(NewTask::titled(format!("First {status}")))
            .unwrap();
        let middle = app
            .ops
            .create_task(NewTask::titled(format!("Second {status}")))
            .unwrap();
        if status != "todo" {
            app.ops.move_task(&older.id, status, false).unwrap();
            app.ops.move_task(&middle.id, status, false).unwrap();
        }
        expected_by_column.push((older.id, middle.id));
    }

    let mut config = app.ops.config.load_fresh().unwrap();
    config.tui.insert(
        serde_yaml_ng::Value::String("task_sort".to_string()),
        serde_yaml_ng::Value::String("task_number_desc".to_string()),
    );
    app.ops.config.save(&config).unwrap();
    assert_eq!(
        super::app::normalize_task_sort("task_number_desc"),
        "task_number_desc"
    );
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    assert_eq!(app.board.columns.len(), expected_by_column.len());
    for (column, (older, newer)) in app.board.columns.iter().zip(&expected_by_column) {
        assert_eq!(
            column.tasks[0].id, *newer,
            "descending sort puts the highest task number first"
        );
        assert_eq!(column.tasks[1].id, *older);
    }
}

#[test]
fn board_renders_with_descending_task_number_sort() {
    let (dir, mut app) = populated_app();
    let ops = Operations::new(dir.path());
    let mut config = ops.config.load_fresh().unwrap();
    config.tui.insert(
        serde_yaml_ng::Value::String("task_sort".to_string()),
        serde_yaml_ng::Value::String("task_number_desc".to_string()),
    );
    ops.config.save(&config).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    insta::assert_snapshot!("board_task_sort_desc", render_snapshot(&mut app));
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
    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter))
        .expect("open agent settings");
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
    for _ in 0..2 {
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
fn settings_dialog_loads_orchestration_defaults() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let modal = app.modal.as_ref().expect("settings modal");
    assert!(modal.queue_enabled);
    assert_eq!(modal.max_running_total.lines(), ["3"]);
    assert_eq!(modal.max_running_designer.lines(), ["1"]);
    assert_eq!(modal.max_running_reviewer.lines(), ["1"]);
    assert_eq!(modal.max_running_executor.lines(), ["3"]);
    let per_backend = modal.max_running_per_backend.lines().join("\n");
    assert!(per_backend.contains("claude: 2"));
    assert!(per_backend.contains("opencode: 2"));
    assert!(modal.auto_restart_enabled);
    assert_eq!(modal.auto_restart_delays.lines(), ["1, 30, 270"]);
    assert!(!modal.designer_enabled);
    assert_eq!(
        modal
            .backend_text_for(super::dialogs::AgentSlot::Designer)
            .as_deref(),
        Some("claude")
    );
    assert_eq!(
        modal
            .model_text_for(super::dialogs::AgentSlot::Designer)
            .as_deref(),
        Some("sonnet")
    );
    assert!(!modal.reviewer_enabled);
    assert_eq!(
        modal.reviewer_on_changes_text().as_deref(),
        Some("in_progress")
    );
    assert_eq!(modal.reviewer_max_rounds.lines(), ["3"]);
    assert!(
        modal
            .fields()
            .contains(&DialogField::MaxRunningPerBackendModel),
        "model cap field must be on the form"
    );
}

#[test]
fn settings_orchestration_snapshots_are_grouped() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    insta::assert_snapshot!("settings_orchestration_top", render_at(&mut app, 80, 24));

    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::QueueEnabled);
    let limits = render_at(&mut app, 80, 24);
    assert!(limits.contains("queue enabled"));
    assert!(limits.contains("Max running total"));
    insta::assert_snapshot!("settings_orchestration_limits", limits);

    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::MaxRunningPerBackendModel);
    let model_cap = render_at(&mut app, 80, 24);
    assert!(
        model_cap.contains("Max tasks per backend/model"),
        "{model_cap}"
    );
    insta::assert_snapshot!("settings_orchestration_model_cap", model_cap);

    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::AutoRestartEnabled);
    insta::assert_snapshot!(
        "settings_orchestration_restarts",
        render_at(&mut app, 80, 24)
    );

    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::DesignerEnabled);
    let designer = render_at(&mut app, 80, 24);
    assert!(designer.contains("Designer"));
    insta::assert_snapshot!("settings_orchestration_designer", designer);

    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::ReviewerEnabled);
    let reviewer = render_at(&mut app, 80, 24);
    assert!(reviewer.contains("Reviewer"));
    insta::assert_snapshot!("settings_orchestration_reviewer", reviewer);
}

#[test]
fn settings_save_persists_orchestration_and_keeps_unknown_keys() {
    let (dir, mut app) = settings_app();
    let config_path = dir.path().join(".kanban/config.yaml");
    let mut raw = std::fs::read_to_string(&config_path).expect("read config");
    raw.push_str(
        "\norchestration:\n  queue_enabled: true\n  extra_user_key: keep-me\n  designer:\n    enabled: false\n    note: leave-this\n",
    );
    std::fs::write(&config_path, raw).expect("write orchestration extras");

    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    {
        let modal = app.modal.as_mut().expect("settings");
        modal.queue_enabled = false;
        modal.max_running_total = TextArea::new(vec!["4".to_string()]);
        modal.max_running_designer = TextArea::new(vec!["2".to_string()]);
        modal.max_running_per_backend_model =
            TextArea::new(vec!["claude/opus: 1".to_string(), "opus: 2".to_string()]);
        modal.auto_restart_enabled = false;
        modal.auto_restart_delays = TextArea::new(vec!["5, 15".to_string()]);
        modal.designer_enabled = true;
        modal.reviewer_enabled = true;
        modal.reviewer_on_changes = TextArea::new(vec!["todo".to_string()]);
        modal.reviewer_max_rounds = TextArea::new(vec!["4".to_string()]);
        modal.field_index = modal.fields().len() - 2;
    }
    app.handle_key(key(KeyCode::Enter)).expect("save");
    assert!(app.modal.is_none(), "save should close the dialog");

    let saved = app.ops.config.load_fresh().expect("reload");
    let orch = crate::core::config::OrchestrationSettings::from_mapping(&saved.orchestration);
    assert!(!orch.queue_enabled);
    assert_eq!(orch.max_running_total, 4);
    assert_eq!(orch.max_running_per_role.get("designer").copied(), Some(2));
    assert_eq!(
        orch.max_running_per_backend_model
            .get("claude/opus")
            .copied(),
        Some(1)
    );
    assert_eq!(
        orch.max_running_per_backend_model
            .get("opencode/opus")
            .copied(),
        Some(2),
        "bare opus is prefixed with the selected default backend: {:?}",
        orch.max_running_per_backend_model
    );
    assert!(!orch.auto_restart_enabled);
    assert_eq!(orch.auto_restart_delays_minutes, vec![5, 15]);
    assert!(orch.designer.enabled);
    assert!(orch.reviewer.enabled);
    assert_eq!(
        orch.reviewer.on_changes_requested,
        crate::core::config::OnChangesRequested::Todo
    );
    assert_eq!(orch.reviewer.max_rounds, 4);
    assert_eq!(
        saved
            .orchestration
            .get("extra_user_key")
            .and_then(|value| value.as_str()),
        Some("keep-me")
    );
    assert_eq!(
        saved
            .orchestration
            .get("designer")
            .and_then(|value| value.get("note"))
            .and_then(|value| value.as_str()),
        Some("leave-this")
    );
}

#[test]
fn settings_model_cap_rejects_unknown_backend_and_empty_model() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    {
        let modal = app.modal.as_mut().expect("settings");
        modal.max_running_per_backend_model = TextArea::new(vec!["mystery/model: 1".to_string()]);
        modal.field_index = modal.fields().len() - 2;
    }
    app.handle_key(key(KeyCode::Enter)).expect("save rejected");
    let modal = app.modal.as_ref().expect("stays open");
    assert_eq!(modal.active_field(), DialogField::MaxRunningPerBackendModel);
    assert!(
        modal
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Unknown backend")),
        "{:?}",
        modal.error
    );

    {
        let modal = app.modal.as_mut().expect("settings");
        modal.max_running_per_backend_model = TextArea::new(vec!["claude/".to_string()]);
        modal.field_index = modal.fields().len() - 2;
    }
    app.handle_key(key(KeyCode::Enter))
        .expect("save rejected empty model");
    let modal = app.modal.as_ref().expect("stays open");
    assert!(
        modal
            .error
            .as_deref()
            .is_some_and(|error| error.contains("model id")),
        "{:?}",
        modal.error
    );
}

#[test]
fn settings_model_cap_prefills_backend_prefix_on_first_edit() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::MaxRunningPerBackendModel);
    app.modal.as_mut().unwrap().max_running_per_backend_model = TextArea::default();
    app.handle_key(key(KeyCode::Char('o'))).expect("type model");
    let text = app
        .modal
        .as_ref()
        .unwrap()
        .max_running_per_backend_model
        .lines()
        .join("");
    assert_eq!(text, "opencode/o");
}

#[test]
fn settings_designer_backend_change_does_not_clobber_default_model() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    let primary_model = app
        .modal
        .as_ref()
        .unwrap()
        .model_text()
        .expect("primary model");
    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::DesignerAgentSettings);
    app.handle_key(key(KeyCode::Enter))
        .expect("open designer settings");
    app.handle_key(key(KeyCode::Down))
        .expect("change designer backend");
    assert_eq!(
        app.modal.as_ref().unwrap().model_text().as_deref(),
        Some(primary_model.as_str()),
        "changing the designer backend must not rewrite the project default model"
    );
    assert!(
        app.modal
            .as_ref()
            .unwrap()
            .backend_text_for(super::dialogs::AgentSlot::Designer)
            .is_some()
    );
}

#[test]
fn mouse_backend_selection_refreshes_each_role_popup() {
    // Given each Project Settings agent launcher in turn.
    for (launcher, slot, backend_field) in [
        (
            DialogField::AgentSettings,
            AgentSlot::Primary,
            DialogField::Backend,
        ),
        (
            DialogField::DesignerAgentSettings,
            AgentSlot::Designer,
            DialogField::DesignerBackend,
        ),
        (
            DialogField::ReviewerAgentSettings,
            AgentSlot::Reviewer,
            DialogField::ReviewerBackend,
        ),
    ] {
        let (_dir, mut app) = settings_app();
        app.handle_key(key(KeyCode::Char('s')))
            .expect("open settings");
        app.modal.as_mut().expect("settings").focus_field(launcher);
        app.handle_key(key(KeyCode::Enter)).expect("open popup");
        let current = app.modal.as_ref().expect("popup").backend_text_for(slot);
        let target = app
            .modal
            .as_ref()
            .expect("popup")
            .options_for(backend_field)
            .iter()
            .enumerate()
            .find(|(_, option)| option.value.is_some() && option.value != current)
            .map(|(index, option)| (index, option.value.clone()))
            .expect("alternate backend");
        let _ = render_at(&mut app, 120, 40);
        let hit = modal_hitbox(
            &app,
            HitAction::ModalOption {
                field: backend_field,
                index: target.0,
            },
        );

        // When the backend option is clicked.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.x,
            row: hit.y,
            modifiers: KeyModifiers::NONE,
        })
        .expect("select backend");

        // Then the role keeps its popup and receives refreshed dependent options.
        let modal = app.modal.as_ref().expect("popup remains");
        assert_eq!(modal.agent_popup_slot(), Some(slot));
        assert_eq!(modal.backend_text_for(slot), target.1);
        assert!(!modal.options_for(backend_field).is_empty());
        let model_field = match slot {
            AgentSlot::Primary => DialogField::Model,
            AgentSlot::Designer => DialogField::DesignerModel,
            AgentSlot::Reviewer => DialogField::ReviewerModel,
        };
        assert!(modal.options_for(model_field).len() > 1);
    }
}

#[test]
fn wide_board_status_bar_is_not_clickable() {
    let (_dir, mut app) = settings_app();
    let _ = render_at(&mut app, 240, 28);
    let status_row = 28u16 - 1;
    assert!(
        !app.hitboxes
            .iter()
            .any(|hitbox| hitbox.area.y == status_row),
        "status bar must register no hitboxes"
    );
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
fn ctrl_c_copies_the_focused_task_without_quitting() {
    let (_dir, mut app) = app_with_board();
    let source = app
        .ops
        .create_task(NewTask::titled("Copy this task"))
        .expect("create source task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload board");

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .expect("copy focused task");

    let copied = app.focused_task().expect("focus copied task");
    assert_ne!(copied.id, source.id);
    assert_eq!(copied.title, source.title);
    assert!(!app.should_quit);
    assert_eq!(app.status, format!("Copied {} → {}", source.id, copied.id));
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
fn hover_selects_the_card_so_enter_opens_it() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    assert_eq!((app.focused_column, app.focused_card), (0, 0));
    let (column, card, area) = card_hits(&app)[1];
    let hovered_id = app.visible_tasks_for_column(column)[card].id.clone();

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: area.x + 1,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover card");

    assert_eq!((app.focused_column, app.focused_card), (column, card));
    app.handle_key(key(KeyCode::Enter))
        .expect("open hovered card");
    assert_eq!(app.screen, Screen::Detail);
    assert_eq!(
        app.detail.as_ref().expect("detail open").task_id,
        hovered_id
    );
}

#[test]
fn keyboard_navigation_retires_the_selection_under_the_pointer() {
    let (_dir, mut app) = plain_tasks_app(3);
    let _ = render_snapshot(&mut app);
    let (_, hovered, hover_area) = card_hits(&app)[1];
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: hover_area.x + 1,
        row: hover_area.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover middle card");
    assert_eq!(app.focused_card, hovered);

    // The pointer never moves, yet the selection does: the card under the
    // resting pointer must stop reading as selected.
    app.handle_key(key(KeyCode::Down)).expect("move selection");
    assert_eq!(app.focused_card, hovered + 1);
    let retired = style_at(&mut app, 96, 28, hover_area.x, hover_area.y);
    assert_ne!(retired.fg, Some(app.theme.focus));
}

#[test]
fn hovering_a_card_takes_the_selection_from_the_keyboard() {
    let (_dir, mut app) = plain_tasks_app(3);
    let _ = render_snapshot(&mut app);
    app.handle_key(key(KeyCode::Down)).expect("keyboard select");
    let (_, _, second_area) = card_hits(&app)[1];
    let (_, _, third_area) = card_hits(&app)[2];

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: third_area.x + 1,
        row: third_area.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover third card");

    assert_eq!(app.focused_card, 2);
    let dropped = style_at(&mut app, 96, 28, second_area.x, second_area.y);
    assert_ne!(dropped.fg, Some(app.theme.focus));
    let selected = style_at(&mut app, 96, 28, third_area.x, third_area.y);
    assert_eq!(selected.fg, Some(app.theme.focus));
}

#[test]
fn a_card_drag_sweeps_the_pointer_without_stealing_the_selection() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let (column, _, source) = card_hits(&app)[0];
    let (_, _, target) = card_hits(&app)[1];
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: source.x + 2,
        row: source.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("lift card");
    assert_eq!((app.focused_column, app.focused_card), (column, 0));

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: target.x + 2,
        row: target.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("drag over another column");
    assert_eq!((app.focused_column, app.focused_card), (column, 0));
}

#[test]
fn hovering_behind_an_open_modal_keeps_the_selection() {
    let (_dir, mut app) = plain_tasks_app(3);
    let _ = render_snapshot(&mut app);
    let (_, _, area) = card_hits(&app)[2];
    app.handle_key(key(KeyCode::Char('n')))
        .expect("open dialog");

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: area.x + 1,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover behind modal");
    assert_eq!((app.focused_column, app.focused_card), (0, 0));
}

#[test]
fn hovering_the_question_preview_selects_its_card() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    let preview = app
        .hitboxes
        .iter()
        .find(|hitbox| matches!(hitbox.action, HitAction::OpenAnswer { .. }))
        .copied()
        .expect("question preview hitbox");
    let HitAction::OpenAnswer { column, card } = preview.action else {
        unreachable!("checked above");
    };
    app.handle_key(key(KeyCode::Right))
        .expect("focus in-progress");
    app.handle_key(key(KeyCode::Right)).expect("focus review");

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: preview.area.x + 1,
        row: preview.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("hover preview line");
    assert_eq!((app.focused_column, app.focused_card), (column, card));
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

    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
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
fn review_editor_alt_delete_removes_next_word_like_description() {
    let (_dir, mut app) = open_focused_review_editor("hello world foo");

    app.handle_key(key(KeyCode::Home)).unwrap();
    app.handle_key(alt_key(KeyCode::Delete)).unwrap();
    assert_eq!(
        app.detail.as_ref().unwrap().review_edits.lines().join("\n"),
        " world foo"
    );
}

#[test]
fn review_editor_ctrl_delete_removes_next_word_after_wrap_render() {
    let (_dir, mut app) = open_focused_review_editor("hello world foo");
    let _ = render_at(&mut app, 72, 40);

    app.handle_key(key(KeyCode::Home)).unwrap();
    app.handle_key(ctrl_key(KeyCode::Delete)).unwrap();
    assert_eq!(
        app.detail.as_ref().unwrap().review_edits.lines().join("\n"),
        " world foo"
    );

    app.handle_key(key(KeyCode::Home)).unwrap();
    app.handle_key(KeyEvent::new(
        KeyCode::Delete,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ))
    .unwrap();
    assert_eq!(
        app.detail.as_ref().unwrap().review_edits.lines().join("\n"),
        " foo"
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
    app.handle_key(key(KeyCode::Home)).unwrap();
    assert_eq!(app.detail.as_ref().unwrap().scroll, 0);

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

#[test]
fn hide_kanban_messages_filters_thread_but_keeps_sidecar() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Filter kanban lines"))
        .unwrap();
    let thread_manager = ThreadManager::new(dir.path()).unwrap();
    thread_manager
        .post(
            &task.id,
            crate::core::models::MessageRole::System,
            crate::core::models::MessageKind::System,
            "KANBAN-AUDIT-NOTE",
            None,
            Vec::new(),
            Some("kanban".to_string()),
        )
        .unwrap();
    thread_manager
        .post(
            &task.id,
            crate::core::models::MessageRole::Agent,
            crate::core::models::MessageKind::Context,
            "AGENT-REPLY-BODY",
            None,
            Vec::new(),
            Some("agent-reply".to_string()),
        )
        .unwrap();
    app.settings.hide_kanban_messages = true;
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.clamp_focus();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let rendered = render_snapshot(&mut app);
    assert!(
        rendered.contains("AGENT-REPLY-BODY"),
        "non-kanban messages must stay visible:\n{rendered}"
    );
    assert!(
        !rendered.contains("KANBAN-AUDIT-NOTE"),
        "kanban messages must be hidden:\n{rendered}"
    );
    assert!(
        !rendered.contains("Task created:"),
        "initial kanban system message must be hidden:\n{rendered}"
    );

    let stored = thread_manager.load(&task.id).unwrap();
    assert!(
        stored
            .messages
            .iter()
            .any(|message| message.body.contains("KANBAN-AUDIT-NOTE")),
        "filter must not delete sidecar messages"
    );
}

#[test]
fn settings_save_persists_hide_kanban_messages() {
    let (_dir, mut app) = settings_app();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    {
        let modal = app.modal.as_mut().expect("settings");
        modal.focus_field(DialogField::HideKanbanMessages);
        assert!(!modal.hide_kanban_messages);
    }
    app.handle_key(key(KeyCode::Char(' ')))
        .expect("toggle hide");
    assert!(app.modal.as_ref().expect("settings").hide_kanban_messages);
    let save_field = app.modal.as_ref().unwrap().fields().len() - 2;
    app.modal.as_mut().unwrap().field_index = save_field;
    app.handle_key(key(KeyCode::Enter)).expect("save settings");

    assert!(app.settings.hide_kanban_messages);
    let config = app.ops.config.load().expect("saved config");
    assert_eq!(
        config
            .tui
            .get("hide_kanban_messages")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
}

#[test]
fn opening_short_thread_does_not_scroll() {
    let (_dir, mut app) = app_with_board();
    app.ops
        .create_task(NewTask::titled("Short thread"))
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.clamp_focus();
    app.handle_key(key(KeyCode::Enter)).unwrap();
    let rendered = render_at(&mut app, 96, 28);
    assert_eq!(app.detail.as_ref().unwrap().scroll, 0);
    assert_eq!(app.detail.as_ref().unwrap().max_scroll, 0);
    assert!(
        rendered.contains("Task created:"),
        "short thread stays at the top:\n{rendered}"
    );
}

#[test]
fn opening_long_thread_pins_last_message_without_blank_tail() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Long thread pin"))
        .unwrap();
    let thread_manager = ThreadManager::new(dir.path()).unwrap();
    thread_manager
        .post(
            &task.id,
            crate::core::models::MessageRole::Agent,
            crate::core::models::MessageKind::Context,
            "FIRST-MARKER",
            None,
            Vec::new(),
            Some("agent".to_string()),
        )
        .unwrap();
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
    thread_manager
        .post(
            &task.id,
            crate::core::models::MessageRole::Agent,
            crate::core::models::MessageKind::Context,
            "LAST-MARKER",
            None,
            Vec::new(),
            Some("agent".to_string()),
        )
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.clamp_focus();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let rendered = render_at(&mut app, 96, 20);
    let detail = app.detail.as_ref().unwrap();
    assert!(detail.max_scroll > 0);
    assert_eq!(detail.scroll, detail.max_scroll);
    assert!(
        rendered.contains("LAST-MARKER"),
        "last message must be in view:\n{rendered}"
    );
    assert!(
        !rendered.contains("FIRST-MARKER"),
        "first extra message must be scrolled away:\n{rendered}"
    );
}

#[test]
fn opening_with_filter_pins_last_visible_message() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Filtered pin"))
        .unwrap();
    let thread_manager = ThreadManager::new(dir.path()).unwrap();
    for index in 0..20 {
        thread_manager
            .post(
                &task.id,
                crate::core::models::MessageRole::Agent,
                crate::core::models::MessageKind::Context,
                &format!("VISIBLE-LINE-{index}"),
                None,
                Vec::new(),
                Some("agent".to_string()),
            )
            .unwrap();
        thread_manager
            .post(
                &task.id,
                crate::core::models::MessageRole::System,
                crate::core::models::MessageKind::System,
                &format!("HIDDEN-LINE-{index}"),
                None,
                Vec::new(),
                Some("kanban".to_string()),
            )
            .unwrap();
    }
    thread_manager
        .post(
            &task.id,
            crate::core::models::MessageRole::Agent,
            crate::core::models::MessageKind::Context,
            "VISIBLE-LAST",
            None,
            Vec::new(),
            Some("agent".to_string()),
        )
        .unwrap();
    thread_manager
        .post(
            &task.id,
            crate::core::models::MessageRole::System,
            crate::core::models::MessageKind::System,
            "HIDDEN-LAST",
            None,
            Vec::new(),
            Some("kanban".to_string()),
        )
        .unwrap();
    app.settings.hide_kanban_messages = true;
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.clamp_focus();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let rendered = render_at(&mut app, 96, 20);
    assert!(
        rendered.contains("VISIBLE-LAST"),
        "last visible message must be in view:\n{rendered}"
    );
    assert!(
        !rendered.contains("HIDDEN-LAST"),
        "filtered kanban tail must stay hidden:\n{rendered}"
    );
    let detail = app.detail.as_ref().unwrap();
    assert!(
        detail.thread_selected.is_none(),
        "opening a task must not pre-highlight a message"
    );
}

#[test]
fn opening_tall_last_message_puts_header_at_top() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Tall last message"))
        .unwrap();
    let thread_manager = ThreadManager::new(dir.path()).unwrap();
    thread_manager
        .post(
            &task.id,
            crate::core::models::MessageRole::Agent,
            crate::core::models::MessageKind::Context,
            "FIRST-MARKER",
            None,
            Vec::new(),
            Some("agent".to_string()),
        )
        .unwrap();
    for index in 0..8 {
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
    let last_body = (0..40)
        .map(|index| format!("LAST-BODY-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    thread_manager
        .post(
            &task.id,
            crate::core::models::MessageRole::Agent,
            crate::core::models::MessageKind::Context,
            &last_body,
            None,
            Vec::new(),
            Some("agent".to_string()),
        )
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.clamp_focus();
    app.handle_key(key(KeyCode::Enter)).unwrap();

    let rendered = render_at(&mut app, 96, 20);
    let detail = app.detail.as_ref().unwrap();
    assert!(detail.max_scroll > 0);
    assert!(
        detail.scroll < detail.max_scroll,
        "tall last message should pin its header, not the thread tail"
    );
    assert!(
        rendered.contains("LAST-BODY-0"),
        "first line of last message must be in view:\n{rendered}"
    );
    assert!(
        !rendered.contains("FIRST-MARKER"),
        "earlier messages must be scrolled away:\n{rendered}"
    );
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
fn review_edits_soft_wraps_like_task_description() {
    let prose = "Soft wrapping keeps normal prose readable in a narrow terminal. ";
    let token = "unbroken".repeat(16);
    let edits = format!("{prose}{token}");
    let (_dir, mut app) = open_focused_review_editor(&edits);

    assert_eq!(
        app.detail
            .as_ref()
            .expect("detail")
            .review_edits
            .wrap_mode(),
        WrapMode::WordOrGlyph
    );

    app.detail
        .as_mut()
        .expect("detail")
        .review_edits
        .input(key(KeyCode::Home));
    let prose_view = render_at(&mut app, 72, 40);
    assert!(prose_view.contains("Soft wrapping"));
    app.detail
        .as_mut()
        .expect("detail")
        .review_edits
        .input(key(KeyCode::End));

    let _ = render_at(&mut app, 160, 48);
    let logical_cursor = app.detail.as_ref().expect("detail").review_edits.cursor();
    let wide_cursor = app
        .detail
        .as_ref()
        .expect("detail")
        .review_edits
        .screen_cursor();
    let narrow = render_at(&mut app, 72, 40);
    let detail = app.detail.as_ref().expect("detail");
    assert_eq!(detail.review_edits.lines(), std::slice::from_ref(&edits));
    assert_eq!(detail.review_edits.cursor(), logical_cursor);
    assert!(detail.review_edits.screen_cursor().row > wide_cursor.row);
    assert!(narrow.contains("unbroken"));
    assert!(!narrow.contains(&token), "long token must be glyph-wrapped");

    let narrow_cursor = detail.review_edits.screen_cursor();
    let detail = app.detail.as_mut().expect("detail");
    detail.review_edits.input(key(KeyCode::Up));
    assert_eq!(
        detail.review_edits.screen_cursor().row + 1,
        narrow_cursor.row,
        "Up must move by one visual wrapped row"
    );
    assert_eq!(detail.review_edits.lines(), std::slice::from_ref(&edits));
    detail.review_edits.input(key(KeyCode::Down));
    assert_eq!(detail.review_edits.cursor(), logical_cursor);
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
fn run_hotkey_falls_back_to_direct_start_when_auto_launch_is_off() {
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

/// Launch calls recorded by [`QueueLaunchSpy`]: (task_id, session_id).
#[derive(Default, Clone)]
struct QueueLaunchSpy(std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>);

impl QueueLaunchSpy {
    fn calls(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

impl crate::core::operations::AgentLauncher for QueueLaunchSpy {
    fn launch(
        &self,
        _roots: crate::core::project::Roots<'_>,
        task: &crate::core::models::Task,
        session_id: &str,
        _revert: bool,
    ) -> crate::core::error::Result<bool> {
        self.0
            .lock()
            .unwrap()
            .push((task.id.clone(), session_id.to_string()));
        Ok(true)
    }
}

/// Board with the queue on, auto-launch on, a total cap of one agent, and a
/// spy launcher standing in for the real backend spawn.
fn queue_run_app() -> (tempfile::TempDir, App, QueueLaunchSpy) {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  queue_enabled: true\n  max_running_total: 1\n  max_running_per_backend: {}\n  max_running_per_role: {}\nagents:\n  opencode:\n    command: /nonexistent/opencode-disabled-for-tests\n",
    )
    .expect("quiet config");
    let spy = QueueLaunchSpy::default();
    let mut app = App::new(dir.path()).expect("create app");
    app.ops = Operations::with_launcher(dir.path(), Box::new(spy.clone()));
    (dir, app, spy)
}

/// An In Progress task with a live session that consumes the board's single
/// agent slot.
fn occupy_the_only_slot(dir: &std::path::Path, app: &mut App) {
    let occupier = app
        .ops
        .create_task(NewTask::titled("Occupier"))
        .expect("occupier");
    let mut current = app.ops.get_task(&occupier.id).unwrap().unwrap();
    current.status = TaskStatus::InProgress;
    current.session = Some("ses-occupy".to_string());
    app.ops.storage.save_task(&current).unwrap();
    SessionManager::new(dir)
        .link_named_session(&occupier.id, "ses-occupy", "Occupier")
        .unwrap();
}

#[test]
fn run_hotkey_queues_and_pumps_the_queue() {
    let (dir, mut app, spy) = queue_run_app();
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

    // The immediate pump started the queued task on the spot, and the queue
    // note is on the thread for the audit trail.
    let started = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(started.status, TaskStatus::InProgress);
    assert_ne!(started.run_phase, Some(RunPhase::Queued));
    let session_id = started.session.expect("session assigned");
    assert!(SessionManager::new(dir.path()).is_session_active(&session_id));
    let calls = spy.calls();
    assert_eq!(calls.len(), 1, "the pump must launch the queued task");
    assert_eq!(calls[0].0, task.id);
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(thread.messages.iter().any(|m| m.body.contains("queued")));

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
    assert!(app.status.contains("Revoked and woke"), "{}", app.status);
    let revoked = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_ne!(revoked.session.as_deref(), Some(session_id.as_str()));
}

#[test]
fn run_hotkey_leaves_task_queued_when_caps_are_full() {
    let (dir, mut app, spy) = queue_run_app();
    occupy_the_only_slot(dir.path(), &mut app);
    let mine = app
        .ops
        .create_task(NewTask::titled("Queue me"))
        .expect("create task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.clamp_focus();
    assert_eq!(
        app.focused_task().map(|focused| focused.id.clone()),
        Some(mine.id.clone()),
        "the only To Do card is the pressed one"
    );

    app.handle_key(key(KeyCode::Char('r'))).expect("run");

    let stored = app.ops.get_task(&mine.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.run_phase, Some(RunPhase::Queued));
    assert_eq!(stored.session, None, "a queued task owns no session");
    assert!(spy.calls().is_empty(), "a full board must not launch");
    assert_eq!(app.status, "Queued TASK-002 — starts when a slot frees");
}

#[test]
fn run_now_hotkey_bypasses_the_queue() {
    let (dir, mut app, spy) = queue_run_app();
    occupy_the_only_slot(dir.path(), &mut app);
    let mine = app
        .ops
        .create_task(NewTask::titled("Run me now"))
        .expect("create task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.clamp_focus();

    app.handle_key(key(KeyCode::Char('F'))).expect("run now");

    let stored = app.ops.get_task(&mine.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_ne!(
        stored.run_phase,
        Some(RunPhase::Queued),
        "F must not park the task in the queue"
    );
    let session_id = stored.session.expect("session assigned");
    assert!(SessionManager::new(dir.path()).is_session_active(&session_id));
    let calls = spy.calls();
    assert_eq!(calls.len(), 1, "F launches despite the full board");
    assert_eq!(calls[0].0, mine.id);
    assert!(app.status.starts_with("Started"), "status: {}", app.status);
}

#[test]
fn run_hotkey_starts_directly_when_the_queue_is_disabled() {
    let (dir, mut app, spy) = queue_run_app();
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  queue_enabled: false\nagents:\n  opencode:\n    command: /nonexistent/opencode-disabled-for-tests\n",
    )
    .expect("quiet config");
    let task = app
        .ops
        .create_task(NewTask::titled("Run regardless"))
        .expect("create task");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.clamp_focus();

    app.handle_key(key(KeyCode::Char('r'))).expect("run");

    assert!(app.status.starts_with("Started"), "status: {}", app.status);
    assert!(
        app.status.contains("queue is off"),
        "the status must say why the run went direct: {}",
        app.status
    );
    let stored = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_ne!(stored.run_phase, Some(RunPhase::Queued));
    let session_id = stored.session.expect("session assigned");
    assert!(SessionManager::new(dir.path()).is_session_active(&session_id));
    assert_eq!(spy.calls().len(), 1);
}

#[test]
fn review_rerun_lands_queued_when_the_queue_is_on() {
    let (dir, mut app, spy) = queue_run_app();
    let task = app
        .ops
        .create_task(NewTask::titled("Review queue"))
        .expect("create task");
    app.ops
        .move_task(&task.id, "review", false)
        .expect("move to review");
    app.ops.set_review_edits(&task.id, "Queued rework").unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = review_column(&app);
    app.focused_card = 0;

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .expect("rerun");

    // Free caps: the pump picked the queued re-run up immediately, the edits
    // are folded, and the board focus followed the task to In Progress.
    let stored = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_ne!(stored.run_phase, Some(RunPhase::Queued));
    assert!(stored.review_edits.is_empty());
    assert_eq!(spy.calls().len(), 1);
    assert_eq!(app.screen, Screen::Board);
    assert_eq!(app.focused_column, in_progress_column(&app));
    assert_eq!(
        app.focused_task().map(|focused| focused.id.as_str()),
        Some(task.id.as_str())
    );
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread.messages.iter().any(|m| m.body == "Queued rework"),
        "the folded edits must be on the thread"
    );
}

#[test]
fn review_rerun_parks_queued_when_caps_are_full() {
    let (dir, mut app, spy) = queue_run_app();
    occupy_the_only_slot(dir.path(), &mut app);
    let task = app
        .ops
        .create_task(NewTask::titled("Review parked"))
        .expect("create task");
    app.ops
        .move_task(&task.id, "review", false)
        .expect("move to review");
    app.ops.set_review_edits(&task.id, "Fold me later").unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.focused_column = review_column(&app);
    app.focused_card = 0;

    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .expect("rerun");

    let stored = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::InProgress);
    assert_eq!(stored.run_phase, Some(RunPhase::Queued));
    assert!(stored.session.is_none());
    assert!(stored.review_edits.is_empty());
    assert!(spy.calls().is_empty(), "a full board must not launch");
    assert_eq!(app.status, "Queued TASK-002 for re-run");
    assert_eq!(app.focused_column, in_progress_column(&app));
    let thread = ThreadManager::new(dir.path())
        .unwrap()
        .load(&task.id)
        .unwrap();
    assert!(
        thread
            .messages
            .iter()
            .any(|m| m.kind == crate::core::models::MessageKind::ReviewEdit),
        "the edits fold even when the run parks in the queue"
    );
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

    let task_count = app
        .ops
        .list_tasks(None, None, "created", "asc")
        .expect("count source tasks")
        .len();
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .expect("ctrl-c copies focused task");
    assert!(!app.should_quit);
    assert_eq!(
        app.ops
            .list_tasks(None, None, "created", "asc")
            .expect("count copied tasks")
            .len(),
        task_count + 1
    );
    assert!(app.status.starts_with("Copied "));

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
    assert_eq!(
        app.status,
        "Press ctrl + C twice to close when no task is selected"
    );
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
fn phase_three_headers_new_task_always_targets_todo_and_bulk_confirmation_work() {
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
            target_status: Some("todo".to_string())
        }
    );
    modal.title.insert_str("Targeted");
    modal.field_index = modal.fields().len() - 2;
    app.handle_key(key(KeyCode::Enter))
        .expect("create targeted task");
    assert_eq!(
        app.ops
            .list_tasks(Some("todo"), None, "created", "asc")
            .unwrap()
            .len(),
        1
    );
    let task = app
        .ops
        .list_tasks(Some("todo"), None, "created", "asc")
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
fn phase_three_question_focus_and_drag_hitboxes_drive_board_state() {
    let (_dir, mut app) = populated_app();
    let _ = render_snapshot(&mut app);
    app.focus_first_question();
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
fn live_design_and_review_cards_show_the_role_running_row() {
    for (phase, badge, color) in [
        (RunPhase::Design, "✎ design", "Rgb(106, 153, 255)"),
        (RunPhase::Review, "⚖ review", "Rgb(210, 95, 180)"),
    ] {
        let (dir, mut app) = app_with_board();
        let mut running = app.ops.create_task(NewTask::titled("Palette")).unwrap();
        SessionManager::new(dir.path())
            .link_session(&running.id, "ses-pal")
            .unwrap();
        running.session = Some("ses-pal".to_string());
        running.agent_backend = Some("claude".to_string());
        running.run_phase = Some(phase);
        app.ops.storage.save_task(&running).unwrap();
        // Same claude transcript as the executor telemetry test: tokens and a
        // last-tool line, no todos.
        let transcript = dir.path().join(".kanban/logs/ses-pal.transcript.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"assistant","message":{"usage":{"input_tokens":12000,"output_tokens":400},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/x.rs"}}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
        app.tick().unwrap();

        let board = render_at(&mut app, 120, 18);
        assert!(board.contains(badge), "phase badge:\n{board}");
        // The badge names the phase, so "▶ running" can only come from the
        // dedicated role-colored row under it.
        assert!(board.contains("▶ running"), "running row:\n{board}");
        assert!(board.contains("12.4k"), "tokens:\n{board}");
        assert!(board.contains(color), "role color:\n{board}");
        // Title + badge + running row + stats + activity.
        assert_eq!(super::card::card_line_count(&app, &running), 5);
    }

    // An executor run keeps the badge-only card: no extra row, and without a
    // transcript no telemetry rows either.
    let (dir, mut app) = app_with_board();
    let mut running = app.ops.create_task(NewTask::titled("Executor")).unwrap();
    SessionManager::new(dir.path())
        .link_session(&running.id, "ses-exec")
        .unwrap();
    running.session = Some("ses-exec".to_string());
    app.ops.storage.save_task(&running).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    assert_eq!(super::card::card_line_count(&app, &running), 2);
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
    app.focus_first_question();
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
    assert!(board.contains("r queue"), "{board}");
    assert!(!board.contains("r revoke"), "{board}");

    app.focused_column = 1;
    app.dispatch(UiAction::OpenDetail).unwrap();
    let detail = render_at(&mut app, 120, 24);
    assert!(!detail.contains("press u / Recover"), "{detail}");
    assert!(!detail.contains("[ Recover u ]"), "{detail}");
    assert!(detail.contains("Queue r"), "{detail}");
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
fn phase_seven_status_bar_is_contextual_and_not_clickable() {
    let (_dir, mut app) = app_with_board();
    let rendered = render_at(&mut app, 140, 18);
    assert!(rendered.contains("n new"));
    assert!(rendered.contains("b review done"));
    // The status bar is an informational hotkey panel: no hitboxes on its row.
    let status_row = 18u16 - 1;
    assert!(
        !app.hitboxes
            .iter()
            .any(|hitbox| hitbox.area.y == status_row),
        "status bar must register no hitboxes"
    );

    app.handle_key(key(KeyCode::Char('l'))).unwrap();
    let sessions = render_snapshot(&mut app);
    assert!(sessions.contains("x kill"));
    // The Sessions status bar is informational too: its hints carry no hitboxes.
    assert!(
        !app.hitboxes
            .iter()
            .any(|hitbox| hitbox.action == HitAction::Action(UiAction::ViewLog)),
        "sessions status bar must register no hitboxes"
    );
    app.handle_key(key(KeyCode::Char('q'))).unwrap();

    // A narrow terminal drops low-priority segments instead of clipping.
    let narrow = render_at(&mut app, 48, 18);
    assert!(narrow.contains("r queue"));
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
    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::ChainTo);
    let output = render_at(&mut app, 80, 24);
    assert!(output.contains("Chain to"));
    insta::assert_snapshot!("phase_four_form_scrolled_80x24", output);

    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Char('n'))).unwrap();
    assert!(!app.modal.as_ref().unwrap().is_dirty());
    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::Confirm);
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
    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::ChainTo);
    for _ in 0..4 {
        app.handle_key(key(KeyCode::Down)).unwrap();
    }
    assert_eq!(app.modal.as_ref().unwrap().chain_selected, 4);

    let _ = render_at(&mut app, 80, 24);
    // The chain row is capped small at this size, so the list is scrolled:
    // clicking the single visible option must map the visible row back to
    // its absolute option index.
    let visible = app
        .hitboxes
        .iter()
        .find(|hitbox| {
            matches!(
                hitbox.action,
                HitAction::ModalOption {
                    field: DialogField::ChainTo,
                    ..
                }
            )
        })
        .copied()
        .expect("visible scrolled option");
    let HitAction::ModalOption {
        field: DialogField::ChainTo,
        index: visible_index,
    } = visible.action
    else {
        unreachable!("matched above");
    };
    assert_ne!(visible_index, 0, "the selector must be scrolled");
    let expected = app.modal.as_ref().unwrap().chain_options[visible_index]
        .value
        .clone();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: visible.area.x,
        row: visible.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    let modal = app.modal.as_ref().unwrap();
    assert_eq!(modal.chain_selected, visible_index);
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
    app.modal
        .as_mut()
        .unwrap()
        .focus_field(DialogField::ChainTo);
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
    let launcher = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::ModalField(DialogField::AgentSettings))
        .copied()
        .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: launcher.area.x,
        row: launcher.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
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
    app.handle_key(key(KeyCode::Esc)).unwrap();

    app.handle_key(key(KeyCode::Char('c'))).unwrap();
    let output = render_at(&mut app, 96, 28);
    assert!(output.contains("[ Save ]  [ Cancel ]"));
    let save_hint = "(Ctrl + S)";
    let nav_hint = "use Tab, Enter or Shift + Tab to navigate";
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

    // Wide enough that the action bar fits every button, including the
    // viewers that a narrower terminal drops first.
    let rendered = render_at(&mut app, 140, 28);
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

/// The answer panel must not submit on Shift/Alt+Enter — those break a line
/// in the custom answer box. Plain Enter submits.
#[test]
fn answer_panel_shift_enter_breaks_a_line_instead_of_submitting() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Question holder"))
        .expect("task");
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

    app.handle_key(key(KeyCode::Char('o'))).expect("type");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
        .expect("shift newline");
    app.handle_key(key(KeyCode::Char('k'))).expect("type");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
        .expect("alt newline");
    app.handle_key(key(KeyCode::Char('9'))).expect("type");
    let detail = app.detail.as_ref().expect("detail state");
    assert_eq!(detail.answer_input.lines(), ["o", "k", "9"]);
    assert!(
        app.ops.get_task(&task.id).unwrap().unwrap().has_questions,
        "Shift/Alt+Enter must not submit the answer"
    );

    // Plain Enter submits the custom answer and clears the question.
    app.handle_key(key(KeyCode::Enter)).expect("submit");
    assert!(
        !app.ops.get_task(&task.id).unwrap().unwrap().has_questions,
        "plain Enter must submit"
    );
}

/// The review editor takes the same newline keys as the dialogs: Shift+Enter
/// and Alt+Enter both break a line (plain Enter already does).
#[test]
fn review_editor_shift_and_alt_enter_break_lines() {
    let (_dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Edit review")).unwrap();
    app.ops.set_review_edits(&task.id, "abcdef").unwrap();
    app.ops.move_task(&task.id, "review", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.focused_column = review_column(&app);
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Enter)).unwrap();
    app.handle_key(key(KeyCode::Tab)).expect("focus editor");
    assert_eq!(
        app.detail.as_ref().expect("detail").focus,
        DetailFocus::Edits
    );
    app.handle_key(key(KeyCode::End)).expect("cursor to end");

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
        .expect("shift newline");
    app.handle_key(key(KeyCode::Char('s'))).expect("type");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
        .expect("alt newline");
    app.handle_key(key(KeyCode::Char('a'))).expect("type");
    assert_eq!(
        app.detail
            .as_ref()
            .expect("detail")
            .review_edits
            .lines()
            .join("\n"),
        "abcdef\ns\na"
    );
}

/// In the detail answer panel ←/→ move the custom-answer caret; switching
/// questions happens through the explicit prev/next buttons only.
#[test]
fn detail_answer_arrows_edit_text_and_buttons_switch_questions() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Two questions"))
        .expect("task");
    app.ops
        .ask_question(&task.id, "First?", "agent", vec!["one".to_string()])
        .expect("first question");
    app.ops
        .ask_question(
            &task.id,
            "Second?",
            "agent",
            vec!["alpha".to_string(), "beta".to_string()],
        )
        .expect("second question");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.handle_key(key(KeyCode::Tab)).expect("focus answer");
    assert_eq!(
        app.detail.as_ref().expect("detail state").focus,
        DetailFocus::Answer
    );

    for character in "hello world".chars() {
        app.handle_key(key(KeyCode::Char(character)))
            .expect("type answer");
    }
    app.handle_key(key(KeyCode::Left)).expect("left");
    app.handle_key(key(KeyCode::Left)).expect("left");
    let detail = app.detail.as_ref().expect("detail state");
    assert_eq!(detail.answer_input.lines(), ["hello world"]);
    let cursor = detail.answer_input.cursor();
    assert_eq!((cursor.0, cursor.1), (0, 9), "← moves the answer caret");
    assert_eq!(detail.question_index, 0, "← must not switch questions");
    assert_eq!(detail.variant_selected, 0);
    app.handle_key(key(KeyCode::Home)).expect("home");
    let cursor = app
        .detail
        .as_ref()
        .expect("detail state")
        .answer_input
        .cursor();
    assert_eq!((cursor.0, cursor.1), (0, 0));
    assert_eq!(app.detail.as_ref().expect("detail state").question_index, 0);

    // Only "next question >" is offered on the first question, and clicking
    // it swaps to the second question with a fresh answer draft.
    let _ = render_snapshot(&mut app);
    assert!(
        !app.hitboxes
            .iter()
            .any(|hitbox| hitbox.action == HitAction::DetailPrevQuestion),
        "previous is disabled on the first question"
    );
    let next = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::DetailNextQuestion)
        .copied()
        .expect("next question button");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: next.area.x,
        row: next.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("click next question");
    let detail = app.detail.as_ref().expect("detail state");
    assert_eq!(detail.question_index, 1);
    assert!(detail.answer_input.lines()[0].is_empty(), "draft resets");
    assert_eq!(detail.variant_selected, 0);

    // On the last question only "< previous question" remains.
    let _ = render_snapshot(&mut app);
    assert!(
        !app.hitboxes
            .iter()
            .any(|hitbox| hitbox.action == HitAction::DetailNextQuestion),
        "next is disabled on the last question"
    );
    let prev = app
        .hitboxes
        .iter()
        .find(|hitbox| hitbox.action == HitAction::DetailPrevQuestion)
        .copied()
        .expect("previous question button");
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: prev.area.x,
        row: prev.area.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("click previous question");
    assert_eq!(app.detail.as_ref().expect("detail state").question_index, 0);
}

/// The description title names Enter as the newline key so a terminal that
/// cannot report Shift+Enter still documents a working chord.
#[test]
fn description_title_names_enter_as_newline() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    let full = render_at(&mut app, 120, 30);
    assert!(full.contains("Enter newline"), "{full}");
}

/// Only a tmux server that reports extended keys in CSI-u form may be asked
/// for modifyOtherKeys: the xterm format it would otherwise use is not
/// parseable here and would swallow every modified key.
#[test]
fn tmux_option_scan_requires_csi_u_extended_keys() {
    assert!(super::tmux_reports_csi_u(
        "escape-time 10\nextended-keys on\nextended-keys-format csi-u\n"
    ));
    assert!(super::tmux_reports_csi_u(
        "extended-keys always\nextended-keys-format csi-u"
    ));
    assert!(!super::tmux_reports_csi_u(
        "extended-keys off\nextended-keys-format csi-u\n"
    ));
    assert!(!super::tmux_reports_csi_u(
        "extended-keys on\nextended-keys-format xterm\n"
    ));
    assert!(
        !super::tmux_reports_csi_u("escape-time 10\n"),
        "options missing entirely"
    );
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

/// Stamp a project's `last_opened_at` so the smart sort's recency stage is
/// deterministic (it mirrors the visible "Last opened" column). A project
/// never opened has no `last_opened_at:` line at all, so insert one then.
fn set_project_last_opened_at(data_root: &std::path::Path, stamp: &str) {
    let file = data_root.join("project.yaml");
    let raw = std::fs::read_to_string(&file).expect("project.yaml");
    let mut replaced = false;
    let mut lines: Vec<String> = raw
        .lines()
        .map(|line| {
            if line.starts_with("last_opened_at:") {
                replaced = true;
                format!("last_opened_at: '{stamp}'")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !replaced {
        lines.push(format!("last_opened_at: '{stamp}'"));
    }
    std::fs::write(&file, format!("{}\n", lines.join("\n"))).expect("rewrite project.yaml");
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
fn projects_screen_ticks_retry_deadlines_on_registered_boards() {
    let store_dir = tempfile::tempdir().expect("store");
    let work = tempfile::tempdir().expect("work");
    let store = ProjectStore::at(store_dir.path());
    let project = store
        .add(work.path(), Some("Background Board"))
        .expect("add project")
        .project;
    Storage::new(&project.data_root)
        .init_board()
        .expect("init board");
    std::fs::write(
        project.data_root.join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: true\norchestration:\n  queue_enabled: true\n  max_running_total: 1\n  auto_restart:\n    enabled: true\n    delays_minutes: [1]\nagents:\n  opencode:\n    command: /nonexistent/opencode-disabled-for-tests\n",
    )
    .expect("queue config");

    let ops = Operations::for_project(&project);
    let occupier = ops
        .create_task(NewTask::titled("Occupy the only slot"))
        .expect("occupier");
    let mut occupier = ops.get_task(&occupier.id).unwrap().unwrap();
    occupier.status = TaskStatus::InProgress;
    occupier.run_phase = Some(RunPhase::Execute);
    occupier.session = Some("ses-occupier".to_string());
    ops.storage.save_task(&occupier).expect("save occupier");
    SessionManager::new(&project.data_root)
        .link_named_session(&occupier.id, "ses-occupier", "Occupier")
        .expect("occupier session");

    let retry = ops
        .create_task(NewTask::titled("Retry while another board is visible"))
        .expect("retry task");
    let mut retry = ops.get_task(&retry.id).unwrap().unwrap();
    retry.status = TaskStatus::InProgress;
    retry.run_phase = Some(RunPhase::Queued);
    retry.restart_at = Some(crate::core::timefmt::now() - chrono::Duration::minutes(1));
    ops.storage.save_task(&retry).expect("save retry task");

    let mut app = App::projects_at(store, None, None).expect("projects app");
    app.tick().expect("global TUI tick");

    let retried = ops.get_task(&retry.id).unwrap().unwrap();
    assert_eq!(retried.crash_restarts, 1);
    assert_eq!(retried.restart_at, None);
    assert_eq!(retried.run_phase, Some(RunPhase::Queued));
    assert!(
        retried.session.is_none(),
        "the occupied slot keeps the retry queued"
    );
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

/// The `o` hotkey hands the selected project's work folder to the
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
    assert!(rendered.contains("o folder"), "{rendered}");
    // The status bar is informational: the hint is not itself clickable.
    assert!(
        !app.hitboxes
            .iter()
            .any(|hitbox| hitbox.action == HitAction::Action(UiAction::OpenProjectFolder))
    );

    app.handle_key(key(KeyCode::Char('o')))
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
fn project_row_shows_paused_tasks_next_to_running_agents() {
    let work = std::path::PathBuf::from("/tmp/k4ai-status-paused");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("work dir");
    let store_dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(store_dir.path());
    let added = store.add(&work, Some("Demo Board")).expect("add project");
    write_task_file(&added.project.data_root, "in_progress", "TASK-001");
    write_task_file_with_flags(
        &added.project.data_root,
        "in_progress",
        "TASK-002",
        "run_phase: queued\n",
    );
    write_active_session(&added.project.data_root, "ses-running");

    let mut app = App::projects_at(store, None, None).expect("projects app");
    let counts = &app.projects[0].counts;
    assert_eq!(counts.sessions, 1);
    assert_eq!(counts.paused, 1);

    let rendered = render_at(&mut app, 96, 12);
    assert!(rendered.contains("▶1 ⏸1"), "{rendered}");
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
    app.handle_key(key(KeyCode::Tab))
        .expect("focus updates checkbox");
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
fn projects_screen_smart_sort_orders_tiers_by_last_opened() {
    // A dedicated store so the recency stage is exercised inside one tier:
    // Beta and Alpha are both quiet, and their last opened order
    // deliberately contradicts their created_at order.
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
    set_project_last_opened_at(&alpha.project.data_root, "2026-09-03T09:10:00");
    set_project_last_opened_at(&beta.project.data_root, "2026-09-01T21:37:00");
    write_task_file_with_flags(
        &gamma.project.data_root,
        "review",
        "TASK-001",
        "review_unseen: true\n",
    );
    let mut app = App::projects_at(store, None, None).expect("projects app");
    app.settings.project_sort = "smart".to_string();
    // Unread Gamma first; the quiet tier follows Last opened (Alpha Sep 3
    // before Beta Sep 1) even though Beta is the newer registration.
    assert_eq!(visible_project_names(&app), ["Gamma", "Alpha", "Beta"]);
}

#[test]
fn projects_screen_smart_name_sort_orders_tiers_by_display_name() {
    let (store_dir, store) = sorted_projects_store();
    // A fourth, newest-created project with no unread work and no agents:
    // under `smart` it would lead the quiet tier, under `smart_name` it must
    // come after Alpha because the stage is the display name.
    let zulu_work = tempfile::tempdir().expect("zulu work");
    let zulu = store.add(zulu_work.path(), Some("Zulu")).expect("add zulu");
    set_project_created_at(&zulu.project.data_root, "2026-04-01T10:00:00");
    let mut app = App::projects_at(store, None, None).expect("projects app");
    app.settings.project_sort = "smart_name".to_string();
    // Unread Gamma, running Beta, then the quiet tier alphabetical: Alpha
    // (created Jan) before Zulu (created Apr) — recency is irrelevant here.
    assert_eq!(
        visible_project_names(&app),
        ["Gamma", "Beta", "Alpha", "Zulu"]
    );

    // Renaming a quiet row reorders its tier, proving the stage is name.
    let store = ProjectStore::at(store_dir.path());
    let zulu = store
        .list()
        .expect("list")
        .into_iter()
        .find(|p| p.name == "Zulu")
        .expect("zulu");
    store.rename(&zulu.id, "Aardvark").expect("rename zulu");
    let mut app = App::projects_at(store, None, None).expect("projects app");
    app.settings.project_sort = "smart_name".to_string();
    assert_eq!(
        visible_project_names(&app),
        ["Gamma", "Beta", "Aardvark", "Alpha"]
    );
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
    app.handle_key(key(KeyCode::Tab))
        .expect("focus updates checkbox");
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
        rolling: false,
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
    // codex reports whatever its last observation said, so the row carries
    // that reading's age alongside the window.
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
fn limits_row_draws_the_yolo_rolling_day_without_a_reset_time() {
    use crate::core::limits::{LimitWindow, LimitsSnapshot, ProviderLimits, ProviderState};

    let (_dir, mut app) = populated_app();
    app.limits = Some(std::sync::Arc::new(LimitsSnapshot {
        fetched_at: chrono::Utc::now().timestamp(),
        providers: vec![ProviderLimits {
            provider: "yolo".to_string(),
            state: ProviderState::Ready,
            windows: vec![LimitWindow {
                label: "24h".to_string(),
                remaining_percent: 94.0,
                resets_at: None,
                rolling: true,
            }],
            observed_at: None,
        }],
    }));

    let lines = rendered_lines(&mut app, 120, 28);
    let row = &lines[lines.len() - 2];

    // A rolling budget has no rollover instant, so the segment is percent-only.
    assert!(row.contains("◉ yolo 24h 94%"), "{row}");
    assert!(!row.contains('↻'), "{row}");
}

#[test]
fn limits_row_drops_reset_times_then_names_as_the_terminal_narrows() {
    let (_dir, mut app) = populated_app();
    app.limits = Some(limits_fixture());

    // Full needs 95 columns, NoReset 67, Percent 32 (22 without grok, 12 for
    // claude alone) — 70 keeps the NoReset rung and 18 forces the row to drop
    // providers from the right.
    let medium = rendered_lines(&mut app, 70, 20);
    let medium_row = medium[medium.len() - 2].clone();
    let narrow = rendered_lines(&mut app, 18, 20);
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
    assert!(!narrow_row.contains('✺'), "{narrow_row}");
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
        rolling: false,
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
    // Every codex window has rolled over, so the segment says so instead of
    // showing a percentage for a period that is finished.
    assert!(row.contains("✺ codex stale"), "{row}");
    assert!(!row.contains("40%"), "{row}");
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
fn limits_row_registers_refresh_hitboxes_on_every_provider() {
    use crate::core::limits::{LimitWindow, LimitsSnapshot, ProviderLimits, ProviderState};

    let (_dir, mut app) = populated_app();
    let now = chrono::Utc::now().timestamp();
    let window = |label: &str, remaining: f64| LimitWindow {
        label: label.to_string(),
        remaining_percent: remaining,
        resets_at: Some(now + 86_400),
        rolling: false,
    };
    app.limits = Some(std::sync::Arc::new(LimitsSnapshot {
        fetched_at: now,
        providers: crate::core::limits::PROVIDERS
            .map(|provider| ProviderLimits {
                provider: provider.to_string(),
                state: ProviderState::Ready,
                windows: vec![window("5h", 66.0)],
                observed_at: None,
            })
            .into(),
    }));

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
    // Each hitbox covers its provider's own text on the rendered row.
    let covers = |hitbox: &super::app::Hitbox, text: &str| {
        let byte = row_text.find(text).expect("provider text");
        let column = unicode_width::UnicodeWidthStr::width(&row_text[..byte]) as u16;
        hitbox.area.x <= column && column < hitbox.area.x + hitbox.area.width
    };
    for provider in crate::core::limits::PROVIDERS {
        let hit = refresh_hit(provider).unwrap_or_else(|| panic!("{provider} hitbox"));
        assert!(covers(hit, provider), "{hit:?} vs {row_text}");
    }
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

/// Tab six times from the Title field lands on Chain to in the task form.
fn open_new_task_on_chain(app: &mut App) {
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    for _ in 0..3 {
        app.handle_key(key(KeyCode::Tab)).expect("tab");
    }
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::ChainTo
    );
}

fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_key(key(KeyCode::Char(character))).expect("type");
    }
}

#[test]
fn chain_filter_narrows_options_and_keeps_selection_on_a_match() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.visible_options(DialogField::ChainTo).len(), 3);

    type_text(&mut app, "question");
    let modal = app.modal.as_ref().expect("modal");
    let visible = modal.visible_options(DialogField::ChainTo);
    assert_eq!(visible.len(), 1, "only the question card matches");
    assert_eq!(modal.chain_selected, visible[0]);
    assert_eq!(modal.chain_text().as_deref(), Some("TASK-001"));

    // Backspacing back to nothing keeps every option available again.
    for _ in 0.."question".len() {
        app.handle_key(key(KeyCode::Backspace)).expect("backspace");
    }
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.chain_filter, "");
    assert_eq!(modal.visible_options(DialogField::ChainTo).len(), 3);
}

#[test]
fn chain_filter_matches_the_default_entry_too() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    type_text(&mut app, "no ch");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.visible_options(DialogField::ChainTo), vec![0]);
    assert_eq!(modal.chain_text(), None, "\"No chain\" carries no value");
}

#[test]
fn enter_on_a_single_filter_match_selects_it_and_advances() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    type_text(&mut app, "implement");
    app.handle_key(key(KeyCode::Enter)).expect("enter");

    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.chain_text().as_deref(), Some("TASK-002"));
    assert_eq!(
        modal.active_field(),
        DialogField::UseOrchestrator,
        "Enter moves on like Tab"
    );
    assert_eq!(modal.filter_error, None);
}

#[test]
fn enter_without_filter_matches_marks_the_section_and_holds_focus() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    let before = app.modal.as_ref().expect("modal").chain_text();
    type_text(&mut app, "zzz");
    app.handle_key(key(KeyCode::Enter)).expect("enter");

    let modal = app.modal.as_ref().expect("modal");
    assert!(modal.visible_options(DialogField::ChainTo).is_empty());
    assert_eq!(modal.filter_error, Some(DialogField::ChainTo));
    assert_eq!(
        modal.active_field(),
        DialogField::ChainTo,
        "focus stays put"
    );
    assert_eq!(modal.chain_text(), before, "nothing was selected");

    // Editing the filter clears the error colouring again.
    app.handle_key(key(KeyCode::Backspace)).expect("backspace");
    assert_eq!(app.modal.as_ref().expect("modal").filter_error, None);
}

#[test]
fn enter_walks_past_a_selector_that_has_no_options_at_all() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    // An empty list is not a filter miss, so Enter must not trap focus here.
    app.modal.as_mut().expect("modal").set_chain_options(vec![]);
    app.handle_key(key(KeyCode::Enter)).expect("enter");

    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.filter_error, None);
    assert_ne!(modal.active_field(), DialogField::ChainTo, "focus moved on");
}

#[test]
fn selecting_an_option_clears_the_filter_error() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    type_text(&mut app, "zzz");
    app.handle_key(key(KeyCode::Enter)).expect("enter");
    assert_eq!(
        app.modal.as_ref().expect("modal").filter_error,
        Some(DialogField::ChainTo)
    );

    app.modal
        .as_mut()
        .expect("modal")
        .select_option(DialogField::ChainTo, 1);
    assert_eq!(app.modal.as_ref().expect("modal").filter_error, None);
}

#[test]
fn leaving_a_selector_clears_its_filter() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    type_text(&mut app, "implement");
    let selected = app.modal.as_ref().expect("modal").chain_selected;
    assert_eq!(
        app.modal
            .as_ref()
            .expect("modal")
            .visible_options(DialogField::ChainTo)
            .len(),
        1
    );

    app.handle_key(key(KeyCode::Tab)).expect("tab away");
    app.handle_key(key(KeyCode::BackTab)).expect("tab back");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.active_field(), DialogField::ChainTo);
    assert_eq!(
        modal.chain_filter, "",
        "the filter does not outlive a visit"
    );
    assert_eq!(modal.visible_options(DialogField::ChainTo).len(), 3);
    assert_eq!(modal.chain_selected, selected, "the pick itself survives");
}

#[test]
fn leaving_a_selector_clears_the_filter_error_too() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    type_text(&mut app, "zzz");
    app.handle_key(key(KeyCode::Enter)).expect("enter");
    assert_eq!(
        app.modal.as_ref().expect("modal").filter_error,
        Some(DialogField::ChainTo)
    );

    app.handle_key(key(KeyCode::Tab)).expect("tab away");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.filter_error, None);
    assert_eq!(modal.chain_filter, "");
}

#[test]
fn backend_filters_like_the_other_long_selectors() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
    let count = app.modal.as_ref().expect("modal").backend_options.len();
    assert!(count > 1, "fixture backends: {count}");

    type_text(&mut app, "default");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.visible_options(DialogField::Backend), vec![0]);

    app.handle_key(key(KeyCode::Enter)).expect("enter");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.backend_selected, 0);
    assert_eq!(modal.active_field(), DialogField::Model, "Enter moves on");
    assert_eq!(modal.backend_filter, "");
}

#[test]
fn short_selectors_take_typed_characters_as_no_filter() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
    for field in [DialogField::Effort, DialogField::Agent] {
        app.modal.as_mut().expect("modal").focus_field(field);
        let before = app.modal.as_ref().expect("modal").visible_options(field);
        type_text(&mut app, "zz");
        let modal = app.modal.as_ref().expect("modal");
        assert_eq!(modal.field_filter(field), None, "{field:?} has no filter");
        assert_eq!(modal.visible_options(field), before);
    }
}

#[test]
fn filtered_selector_click_resolves_to_the_unfiltered_option_index() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    type_text(&mut app, "task-");
    let _ = render_at(&mut app, 100, 60);
    let expected = app.modal.as_ref().expect("modal").chain_options[2]
        .value
        .clone();

    let hit = modal_hitbox(
        &app,
        HitAction::ModalOption {
            field: DialogField::ChainTo,
            index: 2,
        },
    );
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.x,
        row: hit.y,
        modifiers: KeyModifiers::NONE,
    })
    .expect("click option");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.chain_selected, 2);
    assert_eq!(modal.chain_text(), expected);
    assert!(
        !app.hitboxes.iter().any(|hitbox| hitbox.action
            == HitAction::ModalOption {
                field: DialogField::ChainTo,
                index: 0,
            }),
        "the filtered-out \"No chain\" row is not clickable"
    );
}

#[test]
fn title_enter_walks_on_and_description_enter_writes_a_newline() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    type_text(&mut app, "Title text");
    app.handle_key(key(KeyCode::Enter)).expect("enter on title");
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::Description
    );
    assert_eq!(
        app.modal.as_ref().expect("modal").title_text(),
        "Title text"
    );

    type_text(&mut app, "first");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
        .expect("shift enter");
    type_text(&mut app, "second");
    app.handle_key(key(KeyCode::Enter)).expect("plain enter");
    type_text(&mut app, "third");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
        .expect("alt enter");
    type_text(&mut app, "fourth");
    assert_eq!(
        app.modal.as_ref().expect("modal").description_text(),
        "first\nsecond\nthird\nfourth"
    );
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::Description
    );

    app.handle_key(key(KeyCode::Tab)).expect("tab on body");
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::AgentSettings
    );
}

#[test]
fn new_task_bot_toggles_save_on_the_task() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("modal")
        .title
        .insert_str("Per-task bots");
    for _ in 0..4 {
        app.handle_key(key(KeyCode::Tab)).expect("tab");
    }
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::UseOrchestrator
    );
    app.handle_key(key(KeyCode::Char(' '))).expect("space");
    app.handle_key(key(KeyCode::Enter)).expect("enter");
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::UseDesigner
    );
    app.handle_key(key(KeyCode::Char(' '))).expect("space");
    app.handle_key(key(KeyCode::Enter)).expect("enter");
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::UseReviewer
    );
    app.handle_key(key(KeyCode::Char(' '))).expect("space");
    app.handle_key(key(KeyCode::Enter)).expect("enter");
    assert_eq!(
        app.modal.as_ref().expect("modal").active_field(),
        DialogField::Confirm
    );
    app.handle_key(key(KeyCode::Enter)).expect("save");

    let created = app
        .ops
        .list_tasks(None, Some("Per-task bots"), "created", "asc")
        .unwrap()
        .into_iter()
        .find(|task| task.title == "Per-task bots")
        .expect("created task");
    assert!(created.use_orchestrator);
    assert!(created.use_designer);
    assert!(created.use_reviewer);
}

#[test]
fn enter_still_submits_and_cancels_from_the_form_buttons() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    type_text(&mut app, "Enter submits");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::Confirm);
    app.handle_key(key(KeyCode::Enter)).expect("enter");
    assert!(app.modal.is_none(), "Save button still submits on Enter");
    assert!(
        app.board
            .columns
            .iter()
            .flat_map(|column| column.tasks.iter())
            .any(|task| task.title == "Enter submits")
    );
}

#[test]
fn model_filter_survives_a_catalog_refresh_and_stays_on_a_visible_option() {
    let (_dir, mut app) = populated_app();
    app.handle_key(key(KeyCode::Char('n'))).expect("new task");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::AgentSettings);
    app.handle_key(key(KeyCode::Enter)).expect("open popup");
    app.modal
        .as_mut()
        .expect("modal")
        .focus_field(DialogField::Model);
    let modal = app.modal.as_mut().expect("modal");
    modal.set_model_options(vec![
        SelectOption {
            label: "Default model".to_string(),
            value: None,
        },
        SelectOption {
            label: "sonnet".to_string(),
            value: Some("sonnet".to_string()),
        },
        SelectOption {
            label: "opus".to_string(),
            value: Some("opus".to_string()),
        },
    ]);
    type_text(&mut app, "opus");
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.model_text().as_deref(), Some("opus"));

    // A warmed catalog replaces the list; the typed filter must still hold.
    let modal = app.modal.as_mut().expect("modal");
    modal.set_model_options(vec![
        SelectOption {
            label: "Default model".to_string(),
            value: None,
        },
        SelectOption {
            label: "opus-4".to_string(),
            value: Some("opus-4".to_string()),
        },
    ]);
    let modal = app.modal.as_ref().expect("modal");
    assert_eq!(modal.model_filter, "opus");
    assert_eq!(modal.visible_options(DialogField::Model), vec![1]);
    assert_eq!(modal.model_text().as_deref(), Some("opus-4"));
}

#[test]
fn chain_filter_renders_matches_and_the_empty_filter_error() {
    let (_dir, mut app) = populated_app();
    open_new_task_on_chain(&mut app);
    type_text(&mut app, "task-00");
    insta::assert_snapshot!("chain_filter_matches", render_at(&mut app, 80, 24));

    type_text(&mut app, "9");
    app.handle_key(key(KeyCode::Enter)).expect("enter");
    insta::assert_snapshot!("chain_filter_no_matches", render_at(&mut app, 80, 24));
}

// ------------------------------------------------------- queued run phase

#[test]
fn queued_design_and_review_phases_render_phase_badges() {
    let (_dir, mut app) = app_with_board();
    let mut queued = app.ops.create_task(NewTask::titled("Queued task")).unwrap();
    queued.run_phase = Some(RunPhase::Queued);
    app.ops.storage.save_task(&queued).unwrap();
    let mut design = app.ops.create_task(NewTask::titled("Design task")).unwrap();
    design.run_phase = Some(RunPhase::Design);
    app.ops.storage.save_task(&design).unwrap();
    let mut review = app
        .ops
        .create_task(NewTask::titled("Bot review task"))
        .unwrap();
    review.run_phase = Some(RunPhase::Review);
    app.ops.storage.save_task(&review).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let output = render_at(&mut app, 120, 30);
    assert!(output.contains("⏸ queued"), "queued badge missing");
    assert!(output.contains("✎ design"), "design badge missing");
    assert!(output.contains("⚖ review"), "review badge missing");
    insta::assert_snapshot!("phase_queued_badges", output);
}

#[test]
fn a_woken_pause_renders_the_queued_badge() {
    let (dir, mut app) = app_with_board();
    // The state a paused task is left in when its wait ends: the old session
    // is closed and cleared, and the task parks in the queue.
    let mut paused = app.ops.create_task(NewTask::titled("Woken pause")).unwrap();
    SessionManager::new(dir.path())
        .link_session(&paused.id, "ses-paused-1")
        .unwrap();
    paused.status = TaskStatus::InProgress;
    paused.run_phase = Some(RunPhase::Queued);
    paused.session = None;
    app.ops.storage.save_task(&paused).unwrap();
    SessionManager::new(dir.path())
        .close_session("ses-paused-1")
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let output = render_at(&mut app, 120, 24);
    assert!(
        output.contains("⏸ queued"),
        "a woken pause must wear the queued badge:\n{output}"
    );
    assert!(
        !output.contains("✖ crashed"),
        "a parked task owns no session and must not read crashed:\n{output}"
    );
    insta::assert_snapshot!("paused_woken_to_queued_card", output);
}

#[test]
fn live_session_shows_running_even_if_phase_is_still_queued() {
    let (dir, mut app) = app_with_board();
    let mut running = app
        .ops
        .create_task(NewTask::titled("Queued but live"))
        .unwrap();
    SessionManager::new(dir.path())
        .link_session(&running.id, "ses-q-live")
        .unwrap();
    running.session = Some("ses-q-live".to_string());
    running.status = TaskStatus::InProgress;
    running.run_phase = Some(RunPhase::Queued);
    app.ops.storage.save_task(&running).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let output = render_at(&mut app, 120, 24);
    assert!(
        output.contains("▶ running"),
        "a live session must not wear the queued badge:\n{output}"
    );
    assert!(
        !output.contains("⏸ queued"),
        "queued is waiting-for-a-slot, not a live overlay:\n{output}"
    );
    assert!(!output.contains("✖ crashed"));
}

#[test]
fn live_design_session_shows_the_design_badge() {
    let (dir, mut app) = app_with_board();
    let mut design = app
        .ops
        .create_task(NewTask::titled("Designing now"))
        .unwrap();
    SessionManager::new(dir.path())
        .link_session(&design.id, "ses-design-live")
        .unwrap();
    design.session = Some("ses-design-live".to_string());
    design.status = TaskStatus::InProgress;
    design.run_phase = Some(RunPhase::Design);
    app.ops.storage.save_task(&design).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let output = render_at(&mut app, 120, 24);
    assert!(
        output.contains("✎ design"),
        "design badge missing:\n{output}"
    );
    // TASK-300: a live design session now also gets the role-colored
    // "▶ running" row under the phase badge (the badge keeps naming the phase).
    assert!(
        output.contains("▶ running"),
        "design phase must still show the running row:\n{output}"
    );
}

#[test]
fn detail_meta_shows_the_run_phase() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Detail phase"))
        .unwrap();
    app.ops.enqueue_task(&task.id).unwrap().unwrap();
    let queued = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(queued.run_phase, Some(RunPhase::Queued));
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    // The enqueued card sits in the In Progress column now.
    assert_eq!(app.board.columns[1].tasks.len(), 1);
    app.focused_column = 1;
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    assert_eq!(app.screen, Screen::Detail);

    let output = render_at(&mut app, 100, 40);
    assert!(
        output.contains("Status: in_progress · queued"),
        "status line with phase missing"
    );
    insta::assert_snapshot!("phase_queued_detail_meta", output);
}

#[test]
fn q_toggles_enqueue_and_dequeue() {
    let (_dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Toggle me")).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    // Enqueue: To Do moves to In Progress queued, nothing launches.
    app.handle_key(key(KeyCode::Char('Q'))).unwrap();
    let current = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(current.status, TaskStatus::InProgress);
    assert_eq!(current.run_phase, Some(RunPhase::Queued));
    assert_eq!(
        app.status,
        "Queued TASK-001 — the dispatcher starts it when a slot frees"
    );

    // Dequeue: back to a plain idle In Progress task for manual `r`. The
    // card jumped to the In Progress column, so follow it before pressing Q.
    assert_eq!(app.board.columns[1].tasks.len(), 1);
    app.focused_column = 1;
    app.focused_card = 0;
    app.handle_key(key(KeyCode::Char('Q'))).unwrap();
    let current = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(current.run_phase, None);
    assert_eq!(current.status, TaskStatus::InProgress);
}

#[test]
fn q_does_nothing_where_the_queue_has_no_meaning() {
    let (_dir, mut app) = app_with_board();
    let task = app.ops.create_task(NewTask::titled("Finished")).unwrap();
    app.ops.move_task(&task.id, "done", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    // Follow the card into the Done column.
    app.focused_column = 3;
    app.focused_card = 0;
    let before = app.status.clone();

    assert_eq!(
        app.queue_action_for(&app.current_task().unwrap()),
        None,
        "the hint bar offers nothing here, so the key must do nothing"
    );
    app.handle_key(key(KeyCode::Char('Q'))).unwrap();

    let current = app.ops.get_task(&task.id).unwrap().unwrap();
    assert_eq!(current.status, TaskStatus::Done);
    assert_eq!(current.run_phase, None);
    assert_eq!(app.status, before, "no error banner from an unbound key");
}

#[test]
fn chain_and_interactive_badges_follow_the_column() {
    let (_dir, mut app) = app_with_board();
    let make = |title: &str| {
        app.ops
            .create_task(NewTask {
                title: title.to_string(),
                interactive: true,
                chained_to: Some("TASK-154".to_string()),
                ..Default::default()
            })
            .unwrap()
    };
    let todo = make("Chained starter");
    let running = make("Chained running");
    app.ops
        .move_task(&running.id, "in_progress", false)
        .unwrap();
    let review = make("Chained review");
    app.ops.move_task(&review.id, "review", false).unwrap();
    let done = make("Chained done");
    app.ops.move_task(&done.id, "done", false).unwrap();
    let archived = make("Chained archived");
    app.ops.move_task(&archived.id, "archive", false).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let badge_labels = |app: &App, id: &str| {
        let task = app.ops.get_task(id).unwrap().unwrap();
        super::card::badges(&task, None, app)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>()
    };

    let todo_badges = badge_labels(&app, &todo.id);
    assert!(
        todo_badges.iter().any(|label| label == "☑ interactive"),
        "interactive badge missing on To Do: {todo_badges:?}"
    );
    assert!(
        todo_badges.iter().any(|label| label == "↪ chain -> 154"),
        "chain badge must name its target on To Do: {todo_badges:?}"
    );

    let running_badges = badge_labels(&app, &running.id);
    assert!(
        running_badges.iter().any(|label| label == "☑ interactive"),
        "interactive badge missing on In Progress: {running_badges:?}"
    );
    assert!(
        !running_badges.iter().any(|label| label.contains("chain")),
        "chain badge must hide on In Progress: {running_badges:?}"
    );

    for id in [&review.id, &done.id, &archived.id] {
        let labels = badge_labels(&app, id);
        assert!(
            !labels
                .iter()
                .any(|label| label.contains("interactive") || label.contains("chain")),
            "chain/interactive badges must hide past In Progress: {labels:?}"
        );
    }

    let output = render_at(&mut app, 200, 30);
    assert!(
        output.contains("↪ chain -> 154"),
        "chain target missing on the board:\n{output}"
    );
}

#[test]
fn graph_badges_show_planning_and_waiting_nodes() {
    let (_dir, mut app) = app_with_board();
    let planner = app
        .ops
        .create_task(NewTask {
            title: "Big feature".to_string(),
            use_orchestrator: true,
            ..Default::default()
        })
        .unwrap();
    let node = app
        .ops
        .create_task(NewTask {
            title: "Planned node".to_string(),
            depends_on: vec![planner.id.clone()],
            ..Default::default()
        })
        .unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let badge_labels = |app: &App, id: &str| {
        let task = app.ops.get_task(id).unwrap().unwrap();
        super::card::badges(&task, None, app)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>()
    };

    let planner_badges = badge_labels(&app, &planner.id);
    assert!(
        planner_badges.iter().any(|label| label == "◧ plan"),
        "a task that still owes a plan is marked: {planner_badges:?}"
    );
    let node_badges = badge_labels(&app, &node.id);
    assert!(
        node_badges.iter().any(|label| label == "⇢ after 1"),
        "a waiting node names how many edges hold it: {node_badges:?}"
    );

    // Once the plan is in, the planner is the join node instead.
    let mut planned = app.ops.get_task(&planner.id).unwrap().unwrap();
    planned.orchestrated = true;
    planned.depends_on = vec![node.id.clone()];
    app.ops.storage.save_task(&planned).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    let planner_badges = badge_labels(&app, &planner.id);
    assert!(
        planner_badges.iter().any(|label| label == "◧ joins 1"),
        "the join node is distinguished from a plain waiting node: {planner_badges:?}"
    );
    assert!(
        !planner_badges.iter().any(|label| label == "◧ plan"),
        "a planned task does not still advertise a pending plan: {planner_badges:?}"
    );

    let output = render_at(&mut app, 200, 30);
    assert!(output.contains("⇢ after 1"), "board:\n{output}");
}

#[test]
fn design_and_review_marks_last_until_the_stage_completes() {
    let dir = tempfile::tempdir().expect("tempdir");
    Storage::new(dir.path()).init_board().expect("init board");
    std::fs::write(
        dir.path().join(".kanban/config.yaml"),
        "notifications:\n  enabled: false\nauto_launch:\n  enabled: false\nagents:\n  opencode:\n    command: /nonexistent/opencode-disabled-for-tests\norchestration:\n  designer:\n    enabled: true\n  reviewer:\n    enabled: true\n",
    )
    .expect("quiet config");
    let app = App::new(dir.path()).expect("create app");

    let labels = |app: &App, id: &str| {
        let task = app.ops.get_task(id).unwrap().unwrap();
        super::card::badges(&task, None, app)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>()
    };

    // Project-wide bots: a fresh To Do task carries both pending marks.
    let task = app.ops.create_task(NewTask::titled("Pipelined")).unwrap();
    let todo = labels(&app, &task.id);
    assert!(
        todo.iter().any(|label| label == "✎ design"),
        "design mark missing on To Do: {todo:?}"
    );
    assert!(
        todo.iter().any(|label| label == "⚖ review"),
        "review mark missing on To Do: {todo:?}"
    );

    // The design stage completed (`designed`) → design mark drops, review
    // stays pending.
    let mut done_designing = app.ops.get_task(&task.id).unwrap().unwrap();
    done_designing.designed = true;
    app.ops.storage.save_task(&done_designing).unwrap();
    let mid = labels(&app, &task.id);
    assert!(
        !mid.iter().any(|label| label == "✎ design"),
        "design mark must drop once designed: {mid:?}"
    );
    assert!(
        mid.iter().any(|label| label == "⚖ review"),
        "review mark must stay until bot review completes: {mid:?}"
    );

    // While a stage's bot actually runs (phase set, no live session yet) the
    // active phase badge shows it and no duplicate mark is added.
    let mut designing = app.ops.get_task(&task.id).unwrap().unwrap();
    designing.designed = false;
    designing.run_phase = Some(RunPhase::Design);
    app.ops.storage.save_task(&designing).unwrap();
    let active = labels(&app, &task.id);
    assert_eq!(
        active.iter().filter(|label| **label == "✎ design").count(),
        1,
        "active design badge must not be duplicated: {active:?}"
    );

    // Review landed: past In Progress neither mark survives.
    app.ops.move_task(&task.id, "review", false).unwrap();
    let review = labels(&app, &task.id);
    assert!(
        !review
            .iter()
            .any(|label| label == "✎ design" || label == "⚖ review"),
        "stage marks must hide past In Progress: {review:?}"
    );
}

#[test]
fn per_task_bot_opt_in_shows_marks_with_bots_off() {
    let (_dir, app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask {
            title: "Opted in".to_string(),
            use_designer: true,
            use_reviewer: true,
            ..Default::default()
        })
        .unwrap();
    let labels = |app: &App, id: &str| {
        let task = app.ops.get_task(id).unwrap().unwrap();
        super::card::badges(&task, None, app)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>()
    };
    let marks = labels(&app, &task.id);
    assert!(
        marks.iter().any(|label| label == "✎ design"),
        "per-task designer opt-in must show the design mark: {marks:?}"
    );
    assert!(
        marks.iter().any(|label| label == "⚖ review"),
        "per-task reviewer opt-in must show the review mark: {marks:?}"
    );

    // A plain task on the same board (bots off) carries neither mark.
    let plain = app.ops.create_task(NewTask::titled("No bots")).unwrap();
    let plain_marks = labels(&app, &plain.id);
    assert!(
        !plain_marks
            .iter()
            .any(|label| label == "✎ design" || label == "⚖ review"),
        "marks must require the bot or the opt-in: {plain_marks:?}"
    );
}

/// Hold while seeding, polling, or rendering against the process-wide update
/// cache, so parallel tests cannot wipe a seeded status or see it mid-test.
fn update_cache_guard() -> std::sync::MutexGuard<'static, ()> {
    crate::core::update::CACHE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn force_update_cache(version: Option<&str>) {
    let status = version.map(|version| {
        std::sync::Arc::new(crate::core::update::UpdateStatus {
            checked_at: 1_787_000_000,
            latest_version: version.to_string(),
            tag: format!("v{version}"),
            asset_url: Some("https://example.com/kanban4ai.tar.gz".to_string()),
            checksum_url: None,
            notes_url: "https://example.com/notes".to_string(),
            published_at: Some(chrono::Utc::now().timestamp() - 20 * 24 * 3600),
            dismissed_version: None,
        })
    });
    crate::core::update::force_cache(status);
}

fn update_banner(version: &str) -> String {
    format!("↑ kanban4ai {version} available - open Settings to update")
}

fn global_settings_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().expect("store");
    let store = ProjectStore::at(dir.path());
    let app = App::projects_at(store, None, None).expect("projects app");
    (dir, app)
}

fn open_global_settings(app: &mut App) {
    app.handle_key(key(KeyCode::Char('s')))
        .expect("global settings");
    assert!(
        matches!(
            app.modal.as_ref().map(|modal| &modal.modal),
            Some(Modal::GlobalSettings)
        ),
        "s must open the global settings dialog"
    );
}

#[test]
fn update_banner_shows_once_persists_dismissal_and_reopens_on_newer() {
    let _cache = update_cache_guard();
    force_update_cache(Some("9.9.9"));
    let (_dir, mut app) = app_with_board();
    app.poll_update_banner();
    assert_eq!(app.status, update_banner("9.9.9"));

    // Showing persisted the dismissal in the process-wide cache, so a
    // relaunch (a fresh App here) does not nag again for the same version.
    let (_dir2, mut relaunched) = app_with_board();
    relaunched.poll_update_banner();
    assert_eq!(relaunched.status, "TUI ready");

    // A newer tag reopens the banner.
    force_update_cache(Some("9.10.0"));
    relaunched.poll_update_banner();
    assert_eq!(relaunched.status, update_banner("9.10.0"));
    force_update_cache(None);
}

#[test]
fn update_available_banner_snapshot() {
    let _cache = update_cache_guard();
    force_update_cache(Some("9.9.9"));
    let (_dir, mut app) = app_with_board();
    app.poll_update_banner();
    let rendered = render_at(&mut app, 96, 28);
    assert!(
        rendered.contains(&update_banner("9.9.9")),
        "banner missing from the status bar:\n{rendered}"
    );
    insta::assert_snapshot!("update_available", rendered);
    force_update_cache(None);
}

#[test]
fn global_settings_updates_rows_snapshot() {
    let _cache = update_cache_guard();
    force_update_cache(Some("9.9.9"));
    let (_dir, mut app) = global_settings_app();
    open_global_settings(&mut app);
    let rendered = render_at(&mut app, 96, 28);
    assert!(
        rendered.contains("kanban4ai 9.9.9 available (released 20d ago)"),
        "{rendered}"
    );
    assert!(rendered.contains("[ Check now ]"), "{rendered}");
    assert!(rendered.contains("[ Update now ]"), "{rendered}");
    assert!(
        rendered.contains("check for updates when kanban4ai opens"),
        "{rendered}"
    );
    insta::assert_snapshot!("global_settings_updates", rendered);
    force_update_cache(None);
}

#[test]
fn global_settings_up_to_date_row_hides_update_button() {
    let _cache = update_cache_guard();
    force_update_cache(Some(crate::core::update::installed_version()));
    let (_dir, mut app) = global_settings_app();
    open_global_settings(&mut app);
    let rendered = render_at(&mut app, 96, 28);
    assert!(
        rendered.contains(&format!(
            "kanban4ai {} - up to date",
            crate::core::update::installed_version()
        )),
        "{rendered}"
    );
    assert!(!rendered.contains("[ Update now ]"), "{rendered}");
    force_update_cache(None);
}

#[test]
fn global_settings_check_now_failure_shows_reason_row() {
    let _cache = update_cache_guard();
    let (_dir, mut app) = global_settings_app();
    open_global_settings(&mut app);
    // cfg(test) makes check_latest fail without a network request, which is
    // exactly the failure row this test wants to see.
    app.dispatch(UiAction::CheckUpdates).expect("check now");
    assert!(
        app.modal
            .as_ref()
            .expect("dialog stays open")
            .update_check_error
            .is_some(),
        "the failure must reach the dialog state"
    );
    let rendered = render_at(&mut app, 96, 28);
    assert!(rendered.contains("Check failed:"), "{rendered}");
    assert!(!rendered.contains("[ Update now ]"), "{rendered}");
    force_update_cache(None);
}

#[test]
fn global_settings_check_on_open_toggle_persists() {
    let (dir, mut app) = {
        let _cache = update_cache_guard();
        global_settings_app()
    };
    open_global_settings(&mut app);
    {
        let modal = app.modal.as_mut().expect("dialog");
        assert!(modal.update_check_on_open, "default is on");
        modal.focus_field(DialogField::UpdateCheckOnOpen);
        modal.input(key(KeyCode::Char(' ')));
        assert!(!modal.update_check_on_open);
        modal.field_index = modal.fields().len() - 2;
    }
    app.handle_key(key(KeyCode::Enter)).expect("save");
    assert!(app.modal.is_none(), "save closes the dialog");
    let saved = ProjectStore::at(dir.path())
        .load_global_config()
        .expect("reload global config");
    assert!(!saved.update_check_on_open());
}

#[test]
fn isolation_and_conflict_badges_follow_integration_state() {
    let (_dir, mut app) = app_with_board();
    let mut isolated = app
        .ops
        .create_task(NewTask::titled("Isolated work"))
        .unwrap();
    isolated.worktree = Some("TASK-001".to_string());
    isolated.branch = Some("kanban/TASK-001".to_string());
    isolated.base_commit = Some("0123456789abcdef".to_string());
    app.ops.storage.save_task(&isolated).unwrap();

    let mut conflict = app
        .ops
        .create_task(NewTask::titled("Conflicted work"))
        .unwrap();
    conflict.worktree = Some("TASK-002".to_string());
    conflict.branch = Some("kanban/TASK-002".to_string());
    conflict.base_commit = Some("0123456789abcdef".to_string());
    conflict.integration = IntegrationState::Conflict;
    app.ops.storage.save_task(&conflict).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();

    let labels = |id: &str| {
        let task = app.ops.get_task(id).unwrap().unwrap();
        super::card::badges(&task, None, &app)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>()
    };

    let isolated_badges = labels(&isolated.id);
    assert!(
        isolated_badges.iter().any(|label| label == "⑂ worktree"),
        "worktree badge missing: {isolated_badges:?}"
    );
    assert!(
        !isolated_badges
            .iter()
            .any(|label| label.contains("conflict")),
        "plain worktree badge must not claim conflict: {isolated_badges:?}"
    );

    let conflict_badges = labels(&conflict.id);
    assert!(
        conflict_badges.iter().any(|label| label == "⚠ conflict"),
        "conflict badge missing: {conflict_badges:?}"
    );
    assert!(
        !conflict_badges
            .iter()
            .any(|label| label.contains("worktree")),
        "conflict displaces the plain worktree badge: {conflict_badges:?}"
    );

    let output = render_at(&mut app, 120, 30);
    assert!(output.contains("⚠ conflict"), "conflict badge not rendered");
}

#[test]
fn conflict_detail_shows_worktree_branch_base_and_urgent_rerun() {
    let (dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Landing went sideways"))
        .unwrap();
    app.ops.move_task(&task.id, "review", false).unwrap();
    let mut task = app.ops.get_task(&task.id).unwrap().unwrap();
    task.worktree = Some("TASK-001".to_string());
    task.branch = Some("kanban/TASK-001".to_string());
    task.base_commit = Some("0123456789abcdef".to_string());
    task.integration = IntegrationState::Conflict;
    task.review_edits = "conflict in src/main.rs: base abc, ours def, theirs 123".to_string();
    app.ops.storage.save_task(&task).unwrap();
    app.board = super::app::BoardSnapshot::load(&app.ops).unwrap();
    app.focused_column = 2; // Review
    app.focused_card = 0;

    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    let output = render_at(&mut app, 100, 32).replace(&dir.path().display().to_string(), "<board>");
    assert!(output.contains("Worktree: <board>"), "{output}");
    assert!(output.contains("Branch: kanban/TASK-001"), "{output}");
    assert!(output.contains("Base: 0123456"), "{output}");
    assert!(
        output.contains("⚠ Integration conflict"),
        "conflict action line missing: {output}"
    );
    assert!(output.contains("[ Re-run ^R ]"), "{output}");
    insta::assert_snapshot!("isolation_conflict_detail", output);
}

#[test]
fn project_settings_shows_isolation_availability() {
    let (_dir, mut app) = app_with_board();
    app.handle_key(key(KeyCode::Char('s')))
        .expect("open settings");
    {
        let modal = app.modal.as_mut().expect("settings");
        modal.focus_field(DialogField::IsolationStatus);
    }
    let output = render_at(&mut app, 80, 24);
    // The test board's work folder is not a git repository, so the probe has
    // a deterministic answer here.
    assert!(
        output.contains("Worktree isolation:"),
        "isolation row missing: {output}"
    );
    assert!(output.contains("unavailable"), "{output}");
    insta::assert_snapshot!("settings_isolation_status", output);
}

/// The one-row custom answer preview windows its tail so the text cursor
/// stays visible on long answers instead of being truncated away.
#[test]
fn answer_preview_windows_to_keep_the_cursor_visible() {
    let (_dir, mut app) = app_with_board();
    let task = app
        .ops
        .create_task(NewTask::titled("Long answer"))
        .expect("task");
    app.ops
        .ask_question(&task.id, "Only?", "agent", vec![])
        .expect("ask");
    app.board = super::app::BoardSnapshot::load(&app.ops).expect("reload");
    app.handle_key(key(KeyCode::Enter)).expect("open detail");
    app.handle_key(key(KeyCode::Tab)).expect("focus answer");
    // 60 chars: wider than the 50-column panel's 31-column answer area.
    for character in "headmark zzzzzzzzzz zzzzzzzzzz zzzzzzzzzz zzzzzzzzzz tailmark".chars() {
        app.handle_key(key(KeyCode::Char(character)))
            .expect("type answer");
    }
    let rendered = render_at(&mut app, 50, 24);
    assert!(
        rendered.contains("tailmark"),
        "cursor end must stay visible: {rendered}"
    );
    assert!(
        !rendered.contains("headmark"),
        "the overflowed head must be scrolled off: {rendered}"
    );
}
