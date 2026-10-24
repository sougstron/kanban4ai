//! Project trash: a purged project is moved here instead of being deleted, so
//! an accidental removal can be undone until the retention runs out.
//!
//! ```text
//! <store>/trash/
//! └── <id>~<YYYYMMDD-HHMMSS>/   # the former data_root, renamed as a whole
//!     ├── trash.yaml            # original id and deletion time
//!     ├── project.yaml
//!     └── .kanban/
//! ```
//!
//! The trash sits outside `projects/`, so listing and sync capture never see
//! it. Removal replicates as a deletion like before; restoring moves the
//! directory back and reconciles sync, so the board comes back as a newer
//! `restore` that wins over the tombstones.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::core::config::TRASH_RETENTION_DAYS;
use crate::core::error::{KanbanError, Result};
use crate::core::project::{Project, ProjectStore, normalize_path, read_registration};
use crate::core::storage::atomic_write_text;
use crate::core::timefmt;

pub const TRASH_DIR: &str = "trash";
const TRASH_FILE: &str = "trash.yaml";
/// Separates the project id from the deletion stamp in an entry name. Not a
/// slug character, so an entry name never collides with a project id.
const ENTRY_SEPARATOR: char = '~';
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

static LAST_PRUNE: Mutex<Option<Instant>> = Mutex::new(None);

/// One project waiting in the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedProject {
    /// Directory name under `<store>/trash/`; what restore and purge take.
    pub entry: String,
    /// The id the project had, reused on restore when it is still free.
    pub id: String,
    pub name: String,
    pub work_path: PathBuf,
    pub path: PathBuf,
    pub deleted_at: NaiveDateTime,
    /// When [`ProjectStore::prune_trash`] deletes it for good.
    pub expires_at: NaiveDateTime,
}

#[derive(Debug, Serialize, Deserialize)]
struct TrashFile {
    id: String,
    #[serde(with = "timefmt::serde_naive")]
    deleted_at: NaiveDateTime,
}

impl ProjectStore {
    pub fn trash_dir(&self) -> PathBuf {
        self.root().join(TRASH_DIR)
    }

    /// `trash.retention_days` from the global config, default 7.
    pub fn trash_retention_days(&self) -> i64 {
        self.load_global_config()
            .map(|config| config.trash_retention_days())
            .unwrap_or(TRASH_RETENTION_DAYS)
    }

    /// Move a registered project into the trash. The caller holds the store lock.
    pub(crate) fn move_to_trash(&self, project: &Project) -> Result<PathBuf> {
        let deleted_at = timefmt::now();
        let stamp = deleted_at.format("%Y%m%d-%H%M%S").to_string();
        fs::create_dir_all(self.trash_dir())?;
        let mut entry = format!("{}{ENTRY_SEPARATOR}{stamp}", project.id);
        let mut suffix = 2;
        while self.trash_dir().join(&entry).exists() {
            entry = format!("{}{ENTRY_SEPARATOR}{stamp}-{suffix}", project.id);
            suffix += 1;
        }
        let dest = self.trash_dir().join(&entry);
        fs::rename(&project.data_root, &dest)?;
        let file = TrashFile {
            id: project.id.clone(),
            deleted_at,
        };
        let yaml = timefmt::quote_yaml_timestamp_fields(
            &serde_yaml_ng::to_string(&file)?,
            &["deleted_at"],
        );
        atomic_write_text(&dest.join(TRASH_FILE), &yaml)?;
        Ok(dest)
    }

