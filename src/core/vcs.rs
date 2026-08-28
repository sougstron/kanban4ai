//! Git plumbing for worktree isolation (TASK-236 plan, TASK-244).
//!
//! This is the ONLY module in the tree allowed to run `git`. Every call is
//! `Command::new("git")` with an explicit `-C`, no shell, and `LC_ALL=C` —
//! `merge-tree` emits localized conflict prose (it came out in Russian during
//! investigation), so prose is never parsed; only machine-readable output is
//! (object ids, `merge-tree` stage lines, `diff --name-status -z` records).
//!
//! Two invariants from the plan are baked into these primitives:
//! - a snapshot captures the *live* contents of a dirty working directory
//!   (modified + untracked, honoring `.gitignore`) without touching the
//!   user's index or working tree;
//! - nothing here ever creates a commit on the user's branch — landing
//!   writes files to disk as ordinary unstaged modifications instead.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::NamedTempFile;

use super::error::{KanbanError, Result};

/// The spine ref: never in `refs/heads`, never checked out, never pushed.
/// It gives concurrent tasks a common ancestor that includes the human's
/// uncommitted work.
pub const INTEGRATION_REF: &str = "refs/kanban/integration";

/// `git merge-tree --write-tree` (object-database 3-way merge) requires 2.38.
const MIN_MERGE_TREE_VERSION: (u32, u32) = (2, 38);

/// A parsed git object id (SHA-1 or SHA-256 hex).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Oid(String);

impl Oid {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validate a hex object id as printed by git (40 chars SHA-1, 64 SHA-256).
fn oid_from(text: &str) -> Option<Oid> {
    let valid =
        (text.len() == 40 || text.len() == 64) && text.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then(|| Oid(text.to_ascii_lowercase()))
}

fn parse_oid(text: &str) -> Result<Oid> {
    let trimmed = text.trim();
    oid_from(trimmed)
        .ok_or_else(|| KanbanError::Invalid(format!("git did not print an object id: {trimmed:?}")))
}

/// One `<mode> <oid> <stage> <path>` record from `merge-tree --write-tree`.
/// Stage 1 is the merge base, 2 ours, 3 theirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub mode: u32,
    pub oid: Oid,
    pub stage: u8,
    pub path: String,
}

/// Result of a pure object-database 3-way merge. Nothing is ever written to
/// any working tree by [`GitRepo::preflight`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// The merge is conflict-free; `tree` is the merged top-level tree.
    Clean { tree: Oid },
    /// The merge conflicts; nothing was written anywhere.
    Conflict {
        /// Unique conflicting paths, in first-seen order.
        paths: Vec<String>,
        /// Per-path stage records (base/ours/theirs blobs).
        stages: Vec<Stage>,
    },
}

/// A detected git repository (the work tree root).
#[derive(Debug, Clone)]
pub struct GitRepo {
    root: PathBuf,
}

/// Why worktree isolation is (or is not) available for a project.
///
/// `NotRegistered` is never produced by [`availability`]: registration lives
/// in the project store, not here. Callers that gate isolation on
/// `project_id.is_some()` (the plan's hard prerequisite) construct it
/// themselves when the project is not registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// Isolation can run.
    Available,
    /// The folder is not a registered project (caller-side condition).
    NotRegistered,
    /// No `git` binary on PATH.
    NoGit,
    /// Git is present but older than the merge-tree requirement.
    GitTooOld { found: String },
    /// The work path is not inside a git repository.
    NotARepo,
    /// Unborn HEAD: `git init` with zero commits.
    UnbornHead,
    /// Detached HEAD (also covers a rebase in progress): no stable base.
    DetachedHead,
}

impl Availability {
    pub fn is_available(&self) -> bool {
        matches!(self, Availability::Available)
    }
}

impl fmt::Display for Availability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Availability::Available => f.write_str("available"),
            Availability::NotRegistered => f.write_str("project not registered"),
            Availability::NoGit => f.write_str("git not found"),
            Availability::GitTooOld { found } => write!(
                f,
                "git {found} is too old (merge-tree needs >= {}.{})",
                MIN_MERGE_TREE_VERSION.0, MIN_MERGE_TREE_VERSION.1
            ),
            Availability::NotARepo => f.write_str("not a git repository"),
            Availability::UnbornHead => f.write_str("unborn HEAD (no commits yet)"),
            Availability::DetachedHead => f.write_str("detached HEAD or rebase in progress"),
        }
    }
}

/// Report whether worktree isolation is available for `work_path`, and why
/// not when it is not. This is the reporting surface later wired into
/// `kanban check-sessions`; registration is checked by the caller.
pub fn availability(work_path: &Path) -> Availability {
    let Some(version) = git_version() else {
        return Availability::NoGit;
    };
    if version < MIN_MERGE_TREE_VERSION {
        return Availability::GitTooOld {
            found: format!("{}.{}", version.0, version.1),
        };
    }
    let Some(repo) = detect(work_path) else {
        return Availability::NotARepo;
    };
    if !repo.has_commits().unwrap_or(false) {
        return Availability::UnbornHead;
    }
    if repo.detached_head() {
        return Availability::DetachedHead;
    }
    Availability::Available
}

/// True when the installed git can run `merge-tree --write-tree`.
pub fn supports_merge_tree() -> bool {
    matches!(git_version(), Some(v) if v >= MIN_MERGE_TREE_VERSION)
}

/// `(major, minor)` from `git --version` output, e.g. `2.55` from
/// `git version 2.55.0.windows.1`.
fn git_version() -> Option<(u32, u32)> {
    let out = Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_git_version(&String::from_utf8_lossy(&out.stdout))
}

fn parse_git_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.trim().strip_prefix("git version ")?;
    let mut nums = rest.split(['.', ' ']).filter_map(|p| p.parse::<u32>().ok());
    let major = nums.next()?;
    let minor = nums.next()?;
    Some((major, minor))
}

