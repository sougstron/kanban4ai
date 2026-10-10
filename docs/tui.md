# TUI keyboard shortcuts

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when you are changing TUI key handling or dialogs.

## Tables in task threads

Markdown pipe tables in message bodies render as bordered, aligned columns
with a bold header. Both `| A | B |` and `A | B` rows are supported; the
header must be followed by a separator such as `| --- | ---: |`.
Colon markers select left, center, or right alignment. Escaped pipes (`\|`)
remain part of a cell.

Column widths follow the thread panel width. Long cell text wraps inside its
column (including wide Unicode text); at widths too small for a grid, rows
render as `Header: value` pairs instead. Thread scrolling and mouse text
selection use the rendered rows. Tables inside fenced code blocks remain
literal text. Other Markdown formatting is unchanged; messages on disk are
not rewritten.

## TUI Keyboard Shortcuts

Action hotkeys work on both the board (focused card) and the open detail view.

Detail type-to-edit: a Review task opens on the thread, so `Esc` closes the
detail at once. While the detail has a text panel (the Review editor, or the
answer box of an open question), a plain printable key never fires an action:
on the thread it moves focus into that panel (Review editor first) and is
typed there. `Tab` or a click focuses the editor explicitly. Actions then
live on `Alt+letter` from every panel (`Alt+y` approve, `Alt+r` run,
`Alt+Shift+f` run now, `Alt+x` reject, `Alt+q` close, …), shown as `M-y` on the
action buttons and status hints; buttons stay clickable. `Esc` leaves a text
panel, then closes the detail; `[`/`]`, arrows, PgUp/PgDn and the wheel still
drive the thread. Because every `Alt+letter` is a hotkey in the detail, the
textarea's own Alt bindings (`Alt+b/f/d/v`) are unavailable there — use
`Ctrl+←/→` to jump by word and `Alt+←/→`, `Ctrl/Alt+Backspace` or
`Ctrl/Alt+Delete` to delete a word. A detail with no text panel keeps
the plain-letter hotkeys.

- `↑/↓/←/→`: Move focus between tasks/columns
- `Alt+↑/↓`: jump to the block above/below from anywhere — the detail's
  thread/answer/editor panels, a dialog's form fields (the nested
  agent-settings and Options popups included) — even out of a text caret or
  a selector/card selection, where the plain arrows keep their local
  meaning. No wrapping: `Tab` stays the wrapping cycle
- Plain `↑/↓` in a dialog leave a field from its edge: a text field from its
  top line/end, a selector (chain-to, backend/model/effort/agent, the
  Executor middle/cheap slots, sort/theme/status pickers) from its first or
  last visible option; inside, they move the caret or selection
- `Alt+←/→`: switch the settings dialog's tabs from any field (text inputs
  and filtered selectors included). Where no tabs own the chord it deletes a
  word: in the detail's text panels and in every dialog text field `Alt+←`
  removes the word before the caret and `Alt+→` the one after it (the board
  keeps its columns plain-arrow only, and Yes/No confirmation prompts ignore
  the chord). As with `Alt+letter`, `Ctrl+Alt` is left
  alone — that is AltGr on some layouts
- `Tab` / `Shift+Tab`: Next/previous column (board) · cycle
  thread/answer/editor panels (detail)
