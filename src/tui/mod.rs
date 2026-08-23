//! ratatui entry point and terminal lifecycle for the native board UI.

mod app;
mod board;
mod card;
mod detail;
mod dialogs;
mod event;
mod image;
mod limits;
mod projects;
mod search;
mod sessions;
mod theme;

#[cfg(test)]
mod tests;

use std::io::{self, IsTerminal, Stdout, Write};
use std::panic;
use std::path::{Path, PathBuf};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};

use crate::core::error::{KanbanError, Result};
use crate::core::project::{Project, ProjectStore};

use event::LoopOutcome;

type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// XTWINOPS window-title stack. The board renames the terminal after the open
/// project (see `set_window_title`), so the title it found on entry is saved
/// here and restored on exit. Terminals without the stack ignore both
/// sequences and simply keep the board's title after quitting.
const PUSH_WINDOW_TITLE: &str = "\x1b[22;2t";
const POP_WINDOW_TITLE: &str = "\x1b[23;2t";
/// xterm modifyOtherKeys level 2. Inside tmux this asks the multiplexer to
/// report modified keys (Shift+Enter among them) to this pane once the server
/// is configured with `extended-keys on` and `extended-keys-format csi-u`;
/// tmux consumes the request per pane, so other panes are untouched. It is
/// never sent to a bare terminal: one that honours it without also speaking
/// the kitty protocol answers in the `CSI 27;mod;key~` form, which crossterm
/// cannot parse, and every modified key would turn into a dropped event.
const SET_MODIFY_OTHER_KEYS: &str = "\x1b[>4;2m";
const RESET_MODIFY_OTHER_KEYS: &str = "\x1b[>4m";

fn write_escape(stdout: &mut Stdout, escape: &str) -> bool {
    write!(stdout, "{escape}")
        .and_then(|()| stdout.flush())
        .is_ok()
}

/// Name the terminal window after the open project. This is the one cue that
/// survives the board not being on screen at all: it lands in the tab bar, the
/// multiplexer status line, and the window switcher, which is where you look
/// when several boards are open at once.
fn set_window_title(title: &str) {
    let _ = io::stdout().execute(SetTitle(title));
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    key_disambiguation: bool,
    key_modify_other_keys: bool,
    window_title: bool,
}

#[derive(Default)]
struct TerminalSetupGuard {
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
    bracketed_paste: bool,
    key_disambiguation: bool,
    key_modify_other_keys: bool,
    window_title: bool,
    cursor_hidden: bool,
    armed: bool,
}

/// Ask the terminal to disambiguate escape codes so modified Enter arrives as
/// `Enter` + `SHIFT` instead of a bare `Enter`. The detail answer panel still
/// needs that distinction (plain Enter submits). Terminals without the kitty
/// keyboard protocol simply ignore the request.
fn push_key_disambiguation(stdout: &mut Stdout) -> bool {
    if !matches!(
        ratatui::crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    ) {
        return false;
    }
    stdout
        .execute(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ))
        .is_ok()
}

/// Parse `tmux show-options -s` output and tell whether the server reports
/// extended keys to panes in the CSI-u form crossterm understands. Anything
/// else — `extended-keys off`, the default `xterm` format, an older tmux
/// without the format option — must keep the request unissued: tmux would
/// then re-encode modified keys as `CSI 27;mod;key~`, which crossterm drops,
/// so even Alt+Enter would stop working.
fn tmux_reports_csi_u(options: &str) -> bool {
    let mut enabled = false;
    let mut csi_u = false;
    for line in options.lines() {
        let mut fields = line.split_whitespace();
        match (fields.next(), fields.next()) {
            (Some("extended-keys"), Some(value)) => {
                enabled = matches!(value, "on" | "always");
            }
            (Some("extended-keys-format"), Some(value)) => csi_u = value == "csi-u",
            _ => {}
        }
    }
    enabled && csi_u
}

