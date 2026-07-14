use std::path::Path;

use crate::agent::backends::{auto_launch_config, build_launch_plan};
use crate::agent::tmux::spawn_plan;
use crate::core::config::Config;
use crate::core::models::Task;
use crate::core::operations::AgentLauncher;

#[derive(Debug, Default)]
pub struct KanbanLauncher;

impl AgentLauncher for KanbanLauncher {
    fn launch(&self, project_path: &Path, task: &Task, session_id: &str, revert: bool) -> bool {
        match launch(project_path, task, session_id, revert) {
            Ok(started) => started,
            Err(err) => {
                eprintln!(
                    "Warning: failed to launch agent for task {} session {}: {}",
                    task.id, session_id, err
                );
                false
            }
        }
    }
}

fn launch(
    project_path: &Path,
    task: &Task,
    session_id: &str,
    revert: bool,
) -> crate::core::error::Result<bool> {
    let config = Config::new(project_path).load()?;
    let auto_launch = auto_launch_config(&config);
    if !auto_launch.enabled {
        return Ok(false);
    }
    let plan = build_launch_plan(project_path, task, session_id, revert)?;
    spawn_plan(project_path, &plan, &auto_launch)
}
