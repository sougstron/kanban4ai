//! Phase 3 agent runtime: backend command construction, prompt generation,
//! tmux/background process launch, log wiring, and attach support.

mod backends;
mod launcher;
mod prompt;
mod tmux;

pub use backends::parse_opencode_agent_list;
pub use backends::{
    AgentBackendConfig, AutoLaunchConfig, BackendCatalog, LaunchPlan, LaunchSettings,
    backend_catalog, backend_config, backend_has_catalog, build_launch_plan,
    cached_backend_catalog, cached_opencode_catalog, load_pi_catalog, load_pi_catalog_from_dir,
    opencode_catalog, parse_omp_models_json, parse_opencode_models_verbose,
    parse_pi_builtin_catalog, parse_pi_models_json, parse_pi_models_store, pi_builtin_data_dir,
    recent_models, record_recent_model, resolve_bot_launch_settings, resolve_launch_settings,
    resolve_opencode_agent, resolve_task_launch_settings, sort_efforts, sort_opencode_models,
    upcoming_run_plan, warm_backend_catalog, warm_opencode_catalog,
};
pub use launcher::KanbanLauncher;
pub use prompt::build_agent_prompt;
pub use tmux::{attach_to_session, kill_session, run_foreground, session_exists};
