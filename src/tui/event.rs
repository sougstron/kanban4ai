use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event as CrosstermEvent};

use crate::core::error::Result;

use super::app::App;
use super::board;

#[derive(Debug)]
pub enum AppEvent {
    Input(CrosstermEvent),
    InputError(String),
    FsChanged,
    FsDebounced(u64),
    Tick,
}

pub fn run_event_loop<B: Backend<Error = std::io::Error>>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let input_gate = Arc::new(Mutex::new(()));
    spawn_input_thread(tx.clone(), Arc::clone(&input_gate));
    spawn_watcher_thread(app.project_path.as_path(), tx.clone());
    spawn_tick_thread(app.settings.refresh_interval, tx.clone());

    terminal.draw(|frame| board::ui(frame, app))?;
    while !app.should_quit {
        let event = match rx.recv() {
            Ok(event) => event,
            Err(_) => break,
        };
        match event {
            AppEvent::Input(input) => handle_input(app, input)?,
            AppEvent::InputError(message) => app.stop_after_input_error(message),
            AppEvent::FsChanged => {
                let generation = app.note_fs_changed();
                spawn_debounce_timer(tx.clone(), generation);
            }
            AppEvent::FsDebounced(generation) => app.reload_debounced_change(generation)?,
            AppEvent::Tick => app.tick()?,
        }
        if let Some(text) = app.take_pending_copy() {
            app.finish_copy(super::image::copy_text(&text));
        }
        if let Some(action) = app.take_terminal_action() {
            let _input_guard = input_gate.lock().map_err(|_| {
                crate::core::error::KanbanError::Invalid("terminal input lock poisoned".to_string())
            })?;
            let ok = super::run_terminal_action(&action)?;
            app.finish_terminal_action(&action, ok);
            terminal.clear()?;
        }
        terminal.draw(|frame| board::ui(frame, app))?;
    }
    Ok(())
}

fn handle_input(app: &mut App, input: CrosstermEvent) -> Result<()> {
    match input {
        CrosstermEvent::Key(key) => app.handle_key(key),
        CrosstermEvent::Mouse(mouse) => app.handle_mouse(mouse),
        CrosstermEvent::Paste(text) => app.handle_paste(&text),
        CrosstermEvent::Resize(_, _) => Ok(()),
        _ => Ok(()),
    }
}

fn spawn_input_thread(tx: Sender<AppEvent>, input_gate: Arc<Mutex<()>>) {
    thread::spawn(move || {
        loop {
            let input = {
                let Ok(_guard) = input_gate.lock() else {
                    break;
                };
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => event::read(),
                    Ok(false) => continue,
                    Err(err) => Err(err),
                }
            };
            match input {
                Ok(event) => {
                    if tx.send(AppEvent::Input(event)).is_err() {
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

fn spawn_watcher_thread(project_path: &Path, tx: Sender<AppEvent>) {
    let paths = ["tasks", "threads", "sessions"]
        .into_iter()
        .map(|name| project_path.join(".kanban").join(name))
        .collect::<Vec<_>>();
    thread::spawn(move || {
        let tx_events = tx.clone();
        let mut watcher = match RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                if res.is_ok_and(|event| is_board_change(&event.kind)) {
                    let _ = tx_events.send(AppEvent::FsChanged);
                }
            },
            Config::default(),
        ) {
            Ok(watcher) => watcher,
            Err(_) => return,
        };
        for path in paths.iter().filter(|path| path.exists()) {
            let _ = watcher.watch(path, RecursiveMode::Recursive);
        }
        loop {
            thread::park();
        }
    });
}

fn is_board_change(kind: &notify::EventKind) -> bool {
    kind.is_create() || kind.is_modify() || kind.is_remove()
}

#[cfg(test)]
mod tests {
    use notify::EventKind;
    use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

    use super::is_board_change;

    #[test]
    fn watcher_ignores_reads_and_keeps_content_changes() {
        assert!(!is_board_change(&EventKind::Access(AccessKind::Any)));
        assert!(is_board_change(&EventKind::Create(CreateKind::Any)));
        assert!(is_board_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_board_change(&EventKind::Remove(RemoveKind::Any)));
    }
}
