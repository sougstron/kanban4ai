# kanban4ai 0.4.0

## Highlights

- Board data now lives in a central projects store instead of a local
  `.kanban/` inside the work folder. A project is two paths: the **work path**
  (agent cwd) and the **data root** (`<store>/projects/<id>/.kanban`). There is
  no pointer file in the repo. Store root resolution is `$KANBAN_HOME`, then
  `$XDG_DATA_HOME/kanban4ai`, then `$HOME/.local/share/kanban4ai`.
- `kanban init` registers the folder in the store and creates the board there.
  An existing local `.kanban` is migrated (rename first, verified copy on
  `EXDEV`). Repeat init is a no-op. Unregistered boards found in cwd are
  adopted silently, or left in place with a warning if sessions are active.
- New `kanban project` commands: list, add, show, rename, set-path, path,
  remove, and open. Every subcommand accepts global `--project`, and agent
  wrappers export `$KANBAN_PROJECT` / `$KANBAN_DATA_DIR` so callbacks stay
  unambiguous after `cd`. Prompt paths are absolute under the data root.
- The TUI adds a projects list (`P`). With no resolved project, `kanban` /
  `kanban tui` open that list instead of exiting. Rows show the work path,
  column counts, active sessions, and last opened. Unknown cwd gets a pinned
  create row; delete unregisters by default and can also purge board data.
- Tasks an agent moves into Review now show a yellow `●` unseen marker on the
  card and on the project row. Opening the task, answering, rerunning, or any
  human move clears it. The flag is omitted from frontmatter while false so
  legacy boards still round-trip byte-identically.
- The review-edits field now accepts Ctrl+Left/Right/Up/Down for word and
  paragraph motion, and Ctrl+Backspace / Ctrl+Delete to delete the previous or
  next word. Ctrl+S and Ctrl+R remain the save and re-run shortcuts.

## Verification coverage

- Store tests cover registry CRUD, cwd resolution, rename / EXDEV migration,
  `--copy` / `--force`, silent adoption, and in-place fallback.
- CLI tests cover `init`, `project` subcommands, global `--project`, and
  `$KANBAN_PROJECT` resolution.
- TUI tests cover the projects list, create-for-cwd row, and delete dialog,
  plus review-editor word motion/delete and Ctrl+S regression cases.
- Operations and golden-compat tests cover `review_unseen` set/clear paths and
  legacy frontmatter round-trip.
- Release checks for this version include rustfmt, clippy with warnings
  denied, locked tests, a release build, and installer packaging smoke tests.
