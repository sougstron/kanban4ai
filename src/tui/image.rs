use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::error::{KanbanError, Result};

pub fn copy_text(text: &str) -> Result<()> {
    let sequence = osc52_sequence(text);
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn osc52_sequence(text: &str) -> String {
    format!("\u{1b}]52;c;{}\u{1b}\\", base64(text.as_bytes()))
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
    use super::{osc52_sequence, sniff_extension};

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
}