/// Detect the repository containing `work_path` via `rev-parse --show-toplevel`.
/// `None` when git is missing, the command fails, or the path is not in a repo.
pub fn detect(work_path: &Path) -> Option<GitRepo> {
    let out = run(work_path, &["rev-parse", "--show-toplevel"], None).ok()?;
    if !out.status.success() {
        return None;
    }
    let root = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if root.as_os_str().is_empty() {
        None
    } else {
        Some(GitRepo { root })
    }
}

/// Run `git -C <dir> [-c core.quotePath=false] <args>` with `LC_ALL=C`.
/// `index` exports `GIT_INDEX_FILE` for the throwaway-index plumbing.
fn run<S: AsRef<OsStr>>(dir: &Path, args: &[S], index: Option<&Path>) -> Result<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        // Never octal-quote non-ASCII paths in parsed output (stage lines,
        // name-status records, --show-toplevel).
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    Ok(cmd.output()?)
}

/// Convert a failed git invocation into an `Invalid` error carrying stderr.
fn require(out: Output, what: &str) -> Result<Output> {
    if out.status.success() {
        return Ok(out);
    }
    Err(KanbanError::Invalid(format!(
        "git {what} failed (exit {}): {}",
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

fn s(v: &str) -> &OsStr {
    OsStr::new(v)
}

impl GitRepo {
    /// The work tree root this repo was detected at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn git(&self, args: &[&str]) -> Result<Output> {
        run(&self.root, args, None)
    }

    /// False while HEAD points at a branch with no commits (fresh `git init`).
    pub fn has_commits(&self) -> Result<bool> {
        Ok(self
            .git(&["rev-parse", "--verify", "--quiet", "HEAD"])?
            .status
            .success())
    }

    /// True when HEAD is detached (no branch checked out). A rebase in
    /// progress also detaches HEAD, so one check covers both.
    fn detached_head(&self) -> bool {
        match self.git(&["symbolic-ref", "--quiet", "HEAD"]) {
            Ok(out) => !out.status.success(),
            Err(_) => true, // conservative: isolation off when unsure
        }
    }

    /// Turn the live working directory into a commit WITHOUT touching the
    /// user's index or working tree, using a throwaway index:
    ///
    /// ```text
    /// GIT_INDEX_FILE=$tmp git read-tree <parent>
    /// GIT_INDEX_FILE=$tmp git add -A     # tracked + untracked, honors .gitignore
    /// GIT_INDEX_FILE=$tmp git write-tree
    /// git commit-tree $tree -p <parent> -m <message>
    /// ```
    ///
    /// `parent` is any commit-ish (an oid, `HEAD`, or a ref name). `git status`,
    /// `git diff --cached` and HEAD are all unchanged afterwards.
    pub fn snapshot(&self, parent: &str, message: &str) -> Result<Oid> {
        let tmp = NamedTempFile::new()?;
        let index = Some(tmp.path());
        require(run(&self.root, &["read-tree", parent], index)?, "read-tree")?;
        require(run(&self.root, &["add", "-A"], index)?, "add -A")?;
        let out = require(run(&self.root, &["write-tree"], index)?, "write-tree")?;
        let tree = parse_oid(&String::from_utf8_lossy(&out.stdout))?;
        self.commit_tree(&tree, &[parent], message)
    }

    /// Create a dangling commit from `tree` — never on any branch, so the
    /// user's refs are untouched. One `-p` per parent; an empty parent list
    /// makes a root commit (the first landing on a repo with no integration
    /// history).
    pub fn commit_tree(&self, tree: &Oid, parents: &[&str], message: &str) -> Result<Oid> {
        let mut args: Vec<&str> = vec!["commit-tree", tree.as_str(), "-m", message];
        for parent in parents {
            args.push("-p");
            args.push(parent);
        }
        let out = require(self.git(&args)?, "commit-tree")?;
        parse_oid(&String::from_utf8_lossy(&out.stdout))
    }

    /// Current value of [`INTEGRATION_REF`], `None` when it does not exist yet.
    pub fn integration_ref(&self) -> Result<Option<Oid>> {
        self.read_ref(INTEGRATION_REF)
    }

    /// Point [`INTEGRATION_REF`] at `oid`.
    pub fn set_integration_ref(&self, oid: &Oid) -> Result<()> {
        self.set_ref(INTEGRATION_REF, oid)
    }

    /// Current value of an arbitrary ref, `None` when it does not exist yet.
    /// The board passes its configured `orchestration.isolation.integration_ref`.
    pub fn read_ref(&self, refname: &str) -> Result<Option<Oid>> {
        let out = self.git(&["rev-parse", "--verify", "--quiet", refname])?;
        if out.status.success() {
            Ok(Some(parse_oid(&String::from_utf8_lossy(&out.stdout))?))
        } else {
            Ok(None)
        }
    }

    /// Point an arbitrary ref at `oid` (creating it when missing).
    pub fn set_ref(&self, refname: &str, oid: &Oid) -> Result<()> {
        require(
            self.git(&["update-ref", refname, oid.as_str()])?,
            "update-ref",
        )?;
        Ok(())
    }

    /// The commit HEAD points at (checked by [`Self::has_commits`] on the
    /// availability path, so an unborn HEAD never reaches here).
    pub fn head_oid(&self) -> Result<Oid> {
        let out = require(self.git(&["rev-parse", "HEAD"])?, "rev-parse HEAD")?;
        parse_oid(&String::from_utf8_lossy(&out.stdout))
    }

    /// Create a worktree at `path` on a new branch `branch` rooted at `base`.
    pub fn add_worktree(&self, path: &Path, branch: &str, base: &Oid) -> Result<()> {
        let args = [
            s("worktree"),
            s("add"),
            s("-b"),
            s(branch),
            path.as_os_str(),
            s(base.as_str()),
        ];
        require(run(&self.root, &args, None)?, "worktree add")?;
        Ok(())
    }

    /// Commit everything uncommitted in `worktree` (the board's own checkout,
    /// so its index may be used freely). Returns `None` when nothing changed.
    /// A resolved mid-merge state (resolver flow) produces the two-parent
    /// merge commit. An UNresolved one — conflict markers still in the index
    /// — is left open and `None` is returned: concluding it would bake the
    /// markers into the branch tip, and because the merge commit carries the
    /// conflicted snapshot as a parent, the next landing's merge-base would
    /// sit past the conflict and "cleanly" land the markered tree.
    pub fn commit_all(&self, worktree: &Path, message: &str) -> Result<Option<Oid>> {
        let unmerged = require(
            run(worktree, &["ls-files", "--unmerged", "-z"], None)?,
            "ls-files --unmerged",
        )?;
        let has_unmerged = String::from_utf8_lossy(&unmerged.stdout)
            .split('\0')
            .any(|f| !f.is_empty());
        if has_unmerged {
            return Ok(None);
        }
        require(run(worktree, &["add", "-A"], None)?, "add -A")?;
        let staged = run(worktree, &["diff", "--cached", "--quiet"], None)?;
        match staged.status.code() {
            Some(0) => return Ok(None), // nothing to commit
            Some(1) => {}
            code => {
                return Err(KanbanError::Invalid(format!(
                    "git diff --cached --quiet failed (exit {}): {}",
                    code.unwrap_or(-1),
                    String::from_utf8_lossy(&staged.stderr).trim()
                )));
            }
        }
        require(run(worktree, &["commit", "-m", message], None)?, "commit")?;
        let out = require(
            run(worktree, &["rev-parse", "HEAD"], None)?,
            "rev-parse HEAD",
        )?;
        Ok(Some(parse_oid(&String::from_utf8_lossy(&out.stdout))?))
    }

    /// 3-way merge `ours` against `theirs` entirely in the object database
    /// (`merge-tree --write-tree`, needs git >= 2.38). Touches no working
    /// tree at all: exit 0 → [`Preflight::Clean`], exit 1 →
    /// [`Preflight::Conflict`] with machine-readable stage lines parsed
    /// (never the localized prose).
    pub fn preflight(&self, ours: &Oid, theirs: &str) -> Result<Preflight> {
        let out = self.git(&["merge-tree", "--write-tree", ours.as_str(), theirs])?;
        match out.status.code() {
            Some(0) => Ok(Preflight::Clean {
                tree: parse_oid(&String::from_utf8_lossy(&out.stdout))?,
            }),
            Some(1) => {
                let stages = parse_stages(&String::from_utf8_lossy(&out.stdout));
                let mut paths: Vec<String> = Vec::new();
                for stage in &stages {
                    if !paths.contains(&stage.path) {
                        paths.push(stage.path.clone());
                    }
                }
                Ok(Preflight::Conflict { paths, stages })
            }
            code => Err(KanbanError::Invalid(format!(
                "git merge-tree failed (exit {}): {}",
                code.unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    /// Write the paths that differ between `from` and `tree` into the work
    /// tree as ordinary unstaged modifications — the landing primitive. The
    /// user's real index is untouched (`checkout-index` runs against a
    /// throwaway index), HEAD is unmoved, and only differing paths are
    /// written; deletions are removed from disk. Returns the changed paths.
    ///
    /// Race guard: before anything is written, every landing path is
    /// re-compared against `from` through a throwaway index (`read-tree` +
    /// `add -A` + `diff --cached`). A path whose on-disk content no longer
    /// matches the snapshot (the human edited it after the snapshot was
    /// taken) aborts the whole landing with an error and nothing on disk
    /// changes.
    pub fn materialize(&self, from: &Oid, tree: &Oid) -> Result<Vec<PathBuf>> {
        let out = require(
            self.git(&[
                "diff",
                "--name-status",
                "--no-renames",
                "-z",
                from.as_str(),
                tree.as_str(),
            ])?,
            "diff --name-status",
        )?;
        let diff = String::from_utf8_lossy(&out.stdout).into_owned();
        let fields: Vec<&str> = diff.split('\0').filter(|f| !f.is_empty()).collect();

        let mut changed = Vec::new();
        let mut writes = Vec::new();
        let mut deletes = Vec::new();
        for record in fields.chunks(2) {
            let [status, path] = record else {
                break;
            };
            let path = PathBuf::from(*path);
            if *status == "D" {
                deletes.push(path.clone());
            } else {
                writes.push(path.clone());
            }
            changed.push(path);
        }

        if !writes.is_empty() || !deletes.is_empty() {
            let mut landing: Vec<PathBuf> = writes.clone();
            landing.extend(deletes.iter().cloned());
            let raced = self.disk_differs_from(from, &landing)?;
            if !raced.is_empty() {
                let names = raced
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(KanbanError::Invalid(format!(
                    "landing aborted: {names} changed on disk after the snapshot was taken \
                     (concurrent edit); nothing was written"
                )));
            }
        }

        if !writes.is_empty() {
            let tmp = NamedTempFile::new()?;
            let index = Some(tmp.path());
            require(
                run(&self.root, &["read-tree", tree.as_str()], index)?,
                "read-tree",
            )?;
            let mut prefix = self.root.as_os_str().to_os_string();
            prefix.push("/");
            for path in &writes {
                require(
                    run(
                        &self.root,
                        &[
                            s("checkout-index"),
                            s("-f"),
                            s("--prefix"),
                            prefix.as_os_str(),
                            s("--"),
                            path.as_os_str(),
                        ],
                        index,
                    )?,
                    "checkout-index",
                )?;
            }
        }
        for path in &deletes {
            let target = self.root.join(path);
            if target.symlink_metadata().is_ok() {
                fs::remove_file(target)?;
            }
        }
        Ok(changed)
    }

    /// Paths from `paths` whose on-disk content no longer matches `from`'s
    /// recorded blobs. Runs `read-tree from` + `add -A -- <paths>` against a
    /// throwaway index and diffs it against `from`, so filter and attribute
    /// handling is exactly `git add`'s and the user's real index is never
    /// touched. A path that is neither in `from` nor on disk cannot have
    /// raced and is skipped (git would reject the empty pathspec). The
    /// on-disk probe joins the repo root: `paths` are repo-relative, and the
    /// kanban process almost never runs *in* the work folder, so probing them
    /// as-is would resolve against an unrelated cwd and miss a human's
    /// untracked file at that path.
    fn disk_differs_from(&self, from: &Oid, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let tracked: std::collections::HashSet<String> = {
            let mut args: Vec<&OsStr> = vec![
                OsStr::new("ls-tree"),
                OsStr::new("--name-only"),
                OsStr::new("-z"),
                OsStr::new(from.as_str()),
                OsStr::new("--"),
            ];
            args.extend(paths.iter().map(|p| p.as_os_str()));
            let out = require(run(&self.root, &args, None)?, "ls-tree")?;
            String::from_utf8_lossy(&out.stdout)
                .split('\0')
                .filter(|f| !f.is_empty())
                .map(str::to_string)
                .collect()
        };
        let candidates: Vec<PathBuf> = paths
            .iter()
            .filter(|p| {
                tracked.contains(&p.to_string_lossy().into_owned())
                    || self.root.join(p).symlink_metadata().is_ok()
            })
            .cloned()
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let tmp = NamedTempFile::new()?;
        let index = Some(tmp.path());
        require(
            run(&self.root, &["read-tree", from.as_str()], index)?,
            "read-tree",
        )?;
        let mut args: Vec<OsString> = vec![
            OsStr::new("add").to_os_string(),
            OsStr::new("-A").to_os_string(),
            OsStr::new("--").to_os_string(),
        ];
        args.extend(candidates.iter().map(|p| p.as_os_str().to_os_string()));
        require(run(&self.root, &args, index)?, "add")?;
        let out = require(
            run(
                &self.root,
                &["diff", "--cached", "--name-only", "-z", from.as_str()],
                index,
            )?,
            "diff --cached",
        )?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|f| !f.is_empty())
            .map(PathBuf::from)
            .collect())
    }

    /// Merge `refname` INTO the task's own worktree (the resolver flow):
    /// conflict markers appear only inside that isolated checkout, the
    /// user's `work_path` stays untouched. A clean merge commits; conflicts
    /// stop with markers and are not an error.
    pub fn merge_into_worktree(&self, worktree: &Path, refname: &str) -> Result<()> {
        let out = run(worktree, &["merge", "--no-edit", refname], None)?;
        match out.status.code() {
            Some(0) | Some(1) => Ok(()),
            code => Err(KanbanError::Invalid(format!(
                "git merge failed (exit {}): {}",
                code.unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    /// True when `branch` is fully merged into [`INTEGRATION_REF`]. This —
    /// never `HEAD` — is the gate that makes a task branch deletable.
    pub fn is_landed(&self, branch: &str) -> Result<bool> {
        let out = self.git(&["merge-base", "--is-ancestor", branch, INTEGRATION_REF])?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            code => Err(KanbanError::Invalid(format!(
                "git merge-base --is-ancestor failed (exit {}): {}",
                code.unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    /// Remove a kanban worktree. Git refuses on a dirty worktree, so a force
    /// flag is explicit at the call site (cleanup keeps unmerged work only
    /// in `Conflict` state, which callers must not route here).
    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        if force {
            require(
                run(
                    &self.root,
                    &[s("worktree"), s("remove"), s("--force"), path.as_os_str()],
                    None,
                )?,
                "worktree remove --force",
            )?;
        } else {
            require(
                run(
                    &self.root,
                    &[s("worktree"), s("remove"), path.as_os_str()],
                    None,
                )?,
                "worktree remove",
            )?;
        }
        Ok(())
    }

    /// Delete a task branch with `-D`. With `allow_unmerged: false` — the
    /// automatic paths — the deletion happens only after the
    /// [`Self::is_landed`] check passes: `branch -d` refuses here (merged
    /// into [`INTEGRATION_REF`], not `HEAD`), and `-D` without the check
    /// could destroy unmerged agent work. `allow_unmerged: true` is for the
    /// terminal human decisions: dropping a task (abandon) or accepting it
    /// as finished (Done) discards its branch with it, landed or not. A
    /// branch that does not exist is already gone.
    pub fn delete_branch(&self, branch: &str, allow_unmerged: bool) -> Result<()> {
        let head_ref = format!("refs/heads/{branch}");
        let out = self.git(&["rev-parse", "--verify", "--quiet", &head_ref])?;
        if !out.status.success() {
            return Ok(());
        }
        if !allow_unmerged && !self.is_landed(branch)? {
            return Err(KanbanError::Invalid(format!(
                "refusing to delete {branch}: not merged into {INTEGRATION_REF}"
            )));
        }
        require(self.git(&["branch", "-D", branch])?, "branch -D")?;
        Ok(())
    }

    /// Task branches under `prefix` (e.g. `kanban/`) as `(branch, task_id)`
    /// pairs — the enumeration the GC pass walks: a branch whose task id no
    /// longer exists is a leftover. Enumerates every head and filters in
    /// Rust, so a prefix containing glob metacharacters stays literal.
    pub fn branches_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let out = require(
            self.git(&["for-each-ref", "--format=%(refname:short)", "refs/heads/"])?,
            "for-each-ref",
        )?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter_map(|branch| {
                let id = branch.strip_prefix(prefix)?;
                Some((branch.to_string(), id.to_string()))
            })
            .collect())
    }

    /// Reconcile worktrees whose directory was deleted by hand. Safe GC pass.
    pub fn prune_worktrees(&self) -> Result<()> {
        require(self.git(&["worktree", "prune"])?, "worktree prune")?;
        Ok(())
    }
}

/// Parse the machine-readable conflict section of `merge-tree --write-tree`
/// output: line 1 is the tree oid, then `100644 <oid> <1|2|3> TAB <path>`
/// records, then a blank line and localized prose (never parsed).
fn parse_stages(text: &str) -> Vec<Stage> {
    let mut stages = Vec::new();
    for line in text.lines().skip(1) {
        if line.trim().is_empty() {
            if stages.is_empty() {
                continue; // separator between the tree oid and the records
            }
            break; // end of the conflicted-file section
        }
        let Some((meta, path)) = line.split_once('\t') else {
            break;
        };
        let mut fields = meta.split_whitespace();
        let (Some(mode), Some(oid), Some(stage)) = (fields.next(), fields.next(), fields.next())
        else {
            break;
        };
        let Ok(mode) = mode.parse::<u32>() else {
            break;
        };
        let Ok(stage) = stage.parse::<u8>() else {
            break;
        };
        let Some(oid) = oid_from(oid) else {
            break;
        };
        if !(1..=3).contains(&stage) {
            break;
        }
        stages.push(Stage {
            mode,
            oid,
            stage,
            path: path.to_string(),
        });
    }
    stages
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as TestCommand;
    use tempfile::TempDir;

    fn raw(dir: &Path, args: &[&str]) -> Output {
        TestCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .expect("git spawn")
    }

    fn ok(dir: &Path, args: &[&str]) -> String {
        let out = raw(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        ok(dir.path(), &["init", "-q", "-b", "main"]);
        ok(dir.path(), &["config", "user.email", "kanban@example.test"]);
        ok(dir.path(), &["config", "user.name", "Kanban Test"]);
        dir
    }

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn commit_all(dir: &Path, message: &str) {
        ok(dir, &["add", "-A"]);
        ok(dir, &["commit", "-q", "-m", message]);
    }

    fn head(dir: &Path) -> String {
        ok(dir, &["rev-parse", "HEAD"])
    }

    fn repo(dir: &TempDir) -> GitRepo {
        detect(dir.path()).unwrap()
    }

    fn has_blob(r: &GitRepo, rev: &str, path: &str) -> bool {
        r.git(&["cat-file", "-e", &format!("{rev}:{path}")])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn parse_git_versions() {
        assert_eq!(parse_git_version("git version 2.55.0\n"), Some((2, 55)));
        assert_eq!(
            parse_git_version("git version 2.38.0.windows.1"),
            Some((2, 38))
        );
        assert_eq!(parse_git_version("git version 2.37.4"), Some((2, 37)));
        assert_eq!(parse_git_version("git version 1.8.3.1"), Some((1, 8)));
        assert_eq!(parse_git_version(""), None);
        assert_eq!(parse_git_version("not git output"), None);
    }

    #[test]
    fn supports_merge_tree_matches_parsed_version() {
        if let Some(v) = git_version() {
            assert_eq!(supports_merge_tree(), v >= MIN_MERGE_TREE_VERSION);
        }
    }

    #[test]
    fn detect_finds_toplevel_from_subdirectory() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "init");
        let sub = dir.path().join("src/inner");
        fs::create_dir_all(&sub).unwrap();
        assert!(detect(&sub).is_some());
        assert_eq!(detect(&sub).unwrap().root(), dir.path());
        let empty = tempfile::tempdir().unwrap();
        assert!(detect(empty.path()).is_none());
    }

    #[test]
    fn snapshot_captures_live_state_and_leaves_worktree_untouched() {
        let dir = init_repo();
        write(dir.path(), "a.txt", "one\n");
        commit_all(dir.path(), "init");
        write(dir.path(), "a.txt", "two\n");
        write(dir.path(), "b.txt", "new\n");
        write(dir.path(), ".gitignore", "target/\n");
        write(dir.path(), "target/c.txt", "ignored\n");

        let status_before = ok(dir.path(), &["status", "--porcelain"]);
        let cached_before = ok(dir.path(), &["diff", "--cached"]);
        let head_before = head(dir.path());

        let r = repo(&dir);
        let snap = r.snapshot("HEAD", "kanban: snapshot test").unwrap();
        let rev = snap.as_str();

        assert_eq!(ok(dir.path(), &["show", &format!("{rev}:a.txt")]), "two");
        assert_eq!(ok(dir.path(), &["show", &format!("{rev}:b.txt")]), "new");
        assert_eq!(
            ok(dir.path(), &["show", &format!("{rev}:.gitignore")]),
            "target/"
        );
        assert!(!has_blob(&r, rev, "target/c.txt"));
        assert_eq!(status_before, ok(dir.path(), &["status", "--porcelain"]));
        assert_eq!(cached_before, ok(dir.path(), &["diff", "--cached"]));
        assert_eq!(head_before, head(dir.path()));
    }

    #[test]
    fn snapshot_chain_keeps_merge_base_at_snapshot_not_head() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "v1\n");
        commit_all(dir.path(), "init");
        let base_head = head(dir.path());

        let r = repo(&dir);
        let snap1 = r.snapshot("HEAD", "snap 1").unwrap();
        write(dir.path(), "f.txt", "v2\n");
        let snap2 = r.snapshot(snap1.as_str(), "snap 2").unwrap();

        let mb = ok(dir.path(), &["merge-base", snap1.as_str(), snap2.as_str()]);
        assert_eq!(mb, snap1.as_str());
        assert_ne!(mb, base_head);
    }

    #[test]
    fn preflight_clean_merge_touches_nothing() {
        let dir = init_repo();
        write(dir.path(), "hero.txt", "base\n");
        commit_all(dir.path(), "base");
        ok(dir.path(), &["checkout", "-q", "-b", "side"]);
        write(dir.path(), "side.txt", "side\n");
        commit_all(dir.path(), "side work");
        ok(dir.path(), &["checkout", "-q", "main"]);
        write(dir.path(), "main.txt", "main\n");
        commit_all(dir.path(), "main work");

        let status_before = ok(dir.path(), &["status", "--porcelain"]);
        let r = repo(&dir);
        let ours = r.snapshot("HEAD", "W").unwrap();

        match r.preflight(&ours, "side").unwrap() {
            Preflight::Clean { tree } => {
                let t = tree.as_str();
                assert!(has_blob(&r, t, "hero.txt"));
                assert!(has_blob(&r, t, "main.txt"));
                assert!(has_blob(&r, t, "side.txt"));
            }
            other => panic!("expected clean merge, got {other:?}"),
        }
        assert_eq!(status_before, ok(dir.path(), &["status", "--porcelain"]));
    }

    #[test]
    fn preflight_conflicting_merge_reports_stages() {
        let dir = init_repo();
        write(dir.path(), "hero.txt", "one\nbase\n");
        commit_all(dir.path(), "base");
        ok(dir.path(), &["checkout", "-q", "-b", "side"]);
        write(dir.path(), "hero.txt", "one\nSIDE\n");
        commit_all(dir.path(), "side edit");
        ok(dir.path(), &["checkout", "-q", "main"]);
        write(dir.path(), "hero.txt", "one\nMAIN\n");
        commit_all(dir.path(), "main edit");

        let r = repo(&dir);
        let ours = r.snapshot("HEAD", "W").unwrap();

        match r.preflight(&ours, "side").unwrap() {
            Preflight::Conflict { paths, stages } => {
                assert_eq!(paths, vec!["hero.txt".to_string()]);
                assert_eq!(stages.len(), 3);
                assert_eq!(stages[0].stage, 1);
                assert_eq!(stages[1].stage, 2);
                assert_eq!(stages[2].stage, 3);
                let ours_blob = ok(dir.path(), &["cat-file", "blob", stages[1].oid.as_str()]);
                assert_eq!(ours_blob, "one\nMAIN");
            }
            other => panic!("expected conflict, got {other:?}"),
        }
        assert_eq!("", ok(dir.path(), &["status", "--porcelain"]));
    }

    #[test]
    fn parse_stages_ignores_localized_prose() {
        let (a, b, c, d) = (
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40),
            "d".repeat(40),
        );
        let text = format!(
            "e629b626a39c10f7d4fcff99a642a0dc033dcceb\n\
             100644 {a} 1\thero.txt\n\
             100644 {b} 2\thero.txt\n\
             100644 {c} 3\thero.txt\n\
             \n\
             КОНФЛИКТ (содержимое): мусор\n\
             100644 {d} 9\tfake.txt\n"
        );
        let stages = parse_stages(&text);
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].path, "hero.txt");
        assert_eq!(stages[2].stage, 3);
        assert_eq!(stages[0].oid, Oid(a));
        assert_eq!(stages[0].mode, 100644);
    }

    #[test]
    fn unborn_head_reports_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        ok(dir.path(), &["init", "-q", "-b", "main"]);
        ok(dir.path(), &["config", "user.email", "kanban@example.test"]);
        ok(dir.path(), &["config", "user.name", "Kanban Test"]);
        let r = detect(dir.path()).unwrap();
        assert!(!r.has_commits().unwrap());
        assert_eq!(availability(dir.path()), Availability::UnbornHead);
        assert!(r.snapshot("HEAD", "snap").is_err());
    }

    #[test]
    fn detached_head_reports_unavailable() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "init");
        ok(dir.path(), &["checkout", "-q", "--detach"]);
        assert_eq!(availability(dir.path()), Availability::DetachedHead);
    }

    #[test]
    fn availability_on_non_repo_and_good_repo() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(availability(empty.path()), Availability::NotARepo);

        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "init");
        assert_eq!(availability(dir.path()), Availability::Available);
        assert!(availability(dir.path()).is_available());
    }

    #[test]
    fn git_too_old_message_names_the_requirement() {
        let a = Availability::GitTooOld {
            found: "2.35.1".to_string(),
        };
        assert!(!a.is_available());
        assert!(a.to_string().contains("2.35.1"));
        assert!(a.to_string().contains("2.38"));
    }

    #[test]
    fn integration_ref_roundtrip() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "init");
        let r = repo(&dir);
        assert_eq!(r.integration_ref().unwrap(), None);
        let snap = r.snapshot("HEAD", "S").unwrap();
        r.set_integration_ref(&snap).unwrap();
        assert_eq!(r.integration_ref().unwrap(), Some(snap));
    }

    #[test]
    fn worktree_remove_on_dirty_worktree_needs_force() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "init");
        let r = repo(&dir);
        let base = r.snapshot("HEAD", "S").unwrap();

        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("TASK-X");
        r.add_worktree(&wt_path, "kanban/TASK-X", &base).unwrap();
        assert!(wt_path.join("f.txt").exists());

        write(&wt_path, "dirty.txt", "uncommitted\n");
        assert!(r.remove_worktree(&wt_path, false).is_err());
        assert!(wt_path.exists());
        r.remove_worktree(&wt_path, true).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn commit_all_noops_when_clean_and_advances_branch_when_dirty() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "init");
        let r = repo(&dir);
        let base = r.snapshot("HEAD", "S").unwrap();

        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("TASK-X");
        r.add_worktree(&wt_path, "kanban/TASK-X", &base).unwrap();

        assert_eq!(r.commit_all(&wt_path, "noop").unwrap(), None);

        write(&wt_path, "agent.txt", "work\n");
        write(&wt_path, "f.txt", "changed\n");
        let oid = r.commit_all(&wt_path, "agent work").unwrap().unwrap();

        assert_eq!(ok(&wt_path, &["rev-parse", "HEAD"]), oid.as_str());
        assert_eq!(
            ok(dir.path(), &["rev-parse", "kanban/TASK-X"]),
            oid.as_str()
        );
        assert_eq!(
            ok(&wt_path, &["show", &format!("{}:agent.txt", oid.as_str())]),
            "work"
        );
    }

    #[test]
    fn materialize_writes_only_changed_paths_without_touching_index_or_head() {
        let dir = init_repo();
        write(dir.path(), "f1.txt", "v1\n");
        write(dir.path(), "f2.txt", "v2\n");
        commit_all(dir.path(), "base");
        let r = repo(&dir);
        let from = Oid(head(dir.path()));

        write(dir.path(), "f1.txt", "v1-mod\n");
        fs::remove_file(dir.path().join("f2.txt")).unwrap();
        write(dir.path(), "f3.txt", "added\n");
        let snap = r.snapshot("HEAD", "S").unwrap();
        let tree = Oid(ok(
            dir.path(),
            &["rev-parse", &format!("{}^{{tree}}", snap.as_str())],
        ));

        ok(dir.path(), &["reset", "--hard", "-q"]);
        fs::remove_file(dir.path().join("f3.txt")).unwrap();

        let mut changed = r.materialize(&from, &tree).unwrap();
        changed.sort();
        assert_eq!(
            changed,
            vec![
                PathBuf::from("f1.txt"),
                PathBuf::from("f2.txt"),
                PathBuf::from("f3.txt")
            ]
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("f1.txt")).unwrap(),
            "v1-mod\n"
        );
        assert!(
            !dir.path().join("f2.txt").exists(),
            "deleted path removed from disk"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("f3.txt")).unwrap(),
            "added\n"
        );
        assert_eq!(head(dir.path()), from.as_str());
        assert_eq!("", ok(dir.path(), &["diff", "--cached"]));
    }

    #[test]
    fn materialize_aborts_when_a_landing_path_raced_the_snapshot() {
        let dir = init_repo();
        write(dir.path(), "land.txt", "base\n");
        write(dir.path(), "other.txt", "keep\n");
        commit_all(dir.path(), "base");
        let r = repo(&dir);

        // W captures the human's disk, then the task branch changes land.txt
        // in its own worktree (only in the object database as far as the
        // user's tree is concerned).
        let w = r.snapshot("HEAD", "W").unwrap();
        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("TASK-X");
        r.add_worktree(&wt_path, "kanban/TASK-X", &w).unwrap();
        write(&wt_path, "land.txt", "landed\n");
        r.commit_all(&wt_path, "task edit").unwrap();
        let Preflight::Clean { tree } = r.preflight(&w, "kanban/TASK-X").unwrap() else {
            panic!("expected a clean merge");
        };

        // The human edits the same path after W was taken: the whole land
        // aborts before anything is written.
        write(dir.path(), "land.txt", "human raced here\n");
        let head_before = head(dir.path());

        let err = r.materialize(&w, &tree).unwrap_err();
        assert!(err.to_string().contains("land.txt"), "{err}");
        assert!(err.to_string().contains("nothing was written"), "{err}");
        assert_eq!(
            fs::read_to_string(dir.path().join("land.txt")).unwrap(),
            "human raced here\n",
            "the racing edit is untouched"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("other.txt")).unwrap(),
            "keep\n"
        );
        assert_eq!(head(dir.path()), head_before);
        assert_eq!("", ok(dir.path(), &["diff", "--cached"]));
    }

    #[test]
    fn materialize_aborts_when_an_untracked_landing_path_exists_on_disk() {
        let dir = init_repo();
        write(dir.path(), "base.txt", "base\n");
        commit_all(dir.path(), "base");
        let r = repo(&dir);

        // The task branch adds a file that W never knew about.
        let w = r.snapshot("HEAD", "W").unwrap();
        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("TASK-X");
        r.add_worktree(&wt_path, "kanban/TASK-X", &w).unwrap();
        write(&wt_path, "brand-new.txt", "from the task\n");
        r.commit_all(&wt_path, "task edit").unwrap();
        let Preflight::Clean { tree } = r.preflight(&w, "kanban/TASK-X").unwrap() else {
            panic!("expected a clean merge");
        };

        // The human creates the same path by hand after W was taken. It is
        // absent from W, so only the on-disk probe can catch it — and that
        // probe must resolve against the repo root, not the process cwd.
        write(dir.path(), "brand-new.txt", "the human wrote this\n");

        let err = r.materialize(&w, &tree).unwrap_err();
        assert!(err.to_string().contains("brand-new.txt"), "{err}");
        assert_eq!(
            fs::read_to_string(dir.path().join("brand-new.txt")).unwrap(),
            "the human wrote this\n",
            "the human's untracked file is never silently overwritten"
        );
    }

    #[test]
    fn materialize_race_guard_ignores_unrelated_disk_edits() {
        let dir = init_repo();
        write(dir.path(), "land.txt", "base\n");
        write(dir.path(), "other.txt", "keep\n");
        commit_all(dir.path(), "base");
        let r = repo(&dir);

        let w = r.snapshot("HEAD", "W").unwrap();
        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("TASK-X");
        r.add_worktree(&wt_path, "kanban/TASK-X", &w).unwrap();
        write(&wt_path, "land.txt", "landed\n");
        write(&wt_path, "brand-new.txt", "new file\n");
        r.commit_all(&wt_path, "task edit").unwrap();
        let Preflight::Clean { tree } = r.preflight(&w, "kanban/TASK-X").unwrap() else {
            panic!("expected a clean merge");
        };

        // An edit to a path OUTSIDE the landing set never blocks the land,
        // and a land-set path that is new in the tree (absent from W and
        // absent on disk) is skipped, not a race.
        write(dir.path(), "other.txt", "human typed\n");

        let mut changed = r.materialize(&w, &tree).unwrap();
        changed.sort();
        assert_eq!(
            changed,
            vec![PathBuf::from("brand-new.txt"), PathBuf::from("land.txt")]
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("land.txt")).unwrap(),
            "landed\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("brand-new.txt")).unwrap(),
            "new file\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("other.txt")).unwrap(),
            "human typed\n"
        );
    }

    #[test]
    fn commit_tree_builds_multi_parent_commits_off_branch() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "base");
        let r = repo(&dir);

        let a = r.snapshot("HEAD", "a").unwrap();
        let b = r.snapshot("HEAD", "b").unwrap();
        let tree = Oid(ok(
            dir.path(),
            &["rev-parse", &format!("{}^{{tree}}", b.as_str())],
        ));
        let head_before = head(dir.path());
        let merge = r.commit_tree(&tree, &[a.as_str(), "HEAD"], "land").unwrap();

        assert_eq!(
            ok(dir.path(), &["rev-parse", &format!("{merge}^1")]),
            a.as_str()
        );
        assert_eq!(
            ok(dir.path(), &["rev-parse", &format!("{merge}^2")]),
            head_before
        );
        assert_eq!(ok(dir.path(), &["show", &format!("{merge}:f.txt")]), "x");
        // No ref moved.
        assert_eq!(head(dir.path()), head_before);
    }

    #[test]
    fn merge_into_worktree_conflicts_only_in_worktree() {
        let dir = init_repo();
        write(dir.path(), "hero.txt", "one\ntwo\n");
        commit_all(dir.path(), "base");
        let r = repo(&dir);
        let base = r.snapshot("HEAD", "S").unwrap();

        write(dir.path(), "hero.txt", "one\nMAIN\n");
        commit_all(dir.path(), "main edit");
        let main_tip = Oid(head(dir.path()));

        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("TASK-X");
        r.add_worktree(&wt_path, "kanban/TASK-X", &base).unwrap();
        write(&wt_path, "hero.txt", "one\nTASK\n");
        assert!(r.commit_all(&wt_path, "task edit").unwrap().is_some());

        r.set_integration_ref(&main_tip).unwrap();
        r.merge_into_worktree(&wt_path, INTEGRATION_REF).unwrap();

        let wt_hero = fs::read_to_string(wt_path.join("hero.txt")).unwrap();
        assert!(wt_hero.contains("<<<<<<<"));
        assert!(wt_hero.contains("TASK"));
        assert!(wt_hero.contains("MAIN"));
        assert_eq!(
            fs::read_to_string(dir.path().join("hero.txt")).unwrap(),
            "one\nMAIN\n"
        );
        assert!(ok(&wt_path, &["status", "--porcelain"]).contains("UU hero.txt"));
    }

    #[test]
    fn delete_branch_gated_on_landed() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "base");
        ok(dir.path(), &["checkout", "-q", "-b", "task"]);
        write(dir.path(), "task.txt", "work\n");
        commit_all(dir.path(), "task work");
        ok(dir.path(), &["checkout", "-q", "main"]);

        let r = repo(&dir);
        let task_tip = Oid(ok(dir.path(), &["rev-parse", "task"]));
        let main_tip = Oid(head(dir.path()));
        r.set_integration_ref(&main_tip).unwrap();

        assert!(!r.is_landed("task").unwrap());
        assert!(r.delete_branch("task", false).is_err());
        assert!(
            raw(
                dir.path(),
                &["rev-parse", "--verify", "-q", "refs/heads/task"]
            )
            .status
            .success()
        );

        // A terminal human decision (abandon, Done) may discard an
        // unmerged branch.
        r.delete_branch("task", true).unwrap();
        assert!(
            !raw(
                dir.path(),
                &["rev-parse", "--verify", "-q", "refs/heads/task"]
            )
            .status
            .success()
        );
        // A branch that does not exist is already gone.
        r.delete_branch("task", true).unwrap();

        ok(dir.path(), &["branch", "task", task_tip.as_str()]);
        r.set_integration_ref(&task_tip).unwrap();
        assert!(r.is_landed("task").unwrap());
        r.delete_branch("task", false).unwrap();
        assert!(
            !raw(
                dir.path(),
                &["rev-parse", "--verify", "-q", "refs/heads/task"]
            )
            .status
            .success()
        );
    }

    #[test]
    fn branches_with_prefix_pairs_branches_with_task_ids() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "base");
        let r = repo(&dir);
        let base = r.snapshot("HEAD", "S").unwrap();

        assert!(r.branches_with_prefix("kanban/").unwrap().is_empty());
        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("TASK-X");
        r.add_worktree(&wt_path, "kanban/TASK-X", &base).unwrap();
        ok(dir.path(), &["branch", "unrelated"]);

        let pairs = r.branches_with_prefix("kanban/").unwrap();
        assert_eq!(
            pairs,
            vec![("kanban/TASK-X".to_string(), "TASK-X".to_string())]
        );
    }

    #[test]
    fn prune_removes_stale_worktrees() {
        let dir = init_repo();
        write(dir.path(), "f.txt", "x\n");
        commit_all(dir.path(), "init");
        let r = repo(&dir);
        let base = r.snapshot("HEAD", "S").unwrap();

        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("TASK-X");
        r.add_worktree(&wt_path, "kanban/TASK-X", &base).unwrap();
        let count = |dir: &Path| {
            ok(dir, &["worktree", "list", "--porcelain"])
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count()
        };
        assert_eq!(count(dir.path()), 2);

        fs::remove_dir_all(&wt_path).unwrap();
        assert_eq!(count(dir.path()), 2, "stale worktree listed until pruned");
        r.prune_worktrees().unwrap();
        assert_eq!(count(dir.path()), 1);
    }
}
