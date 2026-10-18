//! Diagnostics for a board that stops responding.
//!
//! While the TUI owns the terminal, stderr is redirected to
//! `<store>/logs/tui.log`: the embedded sync worker, panics on background
//! threads and child processes that inherit stderr would otherwise print
//! straight over the alternate screen, which ratatui's diff renderer never
//! repaints. A watchdog logs every main-loop phase that runs longer than
//! [`STALL_REPORT`] with each thread's kernel wait channel and the holders of
//! the lock files the process has open, and `Ctrl+C` during such a stall
//! restores the terminal and exits instead of being swallowed by raw mode.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// First stall report; later ones back off by [`REPORT_BACKOFF`].
const STALL_REPORT: Duration = Duration::from_secs(5);
const REPORT_BACKOFF: u32 = 4;
/// A stall this long turns `Ctrl+C` into an emergency exit.
const EMERGENCY_STALL: Duration = Duration::from_secs(2);
/// `tui.log` is rotated to `tui.log.1` on start once it grows past this.
const LOG_LIMIT: u64 = 2 * 1024 * 1024;

/// The terminal's stderr, saved while fd 2 points at the log; -1 when not
/// redirected (then nothing is logged, so nothing lands on the board).
static SAVED_STDERR: AtomicI32 = AtomicI32::new(-1);
static PHASE: Mutex<Option<Busy>> = Mutex::new(None);

#[derive(Clone, Copy)]
struct Busy {
    phase: &'static str,
    since: Instant,
    reports: u32,
}

pub(super) fn log_path() -> Option<PathBuf> {
    let dir = crate::core::project::store_root().ok()?.join("logs");
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join("tui.log"))
}

pub(super) fn logging() -> bool {
    SAVED_STDERR.load(Ordering::SeqCst) >= 0
}

/// Append one timestamped entry to the log; a no-op when stderr still
/// points at the terminal.
pub(super) fn log(message: &str) {
    if !logging() {
        return;
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let _ = writeln!(io::stderr(), "{stamp} [{}] {message}", std::process::id());
}

/// Holds stderr on the log file for the TUI's lifetime.
pub(super) struct StderrLog;

impl StderrLog {
    pub(super) fn redirect() -> Option<Self> {
        let path = log_path()?;
        if fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_LIMIT) {
            let _ = fs::rename(&path, path.with_extension("log.1"));
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        redirect_fd2(&file)?;
        log(&format!(
            "tui start: kanban4ai {}",
            env!("CARGO_PKG_VERSION")
        ));
        Some(Self)
    }
}

impl Drop for StderrLog {
    fn drop(&mut self) {
        log("tui exit");
        restore_stderr();
    }
}

#[cfg(unix)]
fn redirect_fd2(file: &fs::File) -> Option<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: plain descriptor calls on fds this process owns; the saved copy
    // is close-on-exec so agents and git never inherit the terminal through it.
    unsafe {
        let saved = libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3);
        if saved < 0 {
            return None;
        }
        if libc::dup2(file.as_raw_fd(), 2) < 0 {
            libc::close(saved);
            return None;
        }
        SAVED_STDERR.store(saved, Ordering::SeqCst);
    }
    Some(())
}

#[cfg(not(unix))]
fn redirect_fd2(_file: &fs::File) -> Option<()> {
    None
}

/// Point fd 2 back at the terminal. Idempotent; safe from the panic hook.
pub(super) fn restore_stderr() {
    let saved = SAVED_STDERR.swap(-1, Ordering::SeqCst);
    #[cfg(unix)]
    if saved >= 0 {
        // SAFETY: `saved` is the descriptor `redirect_fd2` duplicated.
        unsafe {
            libc::dup2(saved, 2);
            libc::close(saved);
        }
    }
    #[cfg(not(unix))]
    let _ = saved;
}

/// Mark the main loop as working on `phase` from now on.
pub(super) fn busy(phase: &'static str) {
    let previous = phase_slot().replace(Busy {
        phase,
        since: Instant::now(),
        reports: 0,
    });
    note_recovery(previous);
}

/// Mark the main loop as waiting for the next event.
pub(super) fn idle() {
    let previous = phase_slot().take();
    note_recovery(previous);
}

fn phase_slot() -> std::sync::MutexGuard<'static, Option<Busy>> {
    PHASE.lock().unwrap_or_else(|err| err.into_inner())
}

fn note_recovery(previous: Option<Busy>) {
    if let Some(busy) = previous.filter(|busy| busy.reports > 0) {
        log(&format!(
            "main loop resumed after {:.1}s in {}",
            busy.since.elapsed().as_secs_f64(),
            busy.phase
        ));
    }
}

fn stalled() -> Option<(&'static str, Duration)> {
    let busy = (*phase_slot())?;
    Some((busy.phase, busy.since.elapsed()))
}

/// Next report deadline for a stall already reported `reports` times.
fn report_due(reports: u32) -> Duration {
    STALL_REPORT * REPORT_BACKOFF.saturating_pow(reports.min(4))
}

pub(super) fn spawn_watchdog() {
    let _ = thread::Builder::new()
        .name("tui-watchdog".into())
        .spawn(|| {
            loop {
                thread::sleep(Duration::from_secs(1));
                let due = {
                    let mut slot = phase_slot();
                    match slot.as_mut() {
                        Some(busy) if busy.since.elapsed() >= report_due(busy.reports) => {
                            busy.reports += 1;
                            Some((busy.phase, busy.since.elapsed()))
                        }
                        _ => None,
                    }
                };
                if let Some((phase, elapsed)) = due {
                    report(&format!(
                        "main loop stalled {:.1}s in {phase}",
                        elapsed.as_secs_f64()
                    ));
                }
            }
        });
}

