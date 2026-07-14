use std::process::ExitCode;

fn main() -> ExitCode {
    // Rust ignores SIGPIPE by default, turning `kanban list | head` into a
    // "failed printing to stdout" panic. Restore the conventional CLI
    // behavior: die quietly when the read end of the pipe closes.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    kanban4ai::cli::run()
}
