//! Central projects store: the registry that maps a code folder (`work_path`)
//! to the board data that belongs to it (`data_root`).
//!
//! Layout (see `docs/plans/TASK-163-projects-store.md` §2):
//!
//! ```text
//! <store>/                      # KANBAN_HOME > XDG_DATA_HOME/kanban4ai > ~/.local/share/kanban4ai
//! ├── .lock                     # flock, serializes registry mutations
//! └── projects/
//!     └── <id>/                 # data_root — slug of the folder name, deduped
//!         ├── project.yaml      # the registration; per-project files are the source of truth
//!         └── .kanban/          # byte-for-byte the current board layout
//! ```
//!
//! There is deliberately no central index file and no marker file inside the
//! work folder: a project directory is self-describing, and listing is a scan
//! of `projects/*/project.yaml`.
//!
//! [`ProjectStore::add`] / [`ProjectStore::add_with`] register a folder and,
//! when `<work>/.kanban` exists, relocate that board into the store (rename
//! first, verified copy on `EXDEV`; `--copy` leaves the source in place).

use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::core::error::{KanbanError, Result};
use crate::core::storage::{BoardGuard, atomic_write_text, lock_file_reentrant};
use crate::core::timefmt;

/// Explicit store-root override. Mandatory in tests so a suite never reads or
/// writes the developer's real store.
pub const STORE_ROOT_ENV: &str = "KANBAN_HOME";

/// Selected project for a command. Exported by the agent wrapper so callbacks
/// resolve even when the agent `cd`s elsewhere.
pub const PROJECT_ENV: &str = "KANBAN_PROJECT";

const XDG_DATA_HOME_ENV: &str = "XDG_DATA_HOME";
const HOME_ENV: &str = "HOME";
const STORE_DIR_NAME: &str = "kanban4ai";
const PROJECTS_DIR: &str = "projects";
const PROJECT_FILE: &str = "project.yaml";
/// An unregistered-but-kept project: `remove` without `purge_data` renames
/// `project.yaml` to this, so re-adding the same folder picks its board back up.
const REMOVED_PROJECT_FILE: &str = "project.yaml.removed";
const LOCK_FILE: &str = ".lock";
const MAX_ID_LEN: usize = 64;
const MAX_ID_SUFFIX: usize = 1000;

/// A registered project: one board (`data_root`) bound to one code folder
/// (`work_path`, the working directory agents are launched in).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub work_path: PathBuf,
    pub data_root: PathBuf,
    pub created_at: NaiveDateTime,
    pub last_opened_at: Option<NaiveDateTime>,
    /// Where the board was migrated from, for auditing only.
    pub migrated_from: Option<PathBuf>,
}

impl Project {
    /// The board directory — what `Storage`/`Config`/`ThreadManager` see as
    /// `<root>/.kanban`.
    pub fn kanban_dir(&self) -> PathBuf {
        self.data_root.join(".kanban")
    }

    /// A project whose work folder was deleted or moved stays listed and
    /// readable; only agent launches need the folder to exist.
    pub fn work_path_missing(&self) -> bool {
        !self.work_path.is_dir()
    }
}

/// The two roots every path in a running command resolves against: the board
/// data root (`<data_root>/.kanban`) and the folder agents are launched in.
///
/// A bare path converts into `Roots` with both roles pointing at it, which is
/// exactly a board used in place — the pre-store behavior, and what
/// `Operations::new` still means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Roots<'a> {
    /// Holds `.kanban`; what `Storage`, `Config`, `ThreadManager`, … see.
    pub data_root: &'a Path,
    /// Agent working directory: the code folder.
    pub work_path: &'a Path,
    /// Registered project id, exported to agents as `KANBAN_PROJECT` so their
    /// callbacks resolve unambiguously. `None` for an unregistered board.
    pub project_id: Option<&'a str>,
}

impl<'a> Roots<'a> {
    /// A board used in place: the code folder is also the data root.
    pub fn same(path: &'a Path) -> Self {
        Roots {
            data_root: path,
            work_path: path,
            project_id: None,
        }
    }

    pub fn new(data_root: &'a Path, work_path: &'a Path, project_id: Option<&'a str>) -> Self {
        Roots {
            data_root,
            work_path,
            project_id,
        }
    }

    /// The board directory itself.
    pub fn kanban_dir(&self) -> PathBuf {
        self.data_root.join(".kanban")
    }