    /// Trashed projects, most recently deleted first. Entries without a
    /// readable registration are skipped, like in [`ProjectStore::list`].
    pub fn trash_list(&self) -> Result<Vec<TrashedProject>> {
        let entries = match fs::read_dir(self.trash_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let retention = chrono::Duration::days(self.trash_retention_days());
        let mut items: Vec<TrashedProject> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .filter_map(|path| read_entry(&path, retention).ok().flatten())
            .collect();
        items.sort_by(|a, b| {
            b.deleted_at
                .cmp(&a.deleted_at)
                .then_with(|| a.entry.cmp(&b.entry))
        });
        Ok(items)
    }

    /// Put a trashed project back under its old id, or `<id>-N` when that id
    /// was taken in the meantime. Refuses when its folder is registered again.
    pub fn restore_from_trash(&self, entry: &str) -> Result<Project> {
        let restored = {
            let _guard = self.lock()?;
            let item = self.require_trashed(entry)?;
            let wanted = normalize_path(&item.work_path);
            if let Some(other) = self
                .list()?
                .into_iter()
                .find(|p| normalize_path(&p.work_path) == wanted)
            {
                return Err(KanbanError::Invalid(format!(
                    "{} is registered again as project {}; remove it before restoring",
                    item.work_path.display(),
                    other.id
                )));
            }
            let id = self.free_id(&item.id)?;
            let data_root = self.projects_dir().join(&id);
            fs::create_dir_all(self.projects_dir())?;
            fs::rename(&item.path, &data_root)?;
            fs::remove_file(data_root.join(TRASH_FILE))?;
            let project = self.require(&id)?;
            if id != item.id {
                self.write_project(&project)?;
            }
            project
        };
        crate::core::sync::activate(self.root())?;
        Ok(restored)
    }

    /// Delete one trashed project for good.
    pub fn purge_from_trash(&self, entry: &str) -> Result<()> {
        let _guard = self.lock()?;
        let item = self.require_trashed(entry)?;
        fs::remove_dir_all(&item.path)?;
        Ok(())
    }

    /// Delete everything in the trash; returns how many projects went.
    pub fn empty_trash(&self) -> Result<usize> {
        let _guard = self.lock()?;
        let items = self.trash_list()?;
        for item in &items {
            fs::remove_dir_all(&item.path)?;
        }
        Ok(items.len())
    }

    /// Delete entries past their retention as of `now`.
    pub fn prune_trash(&self, now: NaiveDateTime) -> Result<usize> {
        if !self.trash_dir().is_dir() {
            return Ok(0);
        }
        let _guard = self.lock()?;
        let mut removed = 0;
        for item in self.trash_list()? {
            if item.expires_at <= now {
                fs::remove_dir_all(&item.path)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn require_trashed(&self, entry: &str) -> Result<TrashedProject> {
        let path = self.trash_dir().join(entry);
        let found = if path.parent() == Some(self.trash_dir().as_path()) && path.is_dir() {
            let retention = chrono::Duration::days(self.trash_retention_days());
            read_entry(&path, retention)?
        } else {
            None
        };
        found.ok_or_else(|| KanbanError::Invalid(format!("no such trash entry: {entry}")))
    }

    fn free_id(&self, base: &str) -> Result<String> {
        let mut id = base.to_string();
        let mut suffix = 2;
        while self.projects_dir().join(&id).exists() {
            id = format!("{base}-{suffix}");
            suffix += 1;
        }
        Ok(id)
    }
}

/// Prune expired trash at most once an hour; called from the shared pump.
/// Never fails the tick.
pub fn prune_if_due(store: &ProjectStore) {
    {
        let mut last = LAST_PRUNE.lock().unwrap_or_else(|err| err.into_inner());
        if last.is_some_and(|at| at.elapsed() < PRUNE_EVERY) {
            return;
        }
        *last = Some(Instant::now());
    }
    let _ = store.prune_trash(timefmt::now());
}

fn read_entry(path: &Path, retention: chrono::Duration) -> Result<Option<TrashedProject>> {
    let raw = match fs::read_to_string(path.join(TRASH_FILE)) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let file: TrashFile = serde_yaml_ng::from_str(&raw)?;
    let Some(project) = read_registration(path)? else {
        return Ok(None);
    };
    Ok(Some(TrashedProject {
        entry: path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        id: file.id,
        name: project.name,
        work_path: project.work_path,
        path: path.to_path_buf(),
        deleted_at: file.deleted_at,
        expires_at: file.deleted_at + retention,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(dir: &Path, name: &str) -> (ProjectStore, Project) {
        let store = ProjectStore::at(dir.join("store"));
        let work = dir.join(name);
        fs::create_dir_all(&work).unwrap();
        let project = store.add(&work, None).unwrap().project;
        fs::create_dir_all(project.kanban_dir().join("tasks/todo")).unwrap();
        fs::write(project.kanban_dir().join("tasks/todo/TASK-001.md"), "x").unwrap();
        (store, project)
    }

    #[test]
    fn purge_moves_to_trash_and_restore_brings_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let (store, project) = store_with(dir.path(), "adv");
        store.remove(&project.id, true).unwrap();
        assert!(store.get(&project.id).unwrap().is_none());
        let items = store.trash_list().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, project.id);
        assert_eq!(items[0].name, project.name);
        assert_eq!(
            items[0].expires_at - items[0].deleted_at,
            chrono::Duration::days(TRASH_RETENTION_DAYS)
        );

        let restored = store.restore_from_trash(&items[0].entry).unwrap();
        assert_eq!(restored.id, project.id);
        assert!(
            restored
                .kanban_dir()
                .join("tasks/todo/TASK-001.md")
                .is_file()
        );
        assert!(!restored.data_root.join(TRASH_FILE).exists());
        assert!(store.trash_list().unwrap().is_empty());
    }

    #[test]
    fn restore_takes_a_free_id_and_refuses_a_reregistered_folder() {
        let dir = tempfile::tempdir().unwrap();
        let (store, project) = store_with(dir.path(), "adv");
        store.remove(&project.id, true).unwrap();
        let entry = store.trash_list().unwrap()[0].entry.clone();

        let again = store.add(&project.work_path, None).unwrap().project;
        let err = store.restore_from_trash(&entry).unwrap_err();
        assert!(err.to_string().contains("registered again"), "{err}");

        // Same id, different folder: the restored project gets `<id>-2`.
        store.remove(&again.id, false).unwrap();
        fs::rename(
            again.data_root.join("project.yaml.removed"),
            again.data_root.join("project.yaml"),
        )
        .unwrap();
        let moved = dir.path().join("elsewhere");
        fs::create_dir_all(&moved).unwrap();
        store.set_path(&again.id, &moved).unwrap();
        let restored = store.restore_from_trash(&entry).unwrap();
        assert_eq!(restored.id, format!("{}-2", project.id));
        assert_eq!(store.get(&restored.id).unwrap().unwrap().id, restored.id);
    }

    #[test]
    fn prune_deletes_only_expired_entries_and_purge_is_final() {
        let dir = tempfile::tempdir().unwrap();
        let (store, first) = store_with(dir.path(), "one");
        let work = dir.path().join("two");
        fs::create_dir_all(&work).unwrap();
        let second = store.add(&work, None).unwrap().project;
        store.remove(&first.id, true).unwrap();
        store.remove(&second.id, true).unwrap();
        let items = store.trash_list().unwrap();
        assert_eq!(items.len(), 2);

        let deleted_at = items[0].deleted_at;
        assert_eq!(store.prune_trash(deleted_at).unwrap(), 0);
        let later = deleted_at + chrono::Duration::days(TRASH_RETENTION_DAYS + 1);
        assert_eq!(store.prune_trash(later).unwrap(), 2);
        assert!(store.trash_list().unwrap().is_empty());

        let work = dir.path().join("three");
        fs::create_dir_all(&work).unwrap();
        let third = store.add(&work, None).unwrap().project;
        store.remove(&third.id, true).unwrap();
        let entry = store.trash_list().unwrap()[0].entry.clone();
        store.purge_from_trash(&entry).unwrap();
        assert!(store.trash_list().unwrap().is_empty());
        assert!(store.purge_from_trash("../projects").is_err());
    }

    #[test]
    fn retention_comes_from_the_global_config() {
        let dir = tempfile::tempdir().unwrap();
        let (store, project) = store_with(dir.path(), "adv");
        fs::write(store.global_config_path(), "trash:\n  retention_days: 2\n").unwrap();
        assert_eq!(store.trash_retention_days(), 2);
        store.remove(&project.id, true).unwrap();
        let item = &store.trash_list().unwrap()[0];
        assert_eq!(item.expires_at - item.deleted_at, chrono::Duration::days(2));
    }
}
