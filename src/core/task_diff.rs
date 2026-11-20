//! What a task changed, as parsed unified-diff files: the detail's Diff tab
//! and its `Δ +a −r · n files` summary. Pure parsing lives here; the git
//! calls that pick the right base are in [`Operations::task_diff`].
//!
//! [`Operations::task_diff`]: crate::core::operations::Operations::task_diff

use std::path::Path;
use std::process::Command;

/// Patch lines kept for one diff; the rest is dropped and flagged, so a
/// vendored tree or a generated file cannot balloon the TUI's memory.
pub const MAX_PATCH_LINES: usize = 20_000;

/// Where a [`TaskDiff`] was read from, shown in the Diff tab's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSource {
    /// Patch captured at executor handoff.
    Snapshot,
    /// `git diff <base>` inside the task's worktree, uncommitted work included.
    Worktree,
    /// `git diff <base> <branch>` in the project folder.
    Branch,
    /// The `kanban: land TASK-N` integration commit.
    Landed,
    /// The task's file backups against the current files (non-isolated runs).
    Backups,
}

impl DiffSource {
    pub fn label(&self) -> &'static str {
        match self {
            DiffSource::Snapshot => "review snapshot",
            DiffSource::Worktree => "worktree vs base",
            DiffSource::Branch => "branch vs base",
            DiffSource::Landed => "landed commit",
            DiffSource::Backups => "backups vs current files",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    /// Hunk lines from the first `@@` on, without the file headers.
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDiff {
    pub source: DiffSource,
    pub files: Vec<FileDiff>,
    /// Patch lines past [`MAX_PATCH_LINES`] were dropped.
    pub truncated: bool,
}

impl TaskDiff {
    pub fn added(&self) -> usize {
        self.files.iter().map(|file| file.added).sum()
    }

    pub fn removed(&self) -> usize {
        self.files.iter().map(|file| file.removed).sum()
    }

    /// `+120 −34 · 5 files`.
    pub fn summary(&self) -> String {
        let count = self.files.len();
        format!(
            "+{} −{} · {count} file{}",
            self.added(),
            self.removed(),
            if count == 1 { "" } else { "s" }
        )
    }
}

/// Parse `git diff` output into files. Counts cover every line even past the
/// line cap; only the kept text is cut.
pub fn parse(source: DiffSource, patch: &str) -> TaskDiff {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut kept = 0usize;
    let mut truncated = false;
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(FileDiff {
                path: git_header_path(rest),
                ..FileDiff::default()
            });
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if !in_hunk {
            if let Some(path) = line.strip_prefix("+++ ") {
                if path != "/dev/null" {
                    file.path = strip_side(path).to_string();
                }
                continue;
            }
            if line.starts_with("Binary files ") {
                file.binary = true;
                continue;
            }
            if !line.starts_with("@@") {
                continue;
            }
            in_hunk = true;
        }
        if line.starts_with('+') {
            file.added += 1;
        } else if line.starts_with('-') {
            file.removed += 1;
        }
        if kept < MAX_PATCH_LINES {
            file.lines.push(line.to_string());
            kept += 1;
        } else {
            truncated = true;
        }
    }
    TaskDiff {
        source,
        files,
        truncated,
    }
}

/// `a/src/x.rs b/src/x.rs` → `src/x.rs`; the `+++` line refines it later.
fn git_header_path(rest: &str) -> String {
    match rest.rsplit_once(" b/") {
        Some((_, path)) => path.to_string(),
        None => rest.to_string(),
    }
}

fn strip_side(path: &str) -> &str {
    path.strip_prefix("b/")
        .or_else(|| path.strip_prefix("a/"))
        .unwrap_or(path)
}

/// Run `git diff` with plain, stable output in `dir`; `None` when git fails.
pub fn git_diff(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["diff", "--no-color", "--no-ext-diff", "--no-renames"])
        .args(args)
        .output()
        .ok()?;
    // `--no-index` exits 1 when the files differ.
    let ok = output.status.success() || output.status.code() == Some(1);
    ok.then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Diff of every backed-up original under `backup_dir` against the file at
/// the same relative path under `work`. A file the agent deleted diffs
/// against `/dev/null`.
pub fn backups_patch(backup_dir: &Path, work: &Path) -> String {
    let mut relative = Vec::new();
    collect_files(backup_dir, backup_dir, &mut relative);
    relative.sort();
    let mut patch = String::new();
    for rel in relative {
        let original = backup_dir.join(&rel);
        let current = work.join(&rel);
        let current_arg = if current.is_file() {
            current.display().to_string()
        } else {
            "/dev/null".to_string()
        };
        let Some(text) = git_diff(
            work,
            &[
                "--no-index",
                "--",
                &original.display().to_string(),
                &current_arg,
            ],
        ) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        // `--no-index` headers carry absolute paths; name the file by its
        // repo-relative path instead.
        let name = rel.display().to_string();
        patch.push_str(&format!("diff --git a/{name} b/{name}\n"));
        patch.extend(
            text.lines()
                .skip_while(|line| !line.starts_with("@@") && !line.starts_with("Binary files "))
                .flat_map(|line| [line, "\n"]),
        );
    }
    patch
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_path_buf());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 fn a() {
-    old();
+    new();
+    more();
 }
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
diff --git a/img.png b/img.png
Binary files a/img.png and b/img.png differ
";

    #[test]
    fn parses_files_counts_and_hunks() {
        let diff = parse(DiffSource::Worktree, PATCH);
        assert_eq!(diff.files.len(), 3);
        assert_eq!(diff.files[0].path, "src/a.rs");
        assert_eq!((diff.files[0].added, diff.files[0].removed), (2, 1));
        assert_eq!(diff.files[0].lines[0], "@@ -1,3 +1,4 @@");
        assert_eq!(diff.files[0].lines.len(), 6);
        assert_eq!(diff.files[1].path, "new.txt");
        assert_eq!(diff.files[1].added, 1);
        assert!(diff.files[2].binary);
        assert_eq!(diff.summary(), "+3 −1 · 3 files");
        assert!(!diff.truncated);
    }

    #[test]
    fn caps_kept_lines_but_counts_all() {
        let mut patch = String::from("diff --git a/x b/x\n+++ b/x\n@@ -0,0 +1 @@\n");
        for _ in 0..MAX_PATCH_LINES + 10 {
            patch.push_str("+line\n");
        }
        let diff = parse(DiffSource::Branch, &patch);
        assert!(diff.truncated);
        assert_eq!(diff.files[0].added, MAX_PATCH_LINES + 10);
        assert_eq!(diff.files[0].lines.len(), MAX_PATCH_LINES);
    }

    #[test]
    fn backups_diff_against_current_files() {
        let root = tempfile::tempdir().unwrap();
        let backups = root.path().join("backups");
        let work = root.path().join("work");
        std::fs::create_dir_all(backups.join("src")).unwrap();
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(backups.join("src/a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(work.join("src/a.txt"), "one\nthree\n").unwrap();
        std::fs::write(backups.join("gone.txt"), "bye\n").unwrap();
        std::fs::write(backups.join("same.txt"), "x\n").unwrap();
        std::fs::write(work.join("same.txt"), "x\n").unwrap();
        let diff = parse(DiffSource::Backups, &backups_patch(&backups, &work));
        let paths: Vec<_> = diff.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["gone.txt", "src/a.txt"]);
        assert_eq!((diff.files[0].added, diff.files[0].removed), (0, 1));
        assert_eq!((diff.files[1].added, diff.files[1].removed), (1, 1));
    }
}
