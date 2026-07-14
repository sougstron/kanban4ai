//! ratatui entry point and terminal lifecycle for the native board UI.

mod app;
mod board;
mod card;
mod detail;
mod dialogs;
mod event;
mod image;
mod search;
mod sessions;
mod theme;

#[cfg(test)]
mod tests;

use std::io::{self, IsTerminal, Stdout};
use std::panic;
use std::path::Path;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::core::error::{KanbanError, Result};

type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

#[derive(Default)]
struct TerminalSetupGuard {
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
    cursor_hidden: bool,
    armed: bool,
}

impl TerminalSetupGuard {
    fn armed() -> Self {
        Self {
            armed: true,
            ..Default::default()
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TerminalSetupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut stdout = io::stdout();
        if self.cursor_hidden {
            let _ = stdout.execute(Show);
        }
        if self.mouse_capture {
            let _ = stdout.execute(DisableMouseCapture);
        }
        if self.alternate_screen {
            let _ = stdout.execute(LeaveAlternateScreen);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        let mut setup = TerminalSetupGuard::armed();
        enable_raw_mode()?;
        setup.raw_mode = true;
        let mut stdout = io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        setup.alternate_screen = true;
        stdout.execute(EnableMouseCapture)?;
        setup.mouse_capture = true;
        stdout.execute(Hide)?;
        setup.cursor_hidden = true;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        setup.disarm();
        Ok(Self { terminal })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let backend = self.terminal.backend_mut();
        let _ = backend.execute(Show);
        let _ = backend.execute(DisableMouseCapture);
        let _ = backend.execute(LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

struct PanicHookGuard {
    previous: Option<PanicHook>,
}

impl PanicHookGuard {
    fn install() -> Self {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let mut stdout = io::stdout();
            let _ = stdout.execute(Show);
            let _ = stdout.execute(DisableMouseCapture);
            let _ = stdout.execute(LeaveAlternateScreen);
            eprintln!("{info}");
        }));
        Self {
            previous: Some(previous),
        }
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            panic::set_hook(previous);
        }
    }
}

fn attach_session(session_id: &str) -> Result<bool> {
    let mut stdout = io::stdout();
    let suspended = (|| -> Result<()> {
        stdout.execute(Show)?;
        stdout.execute(DisableMouseCapture)?;
        stdout.execute(LeaveAlternateScreen)?;
        disable_raw_mode()?;
        Ok(())
    })();
    if let Err(err) = suspended {
        let _ = restore_terminal();
        return Err(err);
    }

    let attach_result = crate::agent::attach_to_session(session_id);
    let restore_result = restore_terminal();
    match (attach_result, restore_result) {
        (Ok(attached), Ok(())) => Ok(attached),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
}

fn restore_terminal() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;
    stdout.execute(Hide)?;
    Ok(())
}

pub fn run(project_path: impl AsRef<Path>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(KanbanError::Invalid(
            "The TUI requires an interactive terminal".to_string(),
        ));
    }
    let _panic_hook = PanicHookGuard::install();
    let mut terminal = TerminalGuard::enter()?;
    let mut app = app::App::new(project_path.as_ref())?;
    event::run_event_loop(terminal.terminal_mut(), &mut app)
}
