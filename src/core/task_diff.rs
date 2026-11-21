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
    /// Only files and checkouts outside the project, recorded with
    /// `kanban track`.
    External,
}

impl DiffSource {
    pub fn label(&self) -> &'static str {
        match self {
            DiffSource::Snapshot => "review snapshot",
            DiffSource::Worktree => "worktree vs base",
            DiffSource::Branch => "branch vs base",
            DiffSource::Landed => "landed commit",
            DiffSource::Backups => "backups vs current files",
            DiffSource::External => "tracked outside files",
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
        let current = work.join(&rel);
        file_patch(
            work,
            &backup_dir.join(&rel),
            &current,
            &rel.display().to_string(),
            &mut patch,
        );
    }
    patch
}

/// Append the `--no-index` diff of `original` → `current` under the header
/// name `name`; a missing side diffs against `/dev/null`.
fn file_patch(cwd: &Path, original: &Path, current: &Path, name: &str, patch: &mut String) {
    let side = |path: &Path| {
        if path.is_file() {
            path.display().to_string()
        } else {
            "/dev/null".to_string()
        }
    };
    let Some(text) = git_diff(cwd, &["--no-index", "--", &side(original), &side(current)]) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    // `--no-index` headers carry the compared paths; name the file instead.
    patch.push_str(&format!("diff --git a/{name} b/{name}\n"));
    patch.extend(
        text.lines()
            .skip_while(|line| !line.starts_with("@@") && !line.starts_with("Binary files "))
            .flat_map(|line| [line, "\n"]),
    );
}

/// Manifest of what `kanban track` recorded for one task, one entry a line:
/// `file\t<path>` (original kept under `files/`), `new\t<path>` (did not
/// exist yet) or `git\t<checkout>\t<commit>`.
const MANIFEST: &str = "manifest";

/// The tree of an empty repository, the base for a checkout without commits.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Record the current state of `path`, a file or git checkout outside the
/// project, under `store` so later diffs show what the task changed there.
/// The first record of a path wins, so tracking again keeps the original.
/// Returns a one-line description of what was recorded.
pub fn track_external(store: &Path, path: &Path) -> Result<String, String> {
    let path = absolute(path)?;
    let manifest_path = store.join(MANIFEST);
    let manifest = std::fs::read_to_string(&manifest_path).unwrap_or_default();
    let (entry, done) = if path.is_dir() {
        let Some(top) = git_line(&path, &["rev-parse", "--show-toplevel"]) else {
            return Err(format!(
                "{} is a folder outside git; track its files one by one",
                path.display()
            ));
        };
        let base = git_line(Path::new(&top), &["rev-parse", "--verify", "-q", "HEAD"])
            .unwrap_or_else(|| EMPTY_TREE.to_string());
        let short = &base[..base.len().min(12)];
        (
            format!("git\t{top}\t{base}"),
            format!("{top} (git checkout at {short})"),
        )
    } else if path.is_file() {
        let copy = store
            .join("files")
            .join(path.strip_prefix("/").unwrap_or(&path));
        if !manifest_entries(&manifest).any(|(_, tracked, _)| Path::new(tracked) == path) {
            std::fs::create_dir_all(copy.parent().expect("copy folder"))
                .and_then(|_| std::fs::copy(&path, &copy))
                .map_err(|err| format!("{}: {err}", path.display()))?;
        }
        (
            format!("file\t{}", path.display()),
            path.display().to_string(),
        )
    } else {
        (
            format!("new\t{}", path.display()),
            format!("{} (new file)", path.display()),
        )
    };
    let key = entry.split('\t').nth(1).unwrap_or_default().to_string();
    if manifest_entries(&manifest).any(|(_, tracked, _)| tracked == key) {
        return Ok(format!("{key} (already tracked)"));
    }
    std::fs::create_dir_all(store).map_err(|err| format!("{}: {err}", store.display()))?;
    let mut text = manifest;
    text.push_str(&entry);
    text.push('\n');
    crate::core::storage::atomic_write_text(&manifest_path, &text)
        .map_err(|err| format!("{}: {err}", manifest_path.display()))?;
    Ok(done)
}

