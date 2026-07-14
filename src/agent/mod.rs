//! Phase 3 agent runtime: backend command construction, prompt generation,
//! tmux/background process launch, log wiring, and attach support.

mod backends;
mod launcher;
mod prompt;
mod tmux;

pub use backends::parse_opencode_agent_list;
pub use backends::{AgentBackendConfig, AutoLaunchConfig, LaunchPlan, build_launch_plan};
pub use launcher::KanbanLauncher;
pub use prompt::build_agent_prompt;
pub use tmux::attach_to_session;
