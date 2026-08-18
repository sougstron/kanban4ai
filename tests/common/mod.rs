// Each integration-test binary uses a subset of these helpers.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kanban4ai::core::models::Task;
use kanban4ai::core::operations::{AgentLauncher, Operations};
use kanban4ai::core::project::Roots;
use kanban4ai::core::storage::Storage;

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Fresh initialized board in a temp dir.
pub fn temp_board() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let storage = Storage::new(dir.path());
    storage.init_board().expect("init board");
    (dir, storage)
}

/// Board whose config disables desktop notifications (so tests never spawn
/// notify-send) with auto-launch toggled as requested.
pub fn quiet_board(auto_launch_enabled: bool) -> (tempfile::TempDir, Storage) {
    let (dir, storage) = temp_board();
    write_quiet_config(dir.path(), auto_launch_enabled);
    (dir, storage)
}

pub fn write_quiet_config(project: &Path, auto_launch_enabled: bool) {
    let config = format!(
        "columns:\n- name: To Do\n  id: todo\n- name: In Progress\n  id: in_progress\n- name: Review\n  id: review\n- name: Done\n  id: done\nnotifications:\n  enabled: false\nauto_launch:\n  enabled: {auto_launch_enabled}\n"
    );
    fs::write(project.join(".kanban/config.yaml"), config).expect("write quiet config");
}

pub fn copy_fixture(fixture_rel: &str, dest: &Path) {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::copy(fixtures_dir().join(fixture_rel), dest).expect("copy fixture");
}

/// Launch call recorded by [`RecordingLauncher`]: (task_id, session_id, revert).
pub type LaunchCall = (String, String, bool);

/// The roots a launch was handed: (data_root, work_path, project id).
pub type LaunchRoots = (PathBuf, PathBuf, Option<String>);

#[derive(Clone, Default)]
pub struct RecordingLauncher {
    pub calls: Arc<Mutex<Vec<LaunchCall>>>,
    pub roots: Arc<Mutex<Vec<LaunchRoots>>>,
}

impl RecordingLauncher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calls(&self) -> Vec<LaunchCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn roots(&self) -> Vec<LaunchRoots> {
        self.roots.lock().unwrap().clone()
    }
}

impl AgentLauncher for RecordingLauncher {
    fn launch(
        &self,
        roots: Roots<'_>,
        task: &Task,
        session_id: &str,
        revert: bool,
    ) -> kanban4ai::core::error::Result<bool> {
        self.calls
            .lock()
            .unwrap()
            .push((task.id.clone(), session_id.to_string(), revert));
        self.roots.lock().unwrap().push((
            roots.data_root.to_path_buf(),
            roots.work_path.to_path_buf(),
            roots.project_id.map(str::to_string),
        ));
        Ok(true)
    }
}

/// Operations over a quiet board with a recording launcher injected.
pub fn ops_with_recorder(
    auto_launch_enabled: bool,
) -> (tempfile::TempDir, Operations, RecordingLauncher) {
    let (dir, _storage) = quiet_board(auto_launch_enabled);
    let recorder = RecordingLauncher::new();
    let ops = Operations::with_launcher(dir.path(), Box::new(recorder.clone()));
    (dir, ops, recorder)
}
