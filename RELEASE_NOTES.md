# kanban4ai 0.2.0

## Highlights

- Reworked agent launching so delegated jobs receive `KANBAN_CMD`, keep automatic heartbeats while running, preserve task thread/review context in prompts, and support opencode model variants plus Claude reasoning effort.
- Expanded the TUI with direct Run actions, project settings, clickable status/action bars, add-context/suggestion flows, archive restore, session management, log viewing, search, scroll indicators, and safer confirmation dialogs.
- Improved task/session operations with named sessions, exact bulk moves, stop-session support, archive restore, direct creation in target columns, first-question previews, and YAML timestamp compatibility for legacy Python tooling.
- Updated user and agent documentation for the current Rust CLI/TUI workflow, AUR packaging, release automation, and backend configuration.

## Verification coverage

- Added and refreshed CLI, operations, storage, thread, config, agent, and TUI snapshot tests for the new workflows.
- Release CI continues to run rustfmt, clippy with warnings denied, locked tests, release builds, and installer packaging smoke tests before publishing tagged artifacts.
