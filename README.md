# kanban4ai

A fast native local-first kanban board CLI and TUI designed for AI coding agents such as Opencode and Claude Code. Boards are plain Markdown and YAML under `.kanban/`:
there is no database or language runtime dependency. Work without strong link with git, you can start in any local folder with just two commands.

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

### 1. Initialize a board

```sh
kanban4ai init
kanban4ai
```

`kanban4ai init` creates the local `.kanban/` board. Tasks move through To Do,
In Progress, Review, Done, and Archive. The task ID is printed when a task is
created. Use `kanban4ai` or `kanban4ai tui` for the interactive board, or use the CLI commands that described below when scripting or working from an agent.

### 2. Run a task on an agent CLI

The CLI form stores the backend, model, persona, and interactive behavior on
the task. For example:

```sh
kanban4ai create "Implement the login flow" \
  --backend opencode \
  --model MODEL_NAME \
  --agent-name sisyphus \
  --interactive
kanban4ai take TASK-002 --session ses-login --agent
```

`kanban4ai take ... --agent` moves the task to In Progress and auto-launches the
configured backend when auto-launch is enabled. The TUI is easier for most
users: select a To Do task and press `r` — the agent starts immediately, with
no confirmation. The selected task's backend must be installed and
authenticated separately, for example by signing in to opencode or Claude Code
before running a task. `tmux` is optional, but enables an attachable agent
session when configured.

An agent can record work context and ask a question with:

```sh
kanban4ai context TASK-002 "The API uses the existing session middleware."
kanban4ai ask TASK-002 "Should the new endpoint use JSON or form data?" --agent
kanban4ai answer TASK-002 MSG-003 "Use JSON."
```

For one or more structured questions at once, the agent writes a strict YAML
form and submits it, so each question renders with selectable answer options:

```sh
cat > .kanban/forms/TASK-002.ask.yaml <<'YAML'
questions:
  - prompt: Which data format should the endpoint accept?
    options: [JSON, form data]
  - prompt: Any performance constraints to respect?
YAML
kanban4ai ask-form TASK-002 --file .kanban/forms/TASK-002.ask.yaml --agent
```

Agents are also prompted to file non-blocking ideas with
`kanban4ai suggest TASK-002 "<idea>"`.

For an interactive task, the agent can use `kanban4ai ask ... --agent --wait
--session ses-login` to wait for the human answer. Questions and context are
stored in the task's thread. For long external work that should outlive a
headless agent reply, the agent can declare a bounded wait instead:

```sh
kanban4ai waiting TASK-002 --session ses-login --eta 900 --note "waiting for CI"
```

The board records the wait in the thread, keeps the session alive until the
deadline (ETA with the configured safety multiplier), and relaunches the agent
after the deadline to check the result. Answering the task's last open question
wakes a declared-wait agent immediately; the TUI's `Revoke` action does the same
manually. The agent may call `waiting` again if it still needs more time. When
the agent finishes, it runs
`kanban4ai done TASK-002 --session ses-login --agent`, which moves the task to
Review. The human reviews the work and completes it with:

```sh
kanban4ai done TASK-002
```

### 3. Review and rerun

If changes are needed, record feedback and rerun the task's agent:

```sh
kanban4ai edits TASK-002 "Handle expired sessions and add a regression test."
kanban4ai rerun TASK-002
```

The review edits are added to the thread when the task is rerun. The TUI also
provides a Review-edits field (`Ctrl+S` saves it) and a separate Re-run action
(`Ctrl+R` or the action-bar button) that folds the saved edits into the thread
and relaunches the agent.

### 4. TUI essentials

Start the board with:

```sh
kanban4ai tui
```
or
```sh
kanban4ai
```

Use `↑`/`↓`/`←`/`→` to move focus, `Tab` or `Shift+Tab` to change columns,
`Enter` for task details, `r` to run the task on an agent (or revoke and wake
it when it is already In Progress), `n` to create in the focused column, `A` to
archive all Done tasks, `b` to mark all Review tasks Done (`R` also works), `e`
to edit, `m` to move, `w` to answer a question, `y` to approve Review → Done,
`t` to attach to the task's agent,
`c` to add context or a suggestion, `u` to recover a crashed task, `a` to view
archived tasks, `l` to view running sessions, `/` to search, `?` for help, and
press `Ctrl+C` twice within 3 seconds to quit. Press `s` on the board or task
detail to open Project Settings, where you can edit the project name, default
backend, that backend's default model/effort/persona, theme, and task sorting
(task number or modification date, oldest or newest first); the Board
status-bar hint is clickable when
it fits. `Ctrl+T` remains the quick theme toggle. All action keys work from
the board and from the detail view, which also offers clickable action buttons
and an inline panel for answering agent questions.

The sessions view marks each session `▶` live, `⏳` in a declared wait, or `✖`
crashed; there `Enter` attaches, `v` opens a scrollable pager over the session
log, `x` kills the session after a confirmation, and `o` jumps to the session's
task. Waiting rows and cards show the deadline (`until HH:MM`), while stuck
cards show the `u recover` hint. In the archive view `Enter` opens an archived
task and `u` restores it to To Do. The status bar shows the shortcuts for the
current screen and its hints are clickable.

### 5. Sessions, archives, and data

Useful maintenance commands are:

```sh
kanban4ai sessions
kanban4ai attach TASK-002
kanban4ai archive
kanban4ai archive-done
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
