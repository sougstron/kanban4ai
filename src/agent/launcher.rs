use crate::agent::backends::{
    auto_launch_config, backend_has_catalog, build_launch_plan, record_recent_model,
};
use crate::agent::tmux::spawn_plan;
use crate::core::config::Config;
use crate::core::error::Result;
use crate::core::models::Task;
use crate::core::operations::AgentLauncher;
use crate::core::project::Roots;

#[derive(Debug, Default)]
pub struct KanbanLauncher;

impl AgentLauncher for KanbanLauncher {
    fn launch(
        &self,
        roots: Roots<'_>,
        task: &Task,
        session_id: &str,
        revert: bool,
    ) -> Result<bool> {
        launch(roots, task, session_id, revert)
    }
}

fn launch(
    roots: Roots<'_>,
    task: &Task,
    session_id: &str,
    revert: bool,
) -> crate::core::error::Result<bool> {
    let config = Config::new(roots.data_root).load()?;
    let auto_launch = auto_launch_config(&config);
    if !auto_launch.enabled {
        return Ok(false);
    }
    let plan = build_launch_plan(roots, task, session_id, revert)?;
    let started = spawn_plan(roots, &plan, &auto_launch)?;
    if started
        && backend_has_catalog(&plan.backend)
        && let Some(model) = plan.model.as_deref()
    {
        record_recent_model(roots.data_root, model);
    }
    Ok(started)
}