/// Called by the input thread for every `Ctrl+C`: when the main loop has
/// been stuck long enough that it cannot be the one handling the key, log
/// the state, give the terminal back and exit.
pub(super) fn ctrl_c_pressed() {
    let Some((phase, elapsed)) = stalled().filter(|(_, elapsed)| *elapsed >= EMERGENCY_STALL)
    else {
        return;
    };
    let summary = format!("{phase}, {:.1}s", elapsed.as_secs_f64());
    report(&format!(
        "Ctrl+C during a main-loop stall ({summary}); exiting"
    ));
    restore_stderr();
    let _ = ratatui::crossterm::terminal::disable_raw_mode();
    // The stalled thread may itself be blocked writing to the terminal, so
    // the restore sequence gets a bounded attempt instead of the stdout lock.
    let (done, wait) = mpsc::channel();
    thread::spawn(move || {
        write_terminal(super::restore_sequence().as_bytes());
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(Duration::from_millis(500));
    let path = log_path().map_or_else(String::new, |path| format!("; see {}", path.display()));
    eprintln!("kanban4ai: the board stopped responding ({summary}){path}");
    std::process::exit(130);
}

#[cfg(unix)]
fn write_terminal(bytes: &[u8]) {
    use std::os::fd::FromRawFd;
    // SAFETY: fd 1 stays open for the process lifetime; ManuallyDrop keeps
    // this handle from closing it.
    let mut out = std::mem::ManuallyDrop::new(unsafe { fs::File::from_raw_fd(1) });
    let _ = out.write_all(bytes);
}

#[cfg(not(unix))]
fn write_terminal(bytes: &[u8]) {
    let _ = io::stdout().write_all(bytes);
    let _ = io::stdout().flush();
}

/// Log `headline` with what every thread is blocked on and who holds the
/// lock files this process has open.
fn report(headline: &str) {
    if !logging() {
        return;
    }
    let mut text = String::from(headline);
    for line in thread_states() {
        text.push_str("\n  thread ");
        text.push_str(&line);
    }
    for line in lock_holders() {
        text.push_str("\n  lock ");
        text.push_str(&line);
    }
    log(&text);
}

fn thread_states() -> Vec<String> {
    let Ok(entries) = fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let read = |name: &str| {
            fs::read_to_string(dir.join(name))
                .map(|text| text.trim().to_string())
                .unwrap_or_default()
        };
        // The state letter follows the parenthesised command name.
        let stat = read("stat");
        let state = stat
            .rsplit_once(") ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or("?")
            .to_string();
        lines.push(format!(
            "{} {} {state} wchan={}",
            entry.file_name().to_string_lossy(),
            read("comm"),
            read("wchan"),
        ));
    }
    lines.sort();
    lines
}

/// `/proc/locks` entries (held and waiting) on the `.lock` files open here.
fn lock_holders() -> Vec<String> {
    let Ok(fds) = fs::read_dir("/proc/self/fd") else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for fd in fds.flatten() {
        let Ok(target) = fs::read_link(fd.path()) else {
            continue;
        };
        if target.extension().is_some_and(|ext| ext == "lock") || target.ends_with(".lock") {
            #[cfg(unix)]
            if let Ok(meta) = fs::metadata(&target) {
                use std::os::unix::fs::MetadataExt;
                files.push((meta.ino(), target));
            }
        }
    }
    files.sort();
    files.dedup();
    let locks = fs::read_to_string("/proc/locks").unwrap_or_default();
    let mut lines = Vec::new();
    for (ino, path) in &files {
        let mut any = false;
        for line in locks.lines() {
            let Some((pid, inode)) = parse_lock_line(line) else {
                continue;
            };
            if inode != *ino {
                continue;
            }
            any = true;
            let cmd = fs::read(format!("/proc/{pid}/cmdline"))
                .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " "))
                .unwrap_or_default();
            lines.push(format!("{}: {line} :: {}", path.display(), cmd.trim()));
        }
        if !any {
            lines.push(format!("{}: open, unlocked", path.display()));
        }
    }
    lines
}

/// `(pid, inode)` of one `/proc/locks` line; waiters carry an extra `->`.
fn parse_lock_line(line: &str) -> Option<(u32, u64)> {
    let fields: Vec<&str> = line.split_whitespace().filter(|f| *f != "->").collect();
    let pid = fields.get(4)?.parse().ok()?;
    let inode = fields.get(5)?.rsplit(':').next()?.parse().ok()?;
    Some((pid, inode))
}

#[cfg(test)]
mod tests {
    use super::{parse_lock_line, report_due};
    use std::time::Duration;

    #[test]
    fn parses_held_and_waiting_lock_lines() {
        assert_eq!(
            parse_lock_line("1: FLOCK  ADVISORY  WRITE 4017608 00:2b:123456 0 EOF"),
            Some((4017608, 123456))
        );
        assert_eq!(
            parse_lock_line("1: -> FLOCK  ADVISORY  WRITE 42 00:2b:77 0 EOF"),
            Some((42, 77))
        );
        assert_eq!(parse_lock_line("garbage"), None);
    }

    #[test]
    fn stall_reports_back_off() {
        assert_eq!(report_due(0), Duration::from_secs(5));
        assert_eq!(report_due(1), Duration::from_secs(20));
        assert_eq!(report_due(9), report_due(4));
    }
}
