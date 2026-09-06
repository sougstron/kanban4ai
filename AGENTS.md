# AGENTS.md

## Project: kanban4ai

A local kanban board application for task management within projects, driven by
AI coding agents (opencode, Claude Code, Codex CLI) via CLI commands. Native Rust rewrite
of the Python `kanban-cli`; the on-disk format and the CLI contract are fully
compatible with boards created by the original.

This file is auto-loaded into **every** agent session by opencode/omp/pi, so
every token in it is charged to every run. It deliberately holds only the
project shape and the rules that apply to all work. The exhaustive reference
lives in `docs/` — read the one file a task actually needs (see the map below).
See `docs/token-profile.md` for the measurements behind this split, and keep
`sh scripts/token-budget.sh` green.

### Architecture
- **Type**: Standalone CLI tool + TUI (NOT an opencode plugin)
- **Language**: Rust (stable, edition 2024)
- **TUI Framework**: ratatui + crossterm
- **Storage**: File-based (Markdown + YAML frontmatter), no database
- **Integration**: Shell command calls from agents; binary `kanban4ai` with
  `kanban` / `kb` symlinks

### Rewrite status
Порт на Rust завершён: реализованы ядро данных, полный CLI, business logic,
запуск агентов, нативный TUI и release/AUR packaging. Исходники прежней
Python-версии удалены; `tests/fixtures/` сохранены для проверки совместимости
формата существующих досок.

### Reference map (`docs/`)

Load only what the task touches:

| File | Covers |
|---|---|
| `docs/data-model.md` | Task/Session/Thread models, task & thread file formats, context/questions/review edits, `.kanban/` directories, projects & store |
| `docs/cli.md` | Every implemented `kanban` command (`kanban <cmd> --help` is authoritative) |
| `docs/config.md` | Thresholds, TUI/global/notification/auto-launch/orchestration settings, agent backends |
| `docs/orchestration.md` | Run phases, integration model, auto-launch, queue dispatcher, headless daemon, crash restart, chaining, task dependencies (DAG) & orchestrator mode |
| `docs/worktrees.md` | Worktree isolation (`core/vcs.rs`), landing/merge, backup & revert |
| `docs/tui.md` | TUI keyboard shortcuts and dialogs |
| `docs/limits.md` | Provider subscription limits (`core/limits.rs`, `tui/limits.rs`) |
| `docs/agent-io.md` | Agent reply capture, live telemetry, image attachments |
| `docs/stats.md` | Application-collected usage statistics (`core/stats.rs`): event schema, hooks, the Stats report |
| `docs/releasing.md` | Updater internals (`core/update.rs`, `core/http.rs`, self-update behavior) |
| `docs/token-profile.md` | Measured token cost of the board, and where to cut |

### Directory Structure
```
src/
├── main.rs              # Binary entry point (SIGPIPE reset + cli::run)
├── lib.rs
├── cli/                 # clap CLI: every `kanban` command, Python-compatible output
│   ├── mod.rs           # parser + dispatch; global `--project`
│   ├── init.rs          # store-backed `kanban init`
│   ├── project.rs       # `kanban project` list/add/show/rename/set-path/path/remove/open
│   ├── resolve.rs       # `--project` / $KANBAN_PROJECT / cwd / silent adoption
│   └── daemon.rs        # `kanban daemon` foreground loop
└── core/
    ├── mod.rs
    ├── error.rs         # KanbanError (Io/Yaml/Invalid/Permission) / Result
    ├── timefmt.rs       # Python-isoformat timestamps (parse/format/serde)
    ├── models.rs        # Task, Session, Thread, Message, enums
    ├── config.rs        # BoardConfig + per-project .kanban/config.yaml loader
    ├── storage.rs       # Task file I/O, atomic writes, board lock, fingerprint
    ├── thread.rs        # ThreadManager: sidecar threads, merge-on-save
    ├── operations.rs    # Business-logic hub: CRUD, rules, questions, chaining,
    │                    #   review edits; AgentLauncher seam
    ├── project.rs       # ProjectStore: registry, store-root resolution, add/migrate
    ├── migrate.rs       # Relocate a local `.kanban` into the store (rename / EXDEV copy)
    ├── session.rs       # SessionManager: heartbeats, crash detection, token estimate
    ├── context.rs       # ContextManager: thread-based context + legacy back-compat
    ├── compaction.rs    # Rule-based context compaction (no LLM)
    ├── scheduler.rs     # Slot census, queue dispatch, crash-restart backoff
    ├── daemon.rs        # Store-wide tick + single-instance `daemon.lock`
    ├── limits.rs        # Provider subscription limits (claude/codex/grok/zai/synthetic/yolo) + cache
    ├── stats.rs         # App-collected usage stats: event log, aggregation, report
    ├── notifier.rs      # Desktop notifications (notify-send)
    └── vcs.rs           # Worktree isolation: git probe, live snapshots, merge-tree landing
Additional modules:
    agent/               # process manager, tmux wrapper, backends, prompts
    tui/                 # ratatui board, detail, dialogs, search, sessions, projects,
                         #   limits row
.github/workflows/       # CI and tagged Linux release automation
packaging/aur/           # stable and VCS Arch source packages
scripts/                 # POSIX installer, packaging smoke test, token profiler/budget
docs/                    # long-form reference, loaded on demand (see map above)
tests/
├── fixtures/            # golden files written by the Python version
├── golden_compat.rs     # lossless load/round-trip of Python-written files
├── storage_test.rs, thread_test.rs, config_test.rs
├── operations_test.rs   # agent rules, questions, chaining, review edits
├── project_test.rs      # store CRUD, cwd resolution, migration, silent adoption
├── cli_test.rs          # end-to-end binary tests (assert_cmd)
```