/// Everything recorded by [`track_external`] against the current state, as
/// one patch named by absolute paths. Empty when nothing was tracked.
pub fn external_patch(store: &Path) -> String {
    let Ok(manifest) = std::fs::read_to_string(store.join(MANIFEST)) else {
        return String::new();
    };
    let mut patch = String::new();
    for (kind, path, base) in manifest_entries(&manifest) {
        let target = Path::new(path);
        let cwd = target
            .ancestors()
            .find(|dir| dir.is_dir())
            .unwrap_or(Path::new("/"));
        match kind {
            "file" => {
                let original = store.join("files").join(path.trim_start_matches('/'));
                file_patch(cwd, &original, target, path, &mut patch);
            }
            "new" => file_patch(cwd, Path::new("/dev/null"), target, path, &mut patch),
            "git" if target.is_dir() => patch.push_str(&checkout_patch(target, base)),
            _ => {}
        }
    }
    patch
}

fn manifest_entries(manifest: &str) -> impl Iterator<Item = (&str, &str, &str)> {
    manifest.lines().filter_map(|line| {
        let mut parts = line.split('\t');
        let kind = parts.next()?;
        let path = parts.next()?;
        Some((kind, path, parts.next().unwrap_or_default()))
    })
}

/// Commits, uncommitted edits and untracked files of a checkout since `base`,
/// with paths made absolute.
fn checkout_patch(top: &Path, base: &str) -> String {
    let mut patch = git_diff(top, &[base, "--"]).unwrap_or_default();
    let untracked = Command::new("git")
        .current_dir(top)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    for rel in untracked.split('\0').filter(|rel| !rel.is_empty()) {
        file_patch(top, Path::new("/dev/null"), &top.join(rel), rel, &mut patch);
    }
    rebase_paths(&patch, &top.display().to_string())
}

/// Prefix every file header of `patch` with `root`, leaving hunk text alone.
fn rebase_paths(patch: &str, root: &str) -> String {
    let mut out = String::with_capacity(patch.len());
    let mut in_hunk = false;
    for line in patch.lines() {
        let rebased = if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            let name = git_header_path(rest);
            format!("diff --git a/{root}/{name} b/{root}/{name}")
        } else if in_hunk {
            line.to_string()
        } else if line.starts_with("@@") {
            in_hunk = true;
            line.to_string()
        } else if let Some(path) = line.strip_prefix("--- a/") {
            format!("--- a/{root}/{path}")
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            format!("+++ b/{root}/{path}")
        } else {
            line.to_string()
        };
        out.push_str(&rebased);
        out.push('\n');
    }
    out
}

fn absolute(path: &Path) -> Result<std::path::PathBuf, String> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Ok(resolved);
    }
    // A file that does not exist yet: resolve its folder instead.
    let absolute = std::path::absolute(path).map_err(|err| format!("{}: {err}", path.display()))?;
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .map_err(|err| format!("{}: {err}", parent.display())),
        _ => Err(format!("{}: not a file path", path.display())),
    }
}

fn git_line(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !line.is_empty()).then_some(line)
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

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn tracked_outside_files_and_checkouts_diff_by_absolute_path() {
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let store = root_path.join("store");
        let outside = root_path.join("outside");
        let repo = root_path.join("repo");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(outside.join("conf.txt"), "a\nb\n").unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("main.go"), "x\ny\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);

        assert!(track_external(&store, &outside).is_err(), "plain folder");
        track_external(&store, &outside.join("conf.txt")).unwrap();
        track_external(&store, &outside.join("new.txt")).unwrap();
        track_external(&store, &repo).unwrap();
        // The agent edits the file, then tracking again must keep the original.
        std::fs::write(outside.join("conf.txt"), "a\nc\n").unwrap();
        let again = track_external(&store, &outside.join("conf.txt")).unwrap();
        assert!(again.ends_with("(already tracked)"), "{again}");
        std::fs::write(outside.join("new.txt"), "fresh\n").unwrap();
        std::fs::write(repo.join("main.go"), "x\n").unwrap();
        git(&repo, &["commit", "-qam", "drop y"]);
        std::fs::write(repo.join("extra.go"), "z\n").unwrap();

        let diff = parse(DiffSource::External, &external_patch(&store));
        let counts: Vec<_> = diff
            .files
            .iter()
            .map(|f| (f.path.clone(), f.added, f.removed))
            .collect();
        let abs = |path: &Path| path.display().to_string();
        assert_eq!(
            counts,
            [
                (abs(&outside.join("conf.txt")), 1, 1),
                (abs(&outside.join("new.txt")), 1, 0),
                (abs(&repo.join("main.go")), 0, 1),
                (abs(&repo.join("extra.go")), 1, 0),
            ]
        );
    }
}
