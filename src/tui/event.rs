use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event as CrosstermEvent, KeyEvent, KeyEventKind};

use crate::core::error::Result;
use crate::core::project::Project;

use super::app::App;
use super::board;
use super::diag;

#[derive(Debug)]
pub enum AppEvent {
    Input(CrosstermEvent),
    /// A paste key press with the clipboard text captured when it arrived.
    PasteShortcut(KeyEvent, Option<String>),
    InputError(String),
    FsChanged,
    FsDebounced(u64),
    Tick,
    /// Read-only tick work completed without blocking terminal input.
    TickReady(Box<super::tick_worker::TickResult>),
    /// A background launch finished; its report is waiting on the app.
    LaunchDone,
}

/// Why the per-project event loop returned. `run` keeps the input/tick threads
/// and starts a new loop for the next target.
#[derive(Debug, Clone)]
pub enum LoopOutcome {
    Quit,
    OpenProject(Project),
    ShowProjects { return_to: Option<Project> },
}

pub struct EventThreads {
    pub tx: Sender<AppEvent>,
    pub rx: Receiver<AppEvent>,
}

pub fn spawn_shared_threads(tick: Duration) -> EventThreads {
    let (tx, rx) = mpsc::channel();
    spawn_input_thread(tx.clone());
    spawn_tick_thread(tick, tx.clone());
    EventThreads { tx, rx }
}

pub fn run_event_loop<B: Backend<Error = std::io::Error>>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    threads: &EventThreads,
    network: &mut crate::core::net::Manager,
) -> Result<LoopOutcome> {
    let _watcher = start_watcher(app.watch_root(), threads.tx.clone());
    app.enable_background_launches(threads.tx.clone());
    let mut window_title = String::new();
    let mut debounce_armed = false;

    refresh_limits(app);
    app.poll_update_banner();
    sync_window_title(app, &mut window_title);
    diag::busy("draw");
    terminal.draw(|frame| board::ui(frame, app))?;
    loop {
        if let Some(outcome) = app.take_loop_outcome() {
            return Ok(outcome);
        }
        diag::idle();
        let event = match threads.rx.recv() {
            Ok(event) => event,
            Err(_) => return Ok(LoopOutcome::Quit),
        };
        diag::busy(event_phase(&event));
        let mut event_timer = diag::PhaseTimer::new(event_phase(&event));
        match event {
            AppEvent::Input(input) => handle_input(app, input)?,
            AppEvent::PasteShortcut(key, clipboard) => app.handle_paste_shortcut(key, clipboard)?,
            AppEvent::InputError(message) => app.stop_after_input_error(message),
            // Nothing visible changes until the debounced reload, so no
            // frame is drawn, and one timer serves a whole burst instead of
            // a thread per event.
            AppEvent::FsChanged => {
                let generation = app.note_fs_changed();
                if !debounce_armed {
                    debounce_armed = true;
                    spawn_debounce_timer(threads.tx.clone(), generation);
                }
                continue;
            }
            AppEvent::FsDebounced(generation) => {
                debounce_armed = false;
                match app.pending_fs_change() {
                    // Changes kept arriving: wait for the burst to settle.
                    Some(current) if current != generation => {
                        debounce_armed = true;
                        spawn_debounce_timer(threads.tx.clone(), current);
                        continue;
                    }
                    _ => app.reload_debounced_change(generation)?,
                }
            }
            AppEvent::LaunchDone => app.poll_launches(),
            AppEvent::TickReady(result) => app.apply_tick_result(*result)?,
            AppEvent::Tick => {
                if let Err(error) = network.tick() {
                    app.status = error.to_string();
                }
                event_timer.mark("network");
                diag::busy("tick: limits");
                refresh_limits(app);
                app.poll_update_banner();
                event_timer.mark("limits_update");
                app.tick()?;
            }
        }
        event_timer.mark("event");
        if let Some(text) = app.take_pending_copy() {
            diag::busy("copy");
            app.finish_copy(super::image::copy_text(&text));
        }
        if app.take_full_redraw() {
            terminal.clear()?;
        }
        if let Some(outcome) = app.take_loop_outcome() {
            return Ok(outcome);
        }
        sync_window_title(app, &mut window_title);
        diag::busy("draw");
        terminal.draw(|frame| board::ui(frame, app))?;
        event_timer.mark("draw");
    }
}

/// Watchdog label for the work one event starts.
fn event_phase(event: &AppEvent) -> &'static str {
    match event {
        AppEvent::Input(CrosstermEvent::Key(_)) => "input: key",
        AppEvent::Input(CrosstermEvent::Mouse(_)) => "input: mouse",
        AppEvent::Input(CrosstermEvent::Paste(_)) => "input: paste",
        AppEvent::Input(_) => "input",
        AppEvent::PasteShortcut(..) => "input: paste shortcut",
        AppEvent::InputError(_) => "input error",
        AppEvent::FsChanged => "fs change",
        AppEvent::FsDebounced(_) => "board reload",
        AppEvent::Tick => "tick: network",
        AppEvent::TickReady(_) => "tick: apply",
        AppEvent::LaunchDone => "launch done",
    }
}