### Agent Rules (Enforced with --agent flag)
1. `one_task_per_instance`: Block an agent from taking multiple tasks
2. `user_only_review_to_done`: Only the user can move Review -> Done. Agents must never move a task to Done; an executor's `kanban done` lands in Review (or bot review when the reviewer is on)
3. `auto_move_on_assign`: Move to In Progress on take
4. `auto_move_on_complete`: Move to Review on agent done
5. `questions_go_to_review`: If true, questions move task to Review; if false, keep in In Progress
6. `resume_after_last_answer`: When the last open question is answered and the agent is no longer running, wake it — through the queue when it was paused, otherwise on a fresh session (gated by `auto_launch.enabled`). A live `ask --wait` poller is left alone — it wakes itself
7. `auto_launch_on_delegate`: On agent `take`, auto-launch the backend for the task (gated by `auto_launch.enabled`)
8. `auto_launch_chained`: When a task enters Review, auto-launch every To Do task whose `chained_to` points at it (gated by `auto_launch.enabled`)
9. Designer-phase agents cannot move their task at all; they record a plan and finish the design phase with `kanban done` (that does not complete the work)
10. Reviewer-phase agents cannot move their task at all; they must not implement fixes; their only exit is `kanban verdict`

When `interactive: true`, delegated agents are instructed to use `kanban ask --wait` for blocking questions and `kanban suggest` for non-blocking ideas.

**Role contracts.** A session's role comes from the task's run phase
(`Role::from_phase`: `design` → designer, `review` → reviewer, everything else
including a missing phase → executor). The role picks the prompt
(`agent/prompt.rs`) *and* the move gate in `operations::move_task`, so the
contract is enforced, not merely worded:

| role | prompt says | enforced |
|---|---|---|
| executor | finish with `kanban done`, which lands the task in Review (or starts bot review); never move a task to Done; do not use `kanban move` to change columns | `user_only_review_to_done`: an agent move to Done, or out of Review, is refused |
| designer | plan, do not implement, do not move the task out of In Progress; record the plan with `kanban context`; finish the design phase with `kanban done` (that does not complete the work) | any `move` is refused with *"designer cannot move a task; finish the design phase with kanban done"*; `done` without a recorded plan is refused with *"Designer cannot finish without recording a plan via context"* |
| reviewer | check the result against the task requirements and the project conventions in `AGENTS.md`/`CLAUDE.md`; do not edit project files; do not implement fixes; the only exit is `kanban verdict` | any `move` is refused with *"reviewer cannot move a task; finish with kanban verdict"*; `kanban done` from a review phase is refused with *"bot reviewer must finish with kanban verdict, not done"* |

Run phases, the queue dispatcher and the daemon that drive these rules are in
`docs/orchestration.md`.

### Development Rules
- All thresholds configurable via .kanban/config.yaml — no hardcoded values in business logic
- Atomic file writes (temp file + rename) via `storage::atomic_write_text`
- Any task read-modify-write cycle holds the board lock (`Storage::lock`)
- Context compaction is rule-based (no LLM)
- Tests required: `cargo test --locked` must stay green; golden fixtures in `tests/fixtures/` guard legacy board-format compatibility — never regenerate them from Rust output
- `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt` applied
- Release builds use the single `kanban4ai` binary; installers create relative `kanban` and `kb` symlinks
- No database dependencies
- Compatible with existing opencode plugins (doesn't modify opencode internals)
- Keep `AGENTS.md` and `CLAUDE.md` small; long-form material belongs in `docs/`. `sh scripts/token-budget.sh` enforces this

### Required checks
```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
sh -n scripts/install.sh scripts/test-packaging.sh
sh scripts/test-packaging.sh
sh scripts/token-budget.sh
```
