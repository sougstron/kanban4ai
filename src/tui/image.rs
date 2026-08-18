use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::core::error::{KanbanError, Result};

/// How long a clipboard helper may stay resident before the handoff counts as
/// done. X11 helpers keep running to serve the selection, so "still alive"
/// means success, not failure.
const HELPER_HANDOFF: Duration = Duration::from_millis(300);

/// Put `text` on the system clipboard.
///
/// A native clipboard helper runs first because OSC 52 is write-only and
/// fails silently: tmux drops it unless `set-clipboard`/`allow-passthrough`
/// are enabled, and several terminals refuse clipboard writes outright, so
/// the copy looks successful while nothing can be pasted. The escape sequence
/// stays as the fallback for remote sessions that have no helper binary.
pub fn copy_text(text: &str) -> Result<()> {
    if copy_with_helper(text) {
        return Ok(());
    }
    write_osc52(text)
}

fn copy_with_helper(text: &str) -> bool {
    for (command, args) in helper_commands(
        has_env("WAYLAND_DISPLAY"),
        has_env("DISPLAY"),
        has_env("WSL_DISTRO_NAME") || has_env("WSLENV"),
    ) {
        if run_helper(command, &args, text) {
            return true;
        }
    }
    false
}

fn helper_commands(wayland: bool, x11: bool, wsl: bool) -> Vec<(&'static str, Vec<&'static str>)> {
    let mut commands = Vec::new();
    if cfg!(target_os = "macos") {
        commands.push(("pbcopy", Vec::new()));
    }
    if wayland {
        commands.push(("wl-copy", vec!["--type", "text/plain;charset=utf-8"]));
    }
    if x11 {
        commands.push(("xclip", vec!["-selection", "clipboard"]));
        commands.push(("xsel", vec!["--clipboard", "--input"]));
    }
    if wsl {
        commands.push(("clip.exe", Vec::new()));
    }
    commands
}

/// Feed `text` to one helper. Output is discarded rather than captured: a
/// helper that daemonises to own the selection keeps the inherited pipes open,
/// and reading them to EOF would block the whole TUI.
fn run_helper(command: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let written = stdin.write_all(text.as_bytes()).is_ok();
    drop(stdin);
    if !written {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    let deadline = Instant::now() + HELPER_HANDOFF;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => return true,
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => return false,
        }
    }
}

fn has_env(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|value| !value.is_empty())
}

fn write_osc52(text: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    for sequence in osc52_sequences(text, multiplexer()) {
        stdout.write_all(sequence.as_bytes())?;
    }
    stdout.flush()?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Multiplexer {
    None,
    Tmux,
    Screen,
}

fn multiplexer() -> Multiplexer {
    multiplexer_from_env(has_env("TMUX"), std::env::var("TERM").ok().as_deref())
}

fn multiplexer_from_env(tmux: bool, term: Option<&str>) -> Multiplexer {
    if tmux || term.is_some_and(|term| term.starts_with("tmux")) {
        Multiplexer::Tmux
    } else if term.is_some_and(|term| term.starts_with("screen")) {
        Multiplexer::Screen
    } else {
        Multiplexer::None
    }
}

fn osc52_sequences(text: &str, multiplexer: Multiplexer) -> Vec<String> {
    let sequence = osc52_sequence(text);
    match multiplexer {
        Multiplexer::None => vec![sequence],
        // tmux forwards a bare OSC 52 only when `set-clipboard` allows it and
        // honours the DCS wrapper only when `allow-passthrough` is on. Sending
        // both covers either configuration; the ignored one is swallowed, and
        // setting the same clipboard twice is harmless.
        Multiplexer::Tmux => vec![tmux_passthrough(&sequence), sequence],
        // screen never forwards OSC 52 itself and ends its passthrough at the
        // first embedded escape, so the sequence goes out in DCS chunks.
        Multiplexer::Screen => vec![screen_passthrough(&base64(text.as_bytes()))],
    }
}

fn osc52_sequence(text: &str) -> String {
    format!("\u{1b}]52;c;{}\u{1b}\\", base64(text.as_bytes()))
}

fn tmux_passthrough(sequence: &str) -> String {
    format!(
        "\u{1b}Ptmux;{}\u{1b}\\",
        sequence.replace('\u{1b}', "\u{1b}\u{1b}")
    )
}

fn screen_passthrough(payload: &str) -> String {
    const CHUNK: usize = 76;
    let payload: Vec<char> = payload.chars().collect();
    let mut sequence = String::from("\u{1b}P\u{1b}]52;c;");
    for (index, chunk) in payload.chunks(CHUNK).enumerate() {
        if index > 0 {
            sequence.push_str("\u{1b}P");
        }
        sequence.extend(chunk);
        sequence.push_str("\u{1b}\\");
    }
    if payload.is_empty() {
        sequence.push_str("\u{1b}\\");
    }
    sequence.push_str("\u{1b}P\u{1b}\\\u{1b}\\");
    sequence
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[usize::from(first >> 2)] as char);
        encoded.push(ALPHABET[usize::from(((first & 0b11) << 4) | (second >> 4))] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[usize::from(((second & 0b1111) << 2) | (third >> 6))] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[usize::from(third & 0b11_1111)] as char
        } else {
            '='
        });
    }
    encoded
}

