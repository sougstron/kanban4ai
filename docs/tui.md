# TUI keyboard shortcuts

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when you are changing TUI key handling or dialogs.

## TUI Keyboard Shortcuts

Action hotkeys work on both the board (focused card) and the open detail view.

- `↑/↓/←/→`: Move focus between tasks/columns
- `Tab` / `Shift+Tab`: Next/previous column (board) · cycle
  thread/answer/editor panels (detail)
- `Enter`: Show task detail
- `r`: **Run (= queue) / Revoke** — put the task into the orchestration queue
  (To Do moves to In Progress with phase `queued`; Review folds its edits and
  joins the queue) and pump the queue once, so on an idle board the task starts
  on the spot while a full board parks it with the `⏸ queued` badge. When the
  queue could never drain (`queue_enabled: false` or auto-launch off) `r`
  falls back to the direct launch and says so in the status line. For an In
  Progress task whose session is still live or crashed, `r` stays Revoke: it
  kills the run and wakes a fresh one (the one human action that still
  bypasses the queue). On a paused card (declared wait) `r` revokes too, but
  the wake re-enters the queue instead of launching past the caps; `F` is the
  unconditional direct override there as well. A cleanly closed session stays
  idle: `r` queues a
  fresh run, not recover (the board is human-managed and agent-executed;
  "delegate" terminology and its confirmation dialog were removed)
- `F`: **Run now** — the direct launch `r` used to do: start the agent
  immediately, bypassing the queue and its caps (debug escape hatch). Also a
  detail action-bar button (`⚡ Now F`)
- `k`: **Stop** — kill a live or waiting agent session on the focused In Progress
  task (or its detail). The task stays In Progress so `r` can run it again.
  Confirm first. Distinct from revoke (`r`), which stops and immediately starts
  a fresh session. Sessions view still uses `x` to kill a selected session.
- `Q`: **Queue / Unqueue** — on an idle card a synonym of `r` without the pump
  (To Do moves to In Progress, an idle In Progress task stays put; phase
  becomes `queued` and nothing launches), or take an already-queued task back
  out. The status bar hint flips between `Q queue` and `Q unqueue`; a task with
  a live session cannot be queued
- `n`: New task — always created in To Do, regardless of the focused column
- `s`: Open Project Settings from Board or Detail: project name, default agent
  settings (backend/model/effort/persona) through a nested launcher, dark/light
  theme, task sorting, and the whole `orchestration:` block (queue switch, the
  four cap groups, crash-restart schedule, designer and reviewer bots, each with
  its own nested agent-settings launcher), plus a read-only Worktree isolation row
  (`available`, or `unavailable — <reason>`; probed once when the dialog opens,
  since the probe runs git). On the
  Projects screen `s` instead opens Global Settings (see "Global Settings").
- `e`: Edit task
- `d` / `Ctrl+d` / `Delete` / `Backspace`: Delete task
- `m`: Move task
- `w`: Open the answer-question dialog
- `y`: Approve — move a Review task to Done
- `t`: Open the task's agent session — attach when it is a live tmux session,
  follow the log when the agent runs in the background (no terminal to attach
  to), or reopen the recorded conversation with its backend-specific resume
  command when its session has stopped
- `c`: Add a context/suggestion message to the task thread
- `u`: Recover crashed task (restore to To Do); on an archived task (Archive
  list or its detail) the same key restores it to To Do after a confirmation
- `Ctrl+r`: Fold saved review edits into the thread, re-queue the run (a free
  slot starts it on the spot; a full board parks it `⏸ queued` — same fallback
  to the direct launch as `r` when the queue is off), and switch board focus to
  the task in In Progress (closes Review detail)
- `Ctrl+s`: Save the review-edits buffer (detail; save only, no re-run)
- `a`: Show archived tasks
- `A`: Confirm archiving all Done tasks
- `R`: Confirm marking all Review tasks Done
- `l`: Show running sessions
- `P`: Open the projects list (from Board, Detail, Archive, Sessions; not while typing). The same physical key works on a Russian layout (`З`).
- `Esc` on the Board: clears an active search filter; if the global
  `tui.escape_to_projects` setting is on and the filter is empty, opens the
  projects list
- `Ctrl+t`: Quick theme toggle (persisted to config)
- `/`: Search
- `?`: Help overlay (scrollable, sized to its content; lists mouse gestures)
- `q`: Back from detail/secondary screens — on the Projects screen `q` quits
  the TUI; quit the TUI with `Ctrl+C` twice

Clipboard pastes use bracketed paste: the whole block is inserted into the
focused text field in one edit (flattened to a single line for one-line fields
such as Title, search, and the answer box). Without it the terminal replays a
paste as key events, so tabs jump between dialog fields, newlines press the
focused button, and a paste on the board fires one shortcut per character — the
way earlier boards ended up with tasks whose title and description were random
fragments of the pasted text. A paste with no text field focused is dropped
with a status hint instead of being executed. `Ctrl+V` (image paste from the
clipboard) is unaffected.

