# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Project

`kanban4ai` is a completed native Rust CLI and ratatui application for local
kanban boards driven by AI coding agents. It preserves the original CLI and
on-disk board formats. `AGENTS.md` is the exhaustive behavior, architecture,
change-log, and version-release reference.

The canonical repository is <https://github.com/sougstron/kanban4ai>. The crate
builds one binary named `kanban4ai`; installation packages create relative
`kanban` and `kb` symlinks rather than additional Cargo targets.

## Required checks

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
sh -n scripts/install.sh scripts/test-packaging.sh
sh scripts/test-packaging.sh
```

TUI changes also require ratatui snapshot tests and visual terminal
verification. Packaging changes should validate `makepkg --printsrcinfo` when
makepkg is available.

## Architecture and conventions

Commands flow through `src/cli.rs` or `src/tui/` into
`src/core/operations.rs`, then into storage/thread/config/session services and
optional `src/agent/` process launchers.

- Put business behavior in `core/operations.rs`, not directly in CLI/TUI code.
- Preserve the agent/human distinction in move and assignment rules.
- Read thresholds and rules through `core/config.rs`; do not hardcode them.
- Use atomic writes and hold the board lock for task read-modify-write cycles.
- Keep context compaction deterministic and rule-based.
- Never regenerate or delete `tests/fixtures/`; they verify compatibility with
  boards created by the earlier implementation.
- Keep Cargo's single canonical binary and all package aliases as symlinks.
- Follow the change-log and version-update workflow in `AGENTS.md`: ordinary
  work remains uncommitted, and commit/tag/push/deploy operations are allowed
  only for an explicit update to a specific version.