pub fn paste_image_markdown(project_path: &Path) -> Result<String> {
    let bytes = clipboard_bytes()?;
    let image_bytes = if let Some(path) = clipboard_path(&bytes) {
        fs::read(path)?
    } else {
        bytes
    };
    let ext = sniff_extension(&image_bytes).ok_or_else(|| {
        KanbanError::Invalid("clipboard does not contain a png/jpg/gif/webp image".to_string())
    })?;
    let images_dir = project_path.join(".kanban").join("assets").join("images");
    fs::create_dir_all(&images_dir)?;
    ensure_inside_project(project_path, &images_dir)?;
    let hash = stable_hash(&image_bytes);
    let filename = format!(
        "pasted-{}-{hash:016x}.{ext}",
        crate::core::timefmt::now().and_utc().timestamp()
    );
    let path = images_dir.join(filename);
    atomic_write_bytes(&path, &image_bytes)?;
    let relative = path
        .strip_prefix(project_path)
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string();
    Ok(format!("![pasted image]({relative})"))
}

fn clipboard_bytes() -> Result<Vec<u8>> {
    for (command, args) in [
        ("wl-paste", vec!["--no-newline", "--type", "image/png"]),
        ("wl-paste", vec!["--no-newline"]),
        (
            "xclip",
            vec!["-selection", "clipboard", "-out", "-t", "image/png"],
        ),
        ("xclip", vec!["-selection", "clipboard", "-out"]),
    ] {
        let Ok(output) = Command::new(command).args(args).output() else {
            continue;
        };
        if output.status.success() && !output.stdout.is_empty() {
            return Ok(output.stdout);
        }
    }
    Err(KanbanError::Invalid(
        "no supported clipboard command returned data".to_string(),
    ))
}

fn clipboard_path(bytes: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let path = text.strip_prefix("file://").unwrap_or(text);
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

fn sniff_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| KanbanError::Invalid(format!("path has no parent: {}", path.display())))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp_")
        .tempfile_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.persist(path)
        .map_err(|err| KanbanError::Io(err.error))?;
    Ok(())
}