/// Ask tmux — when the TUI runs inside it — to report modified keys
/// distinctly to this pane. Returns true when Shift+Enter will actually
/// arrive carrying its SHIFT modifier; when the server is not configured
/// for it, nothing is sent and the caller shows the user which options are
/// missing instead of failing silently.
fn push_tmux_key_disambiguation(stdout: &mut Stdout) -> bool {
    if std::env::var_os("TMUX").is_none() {
        return false;
    }
    let Ok(output) = std::process::Command::new("tmux")
        .args(["show-options", "-s"])
        .output()
    else {
        return false;
    };
    if !tmux_reports_csi_u(&String::from_utf8_lossy(&output.stdout)) {
        return false;
    }
    write_escape(stdout, SET_MODIFY_OTHER_KEYS)
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
        if self.window_title {
            write_escape(&mut stdout, POP_WINDOW_TITLE);
        }
        if self.key_disambiguation {
            let _ = stdout.execute(PopKeyboardEnhancementFlags);
        }
        if self.key_modify_other_keys {
            write_escape(&mut stdout, RESET_MODIFY_OTHER_KEYS);
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
        setup.window_title = write_escape(&mut stdout, PUSH_WINDOW_TITLE);
        stdout.execute(EnableMouseCapture)?;
        setup.mouse_capture = true;
        stdout.execute(EnableBracketedPaste)?;
        setup.bracketed_paste = true;
        let key_disambiguation = push_key_disambiguation(&mut stdout);
        setup.key_disambiguation = key_disambiguation;
        // Kitty flags win when the terminal speaks that protocol; only then
        // is the tmux fallback skipped. The flag tracks the escape we sent
        // so teardown can reset it.
        let key_modify_other_keys = if key_disambiguation {
            false
        } else {
            push_tmux_key_disambiguation(&mut stdout)
        };
        setup.key_modify_other_keys = key_modify_other_keys;
        stdout.execute(Hide)?;
        setup.cursor_hidden = true;
        let window_title = setup.window_title;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        setup.disarm();
        Ok(Self {
            terminal,
            key_disambiguation,
            key_modify_other_keys,
            window_title,
        })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let key_disambiguation = self.key_disambiguation;
        let key_modify_other_keys = self.key_modify_other_keys;
        let window_title = self.window_title;
        let backend = self.terminal.backend_mut();
        let _ = backend.execute(Show);
        if window_title {
            let _ = backend.write_all(POP_WINDOW_TITLE.as_bytes());
            let _ = backend.flush();
        }
        if key_disambiguation {
            let _ = backend.execute(PopKeyboardEnhancementFlags);
        }
        if key_modify_other_keys {
            let _ = backend.write_all(RESET_MODIFY_OTHER_KEYS.as_bytes());
            let _ = backend.flush();
        }
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
            write_escape(&mut stdout, POP_WINDOW_TITLE);
            let _ = stdout.execute(PopKeyboardEnhancementFlags);
            write_escape(&mut stdout, RESET_MODIFY_OTHER_KEYS);
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
fn run_terminal_action(action: &app::TerminalAction, window_title: &str) -> Result<bool> {
    let mut stdout = io::stdout();
    let suspended = (|| -> Result<()> {
        stdout.execute(Show)?;
        // The child process gets the terminal back in its default key mode.
        let _ = stdout.execute(PopKeyboardEnhancementFlags);
        write_escape(&mut stdout, RESET_MODIFY_OTHER_KEYS);
        stdout.execute(DisableBracketedPaste)?;
        stdout.execute(DisableMouseCapture)?;
        stdout.execute(LeaveAlternateScreen)?;
        disable_raw_mode()?;
        Ok(())
    })();
    if let Err(err) = suspended {
        let _ = restore_terminal();
        set_window_title(window_title);
        return Err(err);
    }

    let result = match action {
        app::TerminalAction::Attach(session_id) => crate::agent::attach_to_session(session_id),
        app::TerminalAction::Foreground {
            command, args, cwd, ..
        } => crate::agent::run_foreground(command, args, Some(cwd)),
    };
    let restore_result = restore_terminal();
    // The child owned the terminal and may well have renamed it (an agent CLI
    // usually does), so the board's own title has to be re-asserted.
    set_window_title(window_title);
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
    let _ = push_key_disambiguation(&mut stdout) || push_tmux_key_disambiguation(&mut stdout);
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
                let mut next = app::App::for_project(project)?;
                next.apply_global_settings();
                app = next;
            }
            LoopOutcome::ShowProjects { return_to } => {
                let next = app::App::projects_only(return_to)?;
                app = next;
            }
        }
    }
    Ok(())
}

fn app_from_start(start: TuiStart) -> Result<app::App> {
    match start {
        TuiStart::Project(project) => {
            let mut app = app::App::for_project(project)?;
            app.apply_global_settings();
            Ok(app)
        }
        TuiStart::InPlace(path) => {
            let mut app = app::App::new(&path)?;
            app.apply_global_settings();
            Ok(app)
        }
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