- `Enter`: Show task detail. Between the Task and Thread panels an
  Analytics row shows cumulative agent run time, run count, and
  input/output tokens with the cache hit rate, plus a `Limits` row with the
  task's share of each provider window per role (`✎` designer, `▶` executor,
  `⚖` reviewer, in the card role colors) once one exists (`docs/stats.md`)
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
- `s`: Open Project Settings from Board or Detail. The dialog is split into
  five **tabs** — `Common │ Designer │ Reviewer │ Executor │ Prompt` — rendered as a
  labelled strip with a rule under it. **Left/Right switch tabs** (wrapping at
  the ends) whenever the focused field does not own those arrows — i.e.
  everywhere except text inputs, the per-backend cap lists, and the filtered
  selectors (backend/model/chain-to and the Executor slots); **`Alt+←/→`
  switch tabs from those fields too**; Tab/BackTab keep
  cycling inside the active tab, and a tab label is also clickable with the
  mouse. Each tab shows only its own page, but Save/Cancel sit under every
  tab and persist the **whole** dialog, so switching tabs never loses an
  edit; a validation error flips to the tab owning the offending field.
  Each inheritable group opens with a one-row `☑ Inherit <group> from global`
  checkbox (Space toggles; ticked by default on new boards). While ticked the
  group's fields show the global values dimmed, and Tab, the mouse and Save
  skip them; unticking makes them editable, starting from those global
  values. See `docs/config.md` ("Inheriting settings from the global config").
  - **Common**: project name, theme, then the agent group (default agent
    launcher), task sorting, the thread/restart group (hide kanban messages,
    crash-restart schedule), the limits group (queue switch, the four cap
    groups), and the read-only Worktree isolation row (`available`, or
    `unavailable — <reason>`; probed once when the dialog opens, since the
    probe runs git)
  - **Designer**: enable-for-all toggle and the designer bot's nested
    agent-settings launcher
  - **Reviewer**: enable-for-all toggle, launcher, where `--changes` verdicts
    drop the task, and the review bounce cap
  - **Executor**: the six ordered pool slots (`Middle 1st–3rd`, `Cheap
    1st–3rd`, filterable `backend/model` selectors annotated with live quota
    numbers from the limits cache and an `(out of quota)` mark), the resolved
    `next:` order line, and the two quota floors (`Week %`, `5h %`). See
    `docs/config.md` (`orchestration.executors`) and `docs/orchestration.md`
    (Executor Pools).
  - **Prompt**: one multi-line `Instructions for every task` box (Enter
    breaks lines, plain-text paste) stored as `instructions:` in
    `.kanban/config.yaml`; when not blank it is injected into every task's
    agent prompt. See `docs/config.md` ("Project Instructions").
  On the Projects screen `s` instead opens Global Settings (see "Global
  Settings"): the same tabs minus Prompt and the inherit checkboxes, project
  name, theme and isolation row — they edit the global values the projects
  inherit. Its Common tab adds the machine-only settings (Updates section,
  Esc-from-board, project sorting, update check on open).
- `e`: Edit task
- `d` / `Ctrl+d` / `Delete` / `Backspace`: Delete task
- `m`: Move task
- `w`: Open the answer-question dialog
- `y`: Approve — move a Review task to Done
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
with a status hint instead of being executed.

`Ctrl+V`, `Ctrl+Shift+V`, and `Shift+Insert` reach the board as keys when the
terminal does not turn them into a paste — notably the synthetic `Ctrl+V` that
dictation tools such as Handy send after placing a transcript on the clipboard.
With a text field focused the board reads the clipboard itself (`pbpaste`,
`wl-paste`, `xclip`/`xsel`) on the input thread the instant the key arrives,
because those tools restore the old clipboard moments later, and inserts the
text like a bracketed paste. Description, Review edits, and the answer
surfaces — the detail's answer panel and the answer dialog's custom field —
attach a clipboard image instead when there is no text (or the text is an
image file path). With no text field focused the keys keep their normal
meaning.

The mouse edits text fields (dialog text fields, the Review edits editor, and
the detail's custom-answer row) in place: a click puts the caret on the
clicked character (or the end of a shorter line), a drag selects inside the
field and copies the selection on release, and the wheel scrolls the field
under the pointer. Shift+drag keeps the plain screen-text selection. On the
answer panel the wheel has nothing to scroll — the preview is one row — so it
steps the variant list instead, exactly like the panel's ↑/↓ keys.

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
`Enter` opens the session log (same as `v`), `i`
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
The first frame uses registry names while `Loading project counts…` is shown.
Board-specific names and summaries arrive from a background scan; subsequent
scans reuse unchanged task counts, sessions and names independently. Session
wait expiry still updates counts without a file write. Only one scan runs at
a time, and rename/delete/path changes discard an older in-flight result.
Selection follows the project id when refreshed summaries change row ordering.
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
of being launched. `s` opens the Global Settings dialog, `S` (`Ы` in a Russian
layout) opens the read-only usage-stats report (tokens and time spent, by backend/model/project,
across every registered project — see `docs/stats.md`) in the text pager
(↑/↓ scroll one row, Shift+↑/↓ three, Ctrl+↑/↓ ten),
`d` opens the remove
dialog (unregister by default; Space toggles
“also delete board data”), `/` filters. `q` quits the TUI outright; `Esc`
returns to the board this list was opened from, or quits when the list is the
entry screen.

Board refresh tracks task, session and thread files separately. A heartbeat
refreshes session-derived card state without parsing every task again; a
thread post refreshes thread-derived flags and counts. Changed task files are
parsed through a per-file cache, and the Archive view uses the same snapshot.
An open detail reloads for its task/thread/session changes, not every unrelated
heartbeat; polling remains the fallback to filesystem notifications.

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
is visible: the card in flight hangs off the cursor at the offset it was
grabbed with, the slot it came from keeps its size as an empty dash-dot
rectangle (nothing else on the board shifts until the drop), every column the
pointer crosses — the source column included — gets a bold green border, and
the status bar shows `Moving <task> → <column>` with the action a release
would perform.

A drop carries the run semantics of the column it lands on, so the mouse
drives a run and not just a status change:

| Drop target | What happens |
|---|---|
| empty space in In Progress | move, then queue the task for the dispatcher (run phase `Queued`) |
| empty space in Review | stop the task's agent, move, then start every task chained to it |
| empty space in To Do / Done | stop the task's agent and move it |
| another card | chain the dragged task: it gets `chained_to = <card under the pointer>`, so it auto-starts when the drop target reaches Review. Neither card moves. |

Stopping is best effort — a task with no live agent is simply moved. The
chain start ignores the `auto_launch_chained` rule (the drop is an explicit
human request) and skips chained tasks that already left To Do.

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
with clickable, context-sensitive buttons (Run/Stop/Answer/Approve/Re-run/
Edit/Move/+Ctx/Revert/Del). An isolated task gets a meta line with the worktree
path (home-shortened), the branch, the `base_commit` short sha, and
`Integration: <state>` when set; a Conflict task also shows a bold
`⚠ Integration conflict — resolve in the worktree, then Re-run (Ctrl+R)` line,
its Re-run button is painted in the alarm color (the report sits in
review_edits, and re-dispatch after resolving is how a conflict gets acted on),
and the edits panel is retitled `conflict report`. When the task has open questions an inline
**answer panel** appears between the thread and the review-edits editor:
`←/→` switch between questions, `↑/↓` pick one of the agent's variants or the
custom-input row, typing fills the custom answer, `Enter` submits. Closing the
detail (or switching questions) saves the typed answer as a draft on the
question message, so reopening the task restores the text as it was left;
submitting the answer (or clearing the text and closing) drops the draft.
Cards with
open questions show the question text as a preview line; clicking it jumps
straight to the answer panel. Interactive tasks whose agent is blocked on
`kanban ask --wait` show a `⏳ waiting` badge; tasks in declared wait mode show
`⏳ until HH:MM`. A session that is actually crashed (status crashed, stale
heartbeat, or missing session file) shows `✖ crashed · u recover`. A cleanly
closed session on In Progress is idle — `r` runs a fresh agent; it is not
painted crashed.
A live design or review session also earns its own bold `▶ running` row under
the card's badges — blue for the designer, purple for the reviewer — and the
live token/cost stats line is tinted in the same role color (executor runs
keep the green badge-only card).
The review-edits editor is
editable only while the task is in Review (read-only or hidden otherwise), and
saving (`Ctrl+S`) no longer re-runs the agent — re-running is the separate
`Ctrl+R` / action-bar button. Unsaved editor text is written to the task's
`review_edits` buffer automatically when the detail closes, so reopening the
task restores it without an explicit save. Create/edit dialogs group the form
as: Title, Description, an `Agent settings` row that opens a nested popup for
backend, model, effort, and persona, a Readonly checkbox (investigation and
board-side reporting without project-file writes), a "Chain to task" selector,
and an `Options` row that opens a second nested popup holding the
Silence, Orchestrator, Designer, and Reviewer checkboxes plus the "Planned
launch" checkbox with an HH:MM time field. Silence forbids questions on this
task: the agent decides ambiguous points itself and records each dispute as a
suggestion. Orchestrator, Designer, and Reviewer are per-task opt-ins; models
and agents come from project settings. The Options summary names the toggles
that are on (`silence`, `orchestrator`, `designer`, `reviewer`, `launch at
HH:MM`), and a silence task shows a `🔇 silence` badge. The Description box sizes itself from its own text the way
the option selectors size from their option count: at least 5 visible text rows,
growing one row at a time with the soft-wrapped text up to 20 visible rows and
shrinking again on delete. The borders add two rows outside these limits.
Content beyond 20 rows scrolls within the field; a short terminal still clips
the field to the available space. Window size no longer stretches it — spare
rows go to the selectors below instead. When the selected backend exposes no personas (no
`agent_options`), the agent selector would offer only "Default agent", so the
`Agent settings` launcher is hidden entirely — in the task form and for the
default/designer/reviewer launchers in settings alike. Both popups stage
values on Save and restore the exact
opening state on Cancel. When planned launch is on the time is required (an
empty or invalid value keeps the dialog open with an error and reopens the
Options popup on the time field), and saving stores the next local occurrence
of that time as `launch_at`, which the queue dispatcher enqueues when it comes
due (see `docs/orchestration.md`). The card shows a `🕐 HH:MM` badge while the
schedule is pending, and the detail view lists the full timestamp. The TUI no longer exposes the legacy `interactive` switch:
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
from anywhere). Checkboxes toggle on Space only.

Up/Down also walk the dialog fields (without wrapping; Confirm/Cancel count
as one row). In a text field Down first moves the caret down and then to the
very end, Up to the very top; only from that edge (or in an empty field) does
the arrow leave for the next/previous field. Selectors and lists keep Up/Down
for their selection. In the task detail, Up from the top of the review editor
climbs to the answer panel (or thread), and the answer panel's variant list
hands off to the thread above and the review editor below. The TUI requests
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