fn ensure_inside_project(project_path: &Path, directory: &Path) -> Result<()> {
    let project = project_path.canonicalize()?;
    let directory = directory.canonicalize()?;
    if directory.starts_with(&project) {
        Ok(())
    } else {
        Err(KanbanError::Invalid(format!(
            "refusing to write image outside project: {}",
            directory.display()
        )))
    }
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{
        Multiplexer, helper_commands, multiplexer_from_env, osc52_sequence, osc52_sequences,
        screen_passthrough, sniff_extension, tmux_passthrough,
    };

    #[test]
    fn sniffs_common_image_types() {
        assert_eq!(sniff_extension(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(sniff_extension(b"\xff\xd8\xffrest"), Some("jpg"));
        assert_eq!(sniff_extension(b"GIF89arest"), Some("gif"));
        assert_eq!(sniff_extension(b"RIFFxxxxWEBPrest"), Some("webp"));
        assert_eq!(sniff_extension(b"text"), None);
    }

    #[test]
    fn osc52_encodes_utf8_text_for_the_clipboard() {
        assert_eq!(osc52_sequence("foo"), "\u{1b}]52;c;Zm9v\u{1b}\\");
        assert_eq!(osc52_sequence("✓"), "\u{1b}]52;c;4pyT\u{1b}\\");
    }

    #[test]
    fn multiplexer_detection_prefers_the_tmux_marker_over_term() {
        assert_eq!(multiplexer_from_env(true, Some("xterm")), Multiplexer::Tmux);
        assert_eq!(
            multiplexer_from_env(true, Some("screen-256color")),
            Multiplexer::Tmux
        );
        assert_eq!(
            multiplexer_from_env(false, Some("tmux-256color")),
            Multiplexer::Tmux
        );
        assert_eq!(
            multiplexer_from_env(false, Some("screen")),
            Multiplexer::Screen
        );
        assert_eq!(
            multiplexer_from_env(false, Some("xterm-256color")),
            Multiplexer::None
        );
        assert_eq!(multiplexer_from_env(false, None), Multiplexer::None);
    }

    #[test]
    fn tmux_sessions_get_both_the_bare_and_wrapped_sequence() {
        let sequences = osc52_sequences("foo", Multiplexer::Tmux);
        assert_eq!(
            sequences,
            vec![
                "\u{1b}Ptmux;\u{1b}\u{1b}]52;c;Zm9v\u{1b}\u{1b}\\\u{1b}\\".to_string(),
                "\u{1b}]52;c;Zm9v\u{1b}\\".to_string(),
            ]
        );
        assert_eq!(osc52_sequences("foo", Multiplexer::None).len(), 1);
    }

    #[test]
    fn tmux_passthrough_doubles_every_escape_it_carries() {
        assert_eq!(
            tmux_passthrough("\u{1b}]52;c;Zm9v\u{1b}\\"),
            "\u{1b}Ptmux;\u{1b}\u{1b}]52;c;Zm9v\u{1b}\u{1b}\\\u{1b}\\"
        );
    }

    #[test]
    fn screen_passthrough_splits_long_payloads_into_dcs_chunks() {
        assert_eq!(
            screen_passthrough("Zm9v"),
            "\u{1b}P\u{1b}]52;c;Zm9v\u{1b}\\\u{1b}P\u{1b}\\\u{1b}\\"
        );
        let long = "A".repeat(100);
        let chunked = screen_passthrough(&long);
        assert!(chunked.starts_with(&format!("\u{1b}P\u{1b}]52;c;{}\u{1b}\\", "A".repeat(76))));
        assert!(chunked.ends_with(&format!(
            "\u{1b}P{}\u{1b}\\\u{1b}P\u{1b}\\\u{1b}\\",
            "A".repeat(24)
        )));
    }

    #[test]
    fn helper_commands_follow_the_available_display_servers() {
        let wayland = helper_commands(true, false, false);
        assert_eq!(
            wayland.first().map(|(command, _)| *command),
            Some("wl-copy")
        );
        let x11: Vec<&str> = helper_commands(false, true, false)
            .into_iter()
            .map(|(command, _)| command)
            .collect();
        assert_eq!(x11, vec!["xclip", "xsel"]);
        assert_eq!(
            helper_commands(false, false, true)
                .into_iter()
                .map(|(command, _)| command)
                .collect::<Vec<_>>(),
            vec!["clip.exe"]
        );
        assert!(cfg!(target_os = "macos") || helper_commands(false, false, false).is_empty());
    }
}
