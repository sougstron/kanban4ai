//! Phase 3 agent runtime: backend command construction, prompt generation,
//! tmux/background process launch, log wiring, and attach support.

mod backends;
mod launcher;
mod prompt;
mod tmux;

pub use backends::parse_opencode_agent_list;
pub use backends::{
    AgentBackendConfig, AutoLaunchConfig, LaunchPlan, LaunchSettings, OpencodeCatalog,
    build_launch_plan, cached_opencode_catalog, opencode_catalog, parse_opencode_models_verbose,
    recent_models, record_recent_model, resolve_launch_settings, resolve_opencode_agent,
    sort_efforts, sort_opencode_models, warm_opencode_catalog,
};
pub use launcher::KanbanLauncher;
pub use prompt::build_agent_prompt;
pub use tmux::{attach_to_session, kill_session};