/// Keep the terminal's window title in step with the open project. A rename
/// lands mid-loop, so the title is diffed here rather than re-emitted on every
/// frame — the escape would otherwise fight the multiplexer once a second.
fn sync_window_title(app: &App, current: &mut String) {
    let next = app.window_title();
    if *current != next {
        super::set_window_title(&next);
        *current = next;
    }
}

/// Keep the provider-limits row current. The fetch itself runs on its own
/// thread and only starts when the cached snapshot has aged out, so ticking
/// this every second costs nothing; it lives here rather than in `App::tick`
/// so nothing outside a real terminal session ever polls a provider.
fn refresh_limits(app: &mut App) {
    if !app.settings.show_limits {
        app.limits = None;
        return;
    }
    crate::core::limits::refresh_if_stale(app.settings.limits_refresh_interval);
    app.limits = crate::core::limits::cached();
}

fn handle_input(app: &mut App, input: CrosstermEvent) -> Result<()> {
    match input {
        // The kitty keyboard protocol can report repeats and releases; only
        // presses drive the UI.
        CrosstermEvent::Key(key)
            if matches!(
                key.kind,
                ratatui::crossterm::event::KeyEventKind::Press
                    | ratatui::crossterm::event::KeyEventKind::Repeat
            ) =>
        {
            app.handle_key(key)
        }
        CrosstermEvent::Key(_) => Ok(()),
        CrosstermEvent::Mouse(mouse) => app.handle_mouse(mouse),
        CrosstermEvent::Paste(text) => app.handle_paste(&text),
        CrosstermEvent::Resize(_, _) => Ok(()),
        _ => Ok(()),
    }
}

fn spawn_input_thread(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        loop {
            let input = match event::poll(Duration::from_millis(50)) {
                Ok(true) => event::read(),
                Ok(false) => continue,
                Err(err) => Err(err),
            };
            match input {
                Ok(event) => {
                    // Raw mode swallows SIGINT, so a stuck main loop would
                    // otherwise leave no way out short of killing the shell.
                    if let CrosstermEvent::Key(key) = event
                        && key.kind == KeyEventKind::Press
                        && super::app::is_ctrl_c(key)
                    {
                        diag::ctrl_c_pressed();
                    }
                    if tx.send(input_event(event)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(AppEvent::InputError(format!(
                        "Terminal input failed: {err}"
                    )));
                    break;
                }
            }
        }
    });
}

/// Read the clipboard right when a paste shortcut arrives: dictation tools
/// restore the previous clipboard shortly after sending the key.
fn input_event(event: CrosstermEvent) -> AppEvent {
    match event {
        CrosstermEvent::Key(key)
            if key.kind == KeyEventKind::Press && super::app::is_paste_shortcut(key) =>
        {
            AppEvent::PasteShortcut(key, super::image::clipboard_text())
        }
        event => AppEvent::Input(event),
    }
}

fn spawn_tick_thread(interval: Duration, tx: Sender<AppEvent>) {
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            if tx.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });
}

fn spawn_debounce_timer(tx: Sender<AppEvent>, generation: u64) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        let _ = tx.send(AppEvent::FsDebounced(generation));
    });
}

/// Hold the notify watcher in the calling stack so dropping it unsubscribes.
/// notify already runs its own thread; the old park-forever wrapper leaked one.
fn start_watcher(project_path: Option<&Path>, tx: Sender<AppEvent>) -> Option<RecommendedWatcher> {
    let project_path = project_path?;
    let paths = ["tasks", "threads", "sessions"]
        .into_iter()
        .map(|name| project_path.join(".kanban").join(name))
        .collect::<Vec<_>>();
    let tx_events = tx;
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            if res.is_ok_and(|event| is_board_change(&event.kind)) {
                let _ = tx_events.send(AppEvent::FsChanged);
            }
        },
        Config::default(),
    )
    .ok()?;
    for path in paths.iter().filter(|path| path.exists()) {
        let _ = watcher.watch(path, RecursiveMode::Recursive);
    }
    Some(watcher)
}

fn is_board_change(kind: &notify::EventKind) -> bool {
    kind.is_create() || kind.is_modify() || kind.is_remove()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use notify::EventKind;
    use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

    use super::{is_board_change, start_watcher};

    #[test]
    fn watcher_ignores_reads_and_keeps_content_changes() {
        assert!(!is_board_change(&EventKind::Access(AccessKind::Any)));
        assert!(is_board_change(&EventKind::Create(CreateKind::Any)));
        assert!(is_board_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_board_change(&EventKind::Remove(RemoveKind::Any)));
    }

    #[test]
    fn watcher_is_droppable_and_skips_a_missing_root() {
        let (tx, rx) = mpsc::channel();
        let watcher = start_watcher(None, tx);
        assert!(watcher.is_none());
        drop(watcher);
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
    }
}