    /// An absolute `<data_root>/.kanban/<relative>`. Agent-facing text must use
    /// this: an agent's cwd is the code folder, so a relative `.kanban/…` path
    /// would land in the user's repo instead of the board.
    pub fn data_path(&self, relative: impl AsRef<Path>) -> PathBuf {
        normalize_path(&self.kanban_dir()).join(relative)
    }

    /// Whether the board lives outside the code folder.
    pub fn is_split(&self) -> bool {
        normalize_path(self.data_root) != normalize_path(self.work_path)
    }
}

impl<'a> From<&'a Path> for Roots<'a> {
    fn from(path: &'a Path) -> Self {
        Roots::same(path)
    }
}

impl<'a> From<&'a PathBuf> for Roots<'a> {
    fn from(path: &'a PathBuf) -> Self {
        Roots::same(path.as_path())
    }
}

impl<'a> From<&'a Project> for Roots<'a> {
    fn from(project: &'a Project) -> Self {
        Roots::new(
            &project.data_root,
            &project.work_path,
            Some(project.id.as_str()),
        )
    }
}

/// Outcome of [`ProjectStore::add`].
#[derive(Debug, Clone)]
pub struct Added {
    pub project: Project,
    /// `false` when the folder was already registered (the call was a no-op).
    pub created: bool,
    /// `true` when a previously unregistered project directory was reclaimed,
    /// board data and id included.
    pub restored: bool,
}

/// How [`ProjectStore::add_with`] treats an existing `<work>/.kanban`.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// Copy the board into the store and leave the source in place.
    pub copy: bool,
    /// Relocate even when the source board has live agent sessions.
    pub force: bool,
    /// Skip `rename(2)` and take the verified-copy path (the `EXDEV` fallback).
    pub force_copy: bool,
}

/// On-disk form of `project.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectFile {
    id: String,
    name: String,
    work_path: PathBuf,
    #[serde(with = "timefmt::serde_naive")]
    created_at: NaiveDateTime,
    #[serde(
        default,
        with = "timefmt::serde_naive_opt",
        skip_serializing_if = "Option::is_none"
    )]
    last_opened_at: Option<NaiveDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    migrated_from: Option<PathBuf>,
}

/// Bind an on-disk record to the directory it was read from. The directory
/// name wins over the recorded `id`, so a project directory copied or renamed
/// by hand stays self-consistent.
fn project_from_dir(data_root: PathBuf, file: ProjectFile) -> Project {
    let id = data_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or(file.id);
    Project {
        id,
        name: file.name,
        work_path: file.work_path,
        data_root,
        created_at: file.created_at,
        last_opened_at: file.last_opened_at,
        migrated_from: file.migrated_from,
    }
}

impl From<&Project> for ProjectFile {
    fn from(project: &Project) -> Self {
        ProjectFile {
            id: project.id.clone(),
            name: project.name.clone(),
            work_path: project.work_path.clone(),
            created_at: project.created_at,
            last_opened_at: project.last_opened_at,
            migrated_from: project.migrated_from.clone(),
        }
    }
}

pub struct ProjectStore {
    root: PathBuf,
}

impl ProjectStore {
    /// Resolve the store root from the environment and create it.
    pub fn open() -> Result<Self> {
        let store = ProjectStore::at(store_root()?);
        fs::create_dir_all(store.projects_dir())?;
        Ok(store)
    }