Copying (drag across text on the board, then release) puts the selection on the
system clipboard through a native helper first — `pbcopy` on macOS, `wl-copy`
when `WAYLAND_DISPLAY` is set, `xclip`/`xsel` when `DISPLAY` is set, `clip.exe`
under WSL — and only falls back to the OSC 52 escape when no helper exists, as
on a remote session. The helper runs first because OSC 52 is write-only and
fails silently: tmux drops it unless `set-clipboard`/`allow-passthrough` are
enabled and several terminals refuse clipboard writes, which leaves the status
bar reporting a copy that cannot be pasted anywhere. The fallback wraps the
sequence in the tmux DCS passthrough (sending the bare form too, since only one
of the two survives any given tmux configuration) and in chunked DCS
passthroughs under `screen`. Helper output is discarded rather than captured
because helpers that daemonise to own the X11 selection hold the inherited
pipes open; a helper still resident after the handoff counts as success.

Sessions view: each row shows the session state (`▶` live heartbeat, `⏳`
declared wait, `✖` crashed), its task, the token count, the agent's todo
progress and its last activity; waiting rows also show the relaunch deadline.
`Enter` opens the session (attach / follow / resume, as for `t` above), `i`
opens a read-only session-info panel (elapsed time, tokens, cost, todos, last
activity, and the input provenance harvested so far) in the text pager, `v`
opens a scrollable pager over the tail (last 64 KB) of `.kanban/logs/<id>.log`
that follows new output on the refresh tick, `x` kills the session after a
confirmation (`Operations::stop_session`), and `o` opens the session's task
detail — `Esc` returns to the sessions list. Archive view: `Enter` opens the archived task's
detail (its action bar offers only Restore/Delete), `u` restores the selected
task to To Do after a confirmation.

Projects view: a table with a labelled header and two-line rows. The
name is the board's Project Settings `tui.name` when that is set to
something other than the default `Kanban`; otherwise the registry name
(folder basename at add time, or a later `project rename`). The
`~`-shortened work path sits on the second line (struck through when
the folder is missing). Count columns (To Do / Doing / Review / Done)
stay right-aligned under their labels; Agents (`▶N` when live, `⏸N` when
queued, retrying, or waiting) and Last opened drop on a narrow terminal
rather than squeezing the name.
A yellow `?` marks open questions and a `●` marks unseen Review work,
both in a flags column left of the name.
The selected row carries a border-coloured background; the row the mouse
rests on is preselected with a fainter `theme.hover` background, so the
pointer target is visible without moving the keyboard selection.
When the current directory is not registered, a pinned
`+ Create project for <cwd>` row is first: `Enter` or `n` on it registers
immediately (name = folder basename; a local `.kanban` is migrated). `n` on a
normal row opens a path+name dialog. `r` renames, `p` changes the work path,
`o` (status-bar `o folder`) opens the selected row's work folder in the
desktop's own file manager — outside the TUI, in a real window, using
`tui.file_manager` or the platform default chain (see "Global Settings"); on
the pinned create row it opens the folder that row offers to register. The
opener is spawned detached with its streams closed so it cannot write over the
frame, and a folder that no longer exists is reported in the status bar instead
of being launched. `s` opens the Global Settings dialog, `S` opens the
read-only usage-stats report (tokens and time spent, by backend/model/project,
across every registered project — see `docs/stats.md`) in the text pager,
`d` opens the remove
dialog (unregister by default; Space toggles
“also delete board data”), `/` filters. `q` quits the TUI outright; `Esc`
returns to the board this list was opened from, or quits when the list is the
entry screen.

The open project is named in two places, both free of screen space. On screen,
a ` ▸ <name> ` badge is right-aligned into the top border row of the rightmost
block — the row that already carries that block's own title — on Board, Detail,
Sessions and Archive, so a board opened in one of several terminals identifies
itself without leaving the screen. It degrades on its own ladder (full name →
truncated → dropped once fewer than four columns of name would survive) so it
never collides with the title it shares the row with, it is hit-tested ahead of
the column underneath it and clicking it opens the Projects list, and it is
suppressed on the Projects screen, which has no open project to name. Off
screen, the terminal window title is set to `<name> — kanban4ai` (project first,
because tab bars truncate from the right) whenever the open project changes,
including after a child process that renamed the terminal hands it back; the
name is collapsed to one line of printable text and clipped to 64 columns
before it goes into the escape. The title found on entry is saved and restored
with the XTWINOPS title stack (`ESC[22;2t` / `ESC[23;2t`) alongside the
alternate-screen teardown, on the panic path too.

