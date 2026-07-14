use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::error::{KanbanError, Result};

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
    use super::sniff_extension;

    #[test]
    fn sniffs_common_image_types() {
        assert_eq!(sniff_extension(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(sniff_extension(b"\xff\xd8\xffrest"), Some("jpg"));
        assert_eq!(sniff_extension(b"GIF89arest"), Some("gif"));
        assert_eq!(sniff_extension(b"RIFFxxxxWEBPrest"), Some("webp"));
        assert_eq!(sniff_extension(b"text"), None);
    }
}
