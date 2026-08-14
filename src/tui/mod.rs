//! ratatui entry point and terminal lifecycle for the native board UI.

mod app;
mod board;
mod card;
mod detail;
mod dialogs;
mod event;
mod image;
mod projects;
mod search;
mod sessions;
mod theme;

#[cfg(test)]
mod tests;

use std::io::{self, IsTerminal, Stdout};
use std::panic;
use std::path::{Path, PathBuf};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::core::error::{KanbanError, Result};
use crate::core::project::{Project, ProjectStore};

use event::LoopOutcome;

type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

#[derive(Default)]
struct TerminalSetupGuard {
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
    bracketed_paste: bool,
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
        if self.bracketed_paste {
            let _ = stdout.execute(DisableBracketedPaste);
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
        stdout.execute(EnableBracketedPaste)?;
        setup.bracketed_paste = true;
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
        let _ = backend.execute(DisableBracketedPaste);
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
            let _ = stdout.execute(DisableBracketedPaste);
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

/// Suspend the TUI, hand the real terminal to a foreground process, then
/// restore. Shared by tmux attach and `<backend> --resume`: both need the
/// alternate screen, raw mode, and capture modes torn down while the child owns
/// the terminal, and rebuilt afterwards regardless of how the child exited.
fn run_terminal_action(action: &app::TerminalAction) -> Result<bool> {
    let mut stdout = io::stdout();
    let suspended = (|| -> Result<()> {
        stdout.execute(Show)?;
        stdout.execute(DisableBracketedPaste)?;
        stdout.execute(DisableMouseCapture)?;
        stdout.execute(LeaveAlternateScreen)?;
        disable_raw_mode()?;
        Ok(())
    })();
    if let Err(err) = suspended {
        let _ = restore_terminal();
        return Err(err);
    }

    let result = match action {
        app::TerminalAction::Attach(session_id) => crate::agent::attach_to_session(session_id),
        app::TerminalAction::Foreground {
            command, args, cwd, ..
        } => crate::agent::run_foreground(command, args, Some(cwd)),
    };
    let restore_result = restore_terminal();
    match (result, restore_result) {
        (Ok(ok), Ok(())) => Ok(ok),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
}

fn restore_terminal() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;
    stdout.execute(EnableBracketedPaste)?;
    stdout.execute(Hide)?;
    Ok(())
}

/// How the TUI should open: a registered project, a legacy in-place board, or
/// the projects list (unknown cwd).
#[derive(Debug, Clone)]
pub enum TuiStart {
    Project(Project),
    InPlace(PathBuf),
    Projects { return_to: Option<Project> },
}

pub fn run(start: TuiStart) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(KanbanError::Invalid(
            "The TUI requires an interactive terminal".to_string(),
        ));
    }
    let _panic_hook = PanicHookGuard::install();
    let mut terminal = TerminalGuard::enter()?;
    let mut app = app_from_start(start)?;
    let threads = event::spawn_shared_threads(app.settings.refresh_interval);
    loop {
        match event::run_event_loop(terminal.terminal_mut(), &mut app, &threads)? {
            LoopOutcome::Quit => break,
            LoopOutcome::OpenProject(project) => {
                app = app::App::for_project(project)?;
            }
            LoopOutcome::ShowProjects { return_to } => {
                app = app::App::projects_only(return_to)?;
            }
        }
    }
    Ok(())
}

fn app_from_start(start: TuiStart) -> Result<app::App> {
    match start {
        TuiStart::Project(project) => app::App::for_project(project),
        TuiStart::InPlace(path) => app::App::new(&path),
        TuiStart::Projects { return_to } => app::App::projects_only(return_to),
    }
}

/// Open the TUI on a path the same way the pre-store entry point did.
pub fn run_in_place(project_path: impl AsRef<Path>) -> Result<()> {
    run(TuiStart::InPlace(project_path.as_ref().to_path_buf()))
}

pub fn run_project(project: Project) -> Result<()> {
    let _ = ProjectStore::open()?.touch_opened(&project.id);
    run(TuiStart::Project(project))
}