The status bar is contextual per screen (Board, Detail, Sessions, Archive,
Projects, log view); it is an informational hotkey panel and not clickable —
nothing in it reads as a button, so it registers no hitboxes. When the
terminal is narrow the least important segments are dropped instead of
clipping. Column headers show
only the column name and visible task count. Drag a card to a different
column to move it in human mode. A single click on a card opens its detail;
a drag still moves it between columns without opening the detail view. The drag
is visible: the card in flight is inverted, the destination column's border
turns green and bold once the cursor crosses into it, and the status bar shows
`Moving <task> → <column>` so the pending move is never ambiguous.

Cards have exactly one selection, driven by whichever input moved last.
Hovering a card *is* selecting it — `Enter` and every card hotkey act on the
card under the pointer — and the next keyboard navigation moves that selection
away for good: the card a stationary pointer rests on stops being painted as
selected until the pointer moves onto a card again. Hover-steering is
suspended mid-drag (a lifted card keeps the selection) and while a modal is
open.

Note: the opencode subscription/usage overlay (`u` in the Python version) was
dropped in the rewrite — it never worked reliably; `u` now means recover.

The detail view renders the thread (open questions, variants, suggestions,
resolved entries) plus the task's `chained_to` target, and a bottom action bar
with clickable, context-sensitive buttons (Run/Stop/Answer/Approve/Re-run/Attach/
Edit/Move/+Ctx/Revert/Del). An isolated task gets a meta line with the worktree
path (home-shortened), the branch, the `base_commit` short sha, and
`Integration: <state>` when set; a Conflict task also shows a bold
`⚠ Integration conflict — resolve in the worktree, then Re-run (Ctrl+R)` line,
its Re-run button is painted in the alarm color (the report sits in
review_edits, and re-dispatch after resolving is how a conflict gets acted on),
and the edits panel is retitled `conflict report`. When the task has open questions an inline
**answer panel** appears between the thread and the review-edits editor:
`←/→` switch between questions, `↑/↓` pick one of the agent's variants or the
custom-input row, typing fills the custom answer, `Enter` submits. Cards with
open questions show the question text as a preview line; clicking it jumps
straight to the answer panel. Interactive tasks whose agent is blocked on
`kanban ask --wait` show a `⏳ waiting` badge; tasks in declared wait mode show
`⏳ until HH:MM`. A session that is actually crashed (status crashed, stale
heartbeat, or missing session file) shows `✖ crashed · u recover`. A cleanly
closed session on In Progress is idle — `r` runs a fresh agent; it is not
painted crashed. The review-edits editor is
editable only while the task is in Review (read-only or hidden otherwise), and
saving (`Ctrl+S`) no longer re-runs the agent — re-running is the separate
`Ctrl+R` / action-bar button. Create/edit dialogs expose one `Agent settings`
row that opens a nested popup for backend, model, effort, and persona; popup
Save stages those values in the task form and popup Cancel restores the exact
opening state. They also expose Designer and Reviewer checkboxes (per-task
opt-in; models and agents come from project settings), and a "Chain to task"
selector. The TUI no longer exposes the legacy `interactive` switch:
TUI-created tasks use `interactive: false`, and TUI edits leave an existing
value untouched; CLI/YAML compatibility remains. The backend selector leads with
"Default backend" (model/effort/agent have matching Default entries). Saving
with those selected snapshots the board's current defaults onto the task —
`auto_launch.default_agent` and that backend's configured model/effort/agent —
so the detail view and usage stats show the concrete values instead of
`-`/`default`/`unknown`. The selector labels still show what Default would
resolve to.

Dialog fields advance on Enter as well as Tab, except in multi-line text
areas (task Description, Add-message body, custom Answer): those insert a
newline on Enter, Shift+Enter, and Alt+Enter. Many terminals — and tmux
without `extended-keys` — deliver Shift+Enter as a bare Enter, so the field
must treat that the same as the modified chords. Tab still leaves the field.
Enter only submits once focus has reached the Save button (`Ctrl+S` submits
from anywhere). Checkboxes toggle on Space only. The TUI requests
`DISAMBIGUATE_ESCAPE_CODES` at startup where the terminal supports it
and pops the flag again for foreground children and on every teardown path.

The Backend, Model and "Chain to" selectors carry a filter row as their first
line (shown as `/ …`). Typing narrows the list case-insensitively on the option
label, including the leading "Default …" / "No chain" entry; Backspace edits
the filter and Delete clears it. Arrow keys step only through visible matches,
and the selection follows the filter, so narrowing to a single match leaves it
selected and one Enter both picks it and advances. Enter on a filter that
matches nothing is an error: the section border and filter row turn the theme's
error colour and focus stays put, cleared again by any edit to the filter or
any selection. A selector that has no options at all is not an error — Enter
walks past it. The remaining selectors (effort, agent, status, theme, sorting)
have no filter row: their lists are short and fixed, so the row would cost a
line of the dialog without saving a keystroke.

A filter lasts only as long as the visit that typed it. Every focus change —
Tab, Enter, Shift+Tab, or a click on another field — clears the filter of the
field being left along with any error it was showing, so returning to a
selector always starts from the full list rather than a stale narrowing. The
option that was picked while filtered stays selected.
