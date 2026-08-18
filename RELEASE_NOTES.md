# kanban4ai 0.4.6

## Highlights

- Copying selected text now goes through a native clipboard helper first
  (`pbcopy` on macOS, `wl-copy` on Wayland, `xclip`/`xsel` on X11, `clip.exe`
  under WSL). OSC 52 is only the fallback, and it is multiplexer-aware: tmux
  gets both the DCS passthrough and the bare sequence; `screen` gets chunked
  DCS passthroughs. Helper stdout is discarded so X11 helpers that daemonise
  cannot stall the TUI.
- `tmux new-session` is isolated from the TUI TTY (`-x`/`-y` size, `-c` work
  path, stdin/stdout/stderr detached; tmux stderr lands in
  `.kanban/logs/<session>.tmux.err`). A non-zero tmux exit takes the same
  background fallback as a missing tmux binary. The exact error is posted on
  the task thread and returned to the TUI status bar instead of `eprintln`.
- `operations` never writes to stderr while the TUI owns the terminal. After
  a TUI-initiated launch (run / revoke / re-run / revert, or an expired-wait
  relaunch) the event loop `terminal.clear()`s and fully redraws, same as
  after attach, so a leaked glyph cannot desync ratatui's buffer.
- A cleanly closed session on In Progress is idle, not crashed. `r` is Run
  again. Only a missing session file, a crashed record, or a stale heartbeat
  paints `✖ crashed · u recover`. Revoke stays reserved for a live, waiting,
  or crashed session.
- The assembled agent prompt is written to `.kanban/logs/<session>.prompt.txt`
  and the wrapper feeds it as `"$(cat -- <file>)"`. The prompt body is no
  longer placed on the tmux/`bash -c` argv.

## Verification coverage

- Clipboard helper selection per display server, tmux/screen OSC 52 wrapping,
  and screen chunking.
- Duplicate tmux session names fall back to a background process, write the
  agent log, and record the tmux error on the thread — never on stderr.
- Prompt-file wrapper tests assert the body is absent from the script argv
  and is delivered via `cat`.
- Closed-session TUI tests: idle card, `r run`, detail Run, `r` starts a
  fresh session; a missing session file still shows crashed.
- Release checks for this version include rustfmt, clippy with warnings denied,
  locked tests, a release build, and installer packaging smoke tests.