    /// Store rooted at an explicit path; nothing is created until a mutation.
    pub fn at(root: impl AsRef<Path>) -> Self {
        ProjectStore {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join(PROJECTS_DIR)
    }

    /// All registered projects, sorted by display name (then id, for a stable
    /// order between projects that share a name). Directories without a
    /// readable `project.yaml` are skipped rather than failing the listing, so
    /// one damaged registration cannot take the whole board list down.
    pub fn list(&self) -> Result<Vec<Project>> {
        let entries = match fs::read_dir(self.projects_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut projects: Vec<Project> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .filter_map(|data_root| {
                let file = read_project_file(&data_root.join(PROJECT_FILE))
                    .ok()
                    .flatten()?;
                Some(project_from_dir(data_root, file))
            })
            .collect();
        projects.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(projects)
    }

    pub fn get(&self, id: &str) -> Result<Option<Project>> {
        let data_root = self.projects_dir().join(id);
        // Reject ids that would escape the store (`..`, absolute paths).
        if data_root.parent() != Some(self.projects_dir().as_path()) {
            return Ok(None);
        }
        Ok(read_project_file(&data_root.join(PROJECT_FILE))?
            .map(|file| project_from_dir(data_root, file)))
    }

    /// Look a project up the way a user names one: by id, by display name
    /// (case-insensitive), or by work path. First match in listing order.
    pub fn find(&self, needle: &str) -> Result<Option<Project>> {
        if let Some(project) = self.get(needle)? {
            return Ok(Some(project));
        }
        let projects = self.list()?;
        let lowered = needle.to_lowercase();
        if let Some(project) = projects.iter().find(|p| p.name.to_lowercase() == lowered) {
            return Ok(Some(project.clone()));
        }
        let wanted = normalize_path(Path::new(needle));
        Ok(projects
            .into_iter()
            .find(|p| normalize_path(&p.work_path) == wanted))
    }

    /// Register `work_path`. Idempotent: an already-registered folder is
    /// returned as-is (`created: false`) instead of erroring, which is what
    /// `init` needs. A project directory left behind by an unregistering
    /// `remove` is reclaimed, id and board data included (`restored: true`).
    pub fn add(&self, work_path: &Path, name: Option<&str>) -> Result<Added> {
        self.add_with(work_path, name, AddOptions::default())
    }

    /// [`add`] with move/copy options for an existing `<work>/.kanban`.
    pub fn add_with(
        &self,
        work_path: &Path,
        name: Option<&str>,
        opts: AddOptions,
    ) -> Result<Added> {
        let work_path = normalize_path(work_path);
        if !work_path.is_dir() {
            return Err(KanbanError::Invalid(format!(
                "not a directory: {}",
                work_path.display()
            )));
        }
        let _guard = self.lock()?;

        if let Some(project) = self
            .list()?
            .into_iter()
            .find(|p| normalize_path(&p.work_path) == work_path)
        {
            return Ok(Added {
                project,
                created: false,
                restored: false,
            });
        }

        let (id, reclaimed) = self.allocate_id(&work_path)?;
        let data_root = self.projects_dir().join(&id);
        fs::create_dir_all(&data_root)?;

        let source_kanban = work_path.join(".kanban");
        let dest_kanban = data_root.join(".kanban");
        let mut relocated = false;
        if source_kanban.is_dir() && !dest_kanban.exists() {
            if !opts.force && crate::core::migrate::board_has_live_sessions(&work_path) {
                let _ = fs::remove_dir(&data_root);
                return Err(KanbanError::ActiveSessions(source_kanban));
            }
            if let Err(err) =
                crate::core::migrate::relocate_board(&source_kanban, &dest_kanban, &opts)
            {
                let _ = fs::remove_dir_all(&dest_kanban);
                let _ = fs::remove_dir(&data_root);
                return Err(err);
            }
            relocated = true;
        }

        let restored = reclaimed.is_some();
        let previous = reclaimed.map(|file| project_from_dir(data_root.clone(), file));
        let migrated_from = if relocated {
            Some(source_kanban)
        } else {
            previous.as_ref().and_then(|p| p.migrated_from.clone())
        };
        let name = name
            .map(display_name)
            .filter(|name| !name.is_empty())
            .or_else(|| previous.as_ref().map(|p| p.name.clone()))
            .or_else(|| crate::core::migrate::board_display_name(&data_root))
            .unwrap_or_else(|| default_name(&work_path));
        let project = Project {
            id,
            name,
            work_path,
            data_root: data_root.clone(),
            created_at: previous
                .as_ref()
                .map(|p| p.created_at)
                .unwrap_or_else(timefmt::now),
            last_opened_at: previous.as_ref().and_then(|p| p.last_opened_at),
            migrated_from,
        };
        // Registration first: the stale `.removed` copy is only dropped once
        // the live `project.yaml` is on disk, so a failed write is recoverable.
        self.write_project(&project)?;
        if restored {
            fs::remove_file(data_root.join(REMOVED_PROJECT_FILE))?;
        }
        Ok(Added {
            project,
            created: true,
            restored,
        })
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<Project> {
        let name = display_name(name);
        if name.is_empty() {
            return Err(KanbanError::Invalid("project name cannot be empty".into()));
        }
        let _guard = self.lock()?;
        let mut project = self.require(id)?;
        project.name = name;
        self.write_project(&project)?;
        Ok(project)
    }

    /// Repoint a project at another folder; the id and the board are untouched.
    pub fn set_path(&self, id: &str, work_path: &Path) -> Result<Project> {
        let work_path = normalize_path(work_path);
        if !work_path.is_dir() {
            return Err(KanbanError::Invalid(format!(
                "not a directory: {}",
                work_path.display()
            )));
        }
        let _guard = self.lock()?;
        let mut project = self.require(id)?;
        project.work_path = work_path;
        self.write_project(&project)?;
        Ok(project)
    }

    /// Unregister a project. Without `purge_data` the board is kept in the
    /// store and re-adding the same folder picks it back up; with it, the
    /// whole project directory is deleted.
    pub fn remove(&self, id: &str, purge_data: bool) -> Result<()> {
        let _guard = self.lock()?;
        let project = self.require(id)?;
        if purge_data {
            fs::remove_dir_all(&project.data_root)?;
            return Ok(());
        }
        fs::rename(
            project.data_root.join(PROJECT_FILE),
            project.data_root.join(REMOVED_PROJECT_FILE),
        )?;
        Ok(())
    }

    pub fn touch_opened(&self, id: &str) -> Result<()> {
        let _guard = self.lock()?;
        let mut project = self.require(id)?;
        project.last_opened_at = Some(timefmt::now());
        self.write_project(&project)
    }

    /// The project registered for exactly this directory, if any.
    ///
    /// Deliberately no ancestor walk: today's `Operations::new(".")` looks at
    /// the current directory only, and the CLI phase adopts an unregistered
    /// `<cwd>/.kanban` into the store, so a walk would let a command silently
    /// act on — and move — a parent's board the user never named.
    pub fn resolve_from_cwd(&self, cwd: &Path) -> Result<Option<Project>> {
        let cwd = normalize_path(cwd);
        Ok(self
            .list()?
            .into_iter()
            .find(|p| normalize_path(&p.work_path) == cwd))
    }

    /// Store-wide lock; held by every mutating method. Reentrant per thread.
    pub fn lock(&self) -> Result<BoardGuard> {
        fs::create_dir_all(&self.root)?;
        lock_file_reentrant(&self.root.join(LOCK_FILE))
    }

    fn require(&self, id: &str) -> Result<Project> {
        self.get(id)?
            .ok_or_else(|| KanbanError::Invalid(format!("no such project: {id}")))
    }

    fn write_project(&self, project: &Project) -> Result<()> {
        fs::create_dir_all(&project.data_root)?;
        let yaml = serde_yaml_ng::to_string(&ProjectFile::from(project))?;
        let yaml = timefmt::quote_yaml_timestamp_fields(&yaml, &["created_at", "last_opened_at"]);
        atomic_write_text(&project.data_root.join(PROJECT_FILE), &yaml)
    }

    /// Pick the directory name for a new registration: the slug of the folder
    /// basename, with `-2`, `-3`, … appended past a collision. A directory
    /// left behind by an unregistering `remove` for the *same* work path is
    /// reclaimed instead of skipped — anything else is treated as taken.
    fn allocate_id(&self, work_path: &Path) -> Result<(String, Option<ProjectFile>)> {
        let base = slugify(&default_name(work_path));
        for suffix in 1..=MAX_ID_SUFFIX {
            let id = if suffix == 1 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            let data_root = self.projects_dir().join(&id);
            if !data_root.exists() {
                return Ok((id, None));
            }
            if data_root.join(PROJECT_FILE).exists() {
                continue;
            }
            match read_project_file(&data_root.join(REMOVED_PROJECT_FILE))? {
                Some(previous) if normalize_path(&previous.work_path) == work_path => {
                    return Ok((id, Some(previous)));
                }
                _ => continue,
            }
        }
        Err(KanbanError::Invalid(format!(
            "cannot allocate a project id for {base}: too many collisions"
        )))
    }
}

fn read_project_file(path: &Path) -> Result<Option<ProjectFile>> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(Some(serde_yaml_ng::from_str(&raw)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Store root: `$KANBAN_HOME` > `$XDG_DATA_HOME/kanban4ai` >
/// `$HOME/.local/share/kanban4ai`.
pub fn store_root() -> Result<PathBuf> {
    resolve_store_root(
        std::env::var_os(STORE_ROOT_ENV),
        std::env::var_os(XDG_DATA_HOME_ENV),
        std::env::var_os(HOME_ENV),
    )
}

fn resolve_store_root(
    kanban_home: Option<OsString>,
    xdg_data_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf> {
    // An empty or relative value is treated as unset, per the XDG spec.
    let usable = |value: Option<OsString>| {
        value
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.as_os_str().len() > 1)
    };
    if let Some(home) = usable(kanban_home) {
        return Ok(home);
    }
    if let Some(data_home) = usable(xdg_data_home) {
        return Ok(data_home.join(STORE_DIR_NAME));
    }
    if let Some(home) = usable(home) {
        return Ok(home.join(".local").join("share").join(STORE_DIR_NAME));
    }
    Err(KanbanError::Invalid(format!(
        "cannot locate the kanban store: none of ${STORE_ROOT_ENV}, ${XDG_DATA_HOME_ENV} or ${HOME_ENV} is set to an absolute path"
    )))
}

/// Default display name for a work folder: its basename.
pub fn default_name(work_path: &Path) -> String {
    work_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "project".to_string())
}

fn display_name(name: &str) -> String {
    name.trim().to_string()
}

/// Project id: lowercased basename, `[a-z0-9._-]` kept, every other rune
/// folded to `-`, runs collapsed, leading/trailing `-`/`.` trimmed (so no
/// hidden or `..`-shaped directory names). Empty results become `project`.
pub fn slugify(input: &str) -> String {
    let mut slug = String::new();
    for ch in input.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else if matches!(ch, '.' | '_' | '-') {
            ch
        } else {
            '-'
        };
        if mapped == '-' && slug.ends_with('-') {
            continue;
        }
        slug.push(mapped);
    }
    while !slug.is_char_boundary(MAX_ID_LEN.min(slug.len())) {
        slug.pop();
    }
    slug.truncate(MAX_ID_LEN);
    let slug = slug.trim_matches(['-', '.']);
    if slug.is_empty() {
        "project".to_string()
    } else {
        slug.to_string()
    }
}

/// Absolute, symlink-resolved path when the target exists; otherwise the
/// lexically cleaned absolute path, so a project whose folder was deleted
/// still compares equal to its recorded `work_path`.
pub fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        let mut out = if path.is_absolute() {
            PathBuf::new()
        } else {
            std::env::current_dir().unwrap_or_default()
        };
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_root_prefers_kanban_home() {
        let root = resolve_store_root(
            Some("/tmp/store".into()),
            Some("/data".into()),
            Some("/home/u".into()),
        )
        .unwrap();
        assert_eq!(root, PathBuf::from("/tmp/store"));
    }

    #[test]
    fn store_root_falls_back_to_xdg_then_home() {
        let xdg = resolve_store_root(None, Some("/data".into()), Some("/home/u".into())).unwrap();
        assert_eq!(xdg, PathBuf::from("/data/kanban4ai"));

        let home = resolve_store_root(None, None, Some("/home/u".into())).unwrap();
        assert_eq!(home, PathBuf::from("/home/u/.local/share/kanban4ai"));
    }

    #[test]
    fn store_root_ignores_empty_and_relative_values() {
        let root = resolve_store_root(Some("".into()), Some("data".into()), Some("/home/u".into()))
            .unwrap();
        assert_eq!(root, PathBuf::from("/home/u/.local/share/kanban4ai"));

        assert!(resolve_store_root(None, None, None).is_err());
    }

    #[test]
    fn slugify_folds_and_collapses() {
        assert_eq!(slugify("kanban4ai"), "kanban4ai");
        assert_eq!(slugify("My Project"), "my-project");
        assert_eq!(slugify("a  //  b"), "a-b");
        assert_eq!(slugify("Проект"), "project");
        assert_eq!(slugify("web.app_v2"), "web.app_v2");
    }

    #[test]
    fn slugify_never_yields_a_dangerous_directory_name() {
        assert_eq!(slugify(""), "project");
        assert_eq!(slugify("."), "project");
        assert_eq!(slugify(".."), "project");
        assert_eq!(slugify("///"), "project");
        assert_eq!(slugify(".hidden"), "hidden");
        assert!(slugify(&"x".repeat(200)).len() <= MAX_ID_LEN);
    }

    #[test]
    fn normalize_path_cleans_a_missing_absolute_path() {
        assert_eq!(
            normalize_path(Path::new("/tmp/does-not-exist/./a/../b")),
            PathBuf::from("/tmp/does-not-exist/b")
        );
    }
}
