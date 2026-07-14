# kanban4ai

A fast native kanban board CLI and TUI designed for AI coding agents such as
opencode and Claude Code. Boards are plain Markdown and YAML under `.kanban/`:
there is no database or language runtime dependency.

The Rust rewrite is complete through phase 5. It preserves the original
`kanban-cli` command contract and can read existing board files, while shipping
as one canonical `kanban4ai` executable with `kanban` and `kb` aliases.

## Requirements

- Linux or another Unix-like environment
- Rust 1.88 or newer when building from source
- Optional integrations: `tmux`, `notify-send`, `wl-paste` or `xclip`
- Optional agent backends: opencode and/or Claude Code

## Install

From the canonical repository:

```sh
cargo install --locked --git https://github.com/sougstron/kanban4ai
```

`cargo install` installs only the canonical executable. To install the binary
and compatibility aliases after a source build, use the POSIX installer:

```sh
cargo build --release --locked
PREFIX="$HOME/.local" sh scripts/install.sh
```

`PREFIX` defaults to `/usr/local`; packagers can also set `DESTDIR`. The
installer refuses to overwrite any existing `kanban4ai`, `kanban`, or `kb`
path. Arch Linux source recipes are provided for `kanban4ai` and
`kanban4ai-git` under `packaging/aur/`.

Tags matching `v*` publish Linux x86_64 and aarch64 archives plus SHA-256
checksum files at <https://github.com/sougstron/kanban4ai/releases>.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
sh scripts/test-packaging.sh
```

The golden files in `tests/fixtures/` were produced by the earlier Python
implementation and must not be regenerated from Rust output; they preserve
legacy board-format compatibility.

## Usage / Quick start

Run these commands from the project directory that should contain the board.
The examples use `kanban`. A direct `cargo install` provides only
`kanban4ai`, so use `kanban4ai` instead, or install with `scripts/install.sh`
or a package that provides the `kanban` and `kb` aliases.

### 1. Initialize a board

```sh
kanban init
kanban create "Fix the login flow"
kanban list
kanban show TASK-001
```

`kanban init` creates the local `.kanban/` board. Tasks move through To Do,
In Progress, Review, Done, and Archive. The task ID is printed when a task is
created. Use `kanban tui` for the interactive board, or use the CLI commands
when scripting or working from an agent.

### 2. Delegate a task to an agent

The CLI form stores the backend, model, persona, and interactive behavior on
the task. For example:

```sh
kanban create "Implement the login flow" \
  --backend opencode \
  --model MODEL_NAME \
  --agent-name sisyphus \
  --interactive
kanban take TASK-002 --session ses-login --agent
```

`kanban take ... --agent` moves the task to In Progress and auto-launches the
configured backend when auto-launch is enabled. The TUI is easier for most
users: select a To Do task, press `s`, and confirm. The selected task's backend
must be installed and authenticated separately, for example by signing in to
opencode or Claude Code before delegation. `tmux` is optional, but enables an
attachable agent session when configured.

An agent can record work context and ask a question with:

```sh
kanban context TASK-002 "The API uses the existing session middleware."
kanban ask TASK-002 "Should the new endpoint use JSON or form data?" --agent
kanban answer TASK-002 MSG-003 "Use JSON."
```

For an interactive task, the agent can use `kanban ask ... --agent --wait
--session ses-login` to wait for the human answer. Questions and context are
stored in the task's thread. When the agent finishes, it runs
`kanban done TASK-002 --session ses-login --agent`, which moves the task to
Review. The human reviews the work and completes it with:

```sh
kanban done TASK-002
```

### 3. Review and rerun

If changes are needed, record feedback and rerun the task's agent:

```sh
kanban edits TASK-002 "Handle expired sessions and add a regression test."
kanban rerun TASK-002
```

The review edits are added to the thread when the task is rerun. The TUI also
provides a Review-edits field and a Save & Re-run action.

### 4. TUI essentials

Start the board with:

```sh
kanban tui
```

Use `↑`/`↓`/`←`/`→` to move focus, `Tab` or `Shift+Tab` to change columns,
`Enter` for task details, `s` to delegate, `n` to create, `e` to edit, `m` to
move, `w` to answer a question, `r` to recover a crashed task, `a` to view
archived tasks, `l` to view running sessions, `/` to search, `?` for help, and
`q` to quit. Press `Ctrl+T` to change and persist the theme.

### 5. Sessions, archives, and data

Useful maintenance commands are:

```sh
kanban sessions
kanban attach TASK-002
kanban archive
kanban archive-done
```

`attach` connects to a running agent's tmux session when one exists.
`archive-done` moves all Done tasks to Archive, while `archive` lists archived
tasks. Board settings are stored in `.kanban/config.yaml`, where you can tune
columns, agent backends, auto-launch, notifications, TUI settings, and timeouts.
Task Markdown files live under `.kanban/tasks/` in status subdirectories;
conversation threads are `.kanban/threads/`, and agent session state is in
`.kanban/sessions/`. The board is file-based and has no database.

See `AGENTS.md` for the complete command, configuration, data-model, and TUI
reference. kanban4ai is licensed under the MIT License.
