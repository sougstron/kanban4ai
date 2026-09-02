# Token profile: what kanban4ai actually costs an agent

Measured, not estimated. Source data is the board's own run history:
252 `.kanban/logs/*.prompt.txt` files and 240 matching
`*.transcript.jsonl` transcripts across 12 project boards, using the
`usage` objects the backends themselves reported.

Reproduce with:

```sh
python3 scripts/profile-tokens.py --logs <board>/.kanban/logs
python3 scripts/profile-tokens.py --cross-project
sh scripts/token-budget.sh
```

Prompt-side token counts use `tiktoken` cl100k_base — not any vendor's exact
tokenizer, but stable and within a couple of percent on English prose. Every
context figure taken from a transcript is the backend's own reported number.

## Headline

**The board's prompt is not the problem.** kanban4ai injects ~1.2k tokens per
session. The context an agent actually starts with is 20k–54k. kanban's own
share is **2.3%–6.3%**.

The thing that is expensive, and that the board is responsible for, is
**`AGENTS.md` — 29,050 tokens** that opencode/omp/pi auto-load into every
session before the agent reads a single line of code.

## 1. What the board writes, per stage of a task's life

Median tokens, current prompt format (109 sessions):

| Stage | n | kanban prompt | contract | role block | thread replay | user's task |
|---|---|---|---|---|---|---|
| designer (plan pass) | 5 | 1,121 | 945 | 58 | 0 | 27 |
| executor, first launch | 43 | 1,214 | 892 | 65 | 0 | 131 |
| executor, relaunch/resume | 61 | 1,791 | 892 | 65 | 419 | 68 |

Across the whole history: 66.2% of kanban prompt tokens are fixed
boilerplate, 22.8% is replayed thread context, and only **10.9% is the text
the human actually wrote**.

Thread replay is the part that compounds — median 419, p90 1,900, max 4,163.

## 2. Where the 895-token session contract goes

Two bullets are half of it:

| Tokens | Bullet |
|---|---|
| 297 | the detach / backgrounding / `waiting` explanation |
| 171 | the ask-form YAML schema |
| 56 | "final command of your reply must be…" |
| 52 | long-running foreground commands are safe |
| 44 | backup-before-edit |
| 275 | the remaining 10 bullets combined |

Both of the big two are **conditional-use features** whose full contract is
already available on demand: `kanban4ai detach --help`, `kanban4ai waiting --help`
and `kanban4ai ask-form --help` each print the same rules clap already stores.

## 3. The backend tax — measured across 12 boards

A single board cannot separate a backend's own fixed cost from the repo docs
it auto-loads, because both are constant within that board. Several boards
with different-sized `AGENTS.md` files can. Fitting
`turn-1 tax = floor + slope x AGENTS.md`:

| Backend | boards | floor (own system prompt + tool schemas) | AGENTS.md slope | r |
|---|---|---|---|---|
| pi | 7 | **3,951** | 0.56 | 1.00 |
| claude | 8 | 20,025 | **-0.06** | -0.20 |
| omp | 7 | 21,287 | 0.49 | 0.99 |
| opencode | 9 | 32,524 | 0.68 | 0.90 |

Two results matter:

* **claude ignores `AGENTS.md` entirely** (slope ≈ 0; it reads `CLAUDE.md`,
  441 tokens). opencode/omp/pi all pull a large fraction of it in.
* **The backends' own floors differ by 8x.** `pi` starts a session with ~4k of
  system prompt and tools; `opencode` starts with ~32.5k.

What this repo's 29,050-token `AGENTS.md` therefore costs, every session:

| Backend | context burned by AGENTS.md | measured turn-1 total |
|---|---|---|
| claude | 0 | 19,648 |
| omp *(current default)* | 14,174 | 36,359 |
| pi | 16,395 | 21,620 |
| opencode | 19,747 | 54,346 |

## 4. Amplification

Two multipliers turn a fixed prompt cost into a large one.

**Per turn.** Every turn re-sends the whole conversation, so a token in the
opening prompt is re-read on every turn. Median run length is 76 turns
(claude), 35 (omp), 36 (pi). `AGENTS.md` at 29k is therefore ~1.0M token-reads
per omp session; the kanban prompt is ~42k.

**Per relaunch.** Every relaunch calls `build_launch_plan` → `build_agent_prompt`
and rebuilds the entire prompt from scratch (`src/agent/prompt.rs:13`). 52 of
142 tasks were launched more than once, up to 12 times; **44.9% of all
board-fixed tokens were re-paid on a relaunch** rather than a first launch.

## 5. Limits vs context window — these are different problems

Claude runs show a **97.2% cache hit rate** (404.2M cache-read vs 11.8M
cache-write vs 39k uncached input across 58 runs). Weighting cache reads at
0.1x and writes at 1.25x, caching is already absorbing **86.7% of input
cost**. Reported spend: $191.40 over 55 runs, median $2.15/run.

So **subscription-limit pressure is largely already solved by caching**, and
shaving prompt tokens buys little there. What caching does *not* fix is
**context-window occupancy**: a 29k `AGENTS.md` holds 29k of the window from
turn 1 to the end of the run whether it was cached or not. That is the pressure
worth attacking.

## 6. Where to cut, in order of payoff

### A. Split `AGENTS.md` — by far the largest lever

29,050 tokens, auto-loaded by 3 of 4 backends. The bulk of it is reference
material an executor almost never needs:

| Tokens | Section |
|---|---|
| 3,891 | TUI Keyboard Shortcuts |
| 2,858 | Provider Subscription Limits |
| 2,268 | Worktree Isolation |
| 1,833 | CLI Commands (implemented) |
| 1,330 | Run Phases |
| 1,325 | Headless Dispatcher Daemon |
| 967 | Updater |
| 634 | Change Logs and Version Updates |

Keep a lean `AGENTS.md` (~4–6k: project shape, hard rules, required checks,
pointers) and move the rest to `docs/*.md` that an agent reads only when the
task touches that subsystem.

Trimming 29,050 → 6,000 saves, per session: **11.2k (omp), 13.0k (pi),
15.7k (opencode), 0 (claude)**.

Implemented after this baseline: `AGENTS.md` is now below 3k tokens, reference
sections live in `docs/*.md`, and `scripts/token-budget.sh` enforces a 6k ceiling.
The ask-form schema was moved into `ask-form --help` before leaving the core doc.

### B. Pick the backend for its floor

The floors are repo-independent and differ by 8x: **pi 3,951 vs opencode
32,524**. The board currently defaults to `omp` (21,287). Combined with the
`AGENTS.md` trim, a `pi` session would start at ~8k of context against the
36.4k an `omp` session starts at today — a **~78% reduction**.

This is a per-task setting already, so it can be routed: cheap/mechanical
tasks to a low-floor backend, and only genuinely hard ones to the expensive
one.

### C. Trim the session contract, ~430 tokens

Replace the 297-token detach block and the 171-token ask-form schema with a
single pointer line, since `--help` already carries both contracts in full:

> Long-running/detached work and YAML ask-forms have their own contracts:
> run `"$KANBAN_CMD" detach --help` or `"$KANBAN_CMD" ask-form --help`
> before using them.

Contract goes 895 → ~465. Small next to (A), but it is free, it applies to
**every** backend including claude, and it is re-paid on every relaunch.

Cheaper still: emit the detach block only when the task plausibly needs it —
`task.interactive` already gates a similar line.

### D. Stop re-paying the contract on relaunch

44.9% of board-fixed tokens go to relaunches that rebuild the whole prompt.
Where a backend supports native session resume (`claude --resume`, opencode
session ids), resuming and sending only the delta would drop nearly all of it.
This is the largest structural saving after (A), and unlike (A) it also cuts
the thread replay, which is the part that grows with task age.

The first implementation now covers automatic pi/omp relaunches. Their native
conversation id is already captured in each provenance manifest, so no task
format change was needed. The launcher selects the newest completed manifest
for the same task/backend and uses `pi --session <id>` or
`omp --resume <id>`. (Despite pi's help wording, `pi --resume` is an interactive
picker; `--session` is the exact non-interactive form.) The new turn contains
only the replacement board session identity and messages added after the prior
run. Missing manifests, backend changes, first launches, human restarts, and
reverts automatically retain the old full-prompt path. Claude and opencode
remain separate follow-ups because their resume interfaces and failure modes
differ.

### E. Cap the thread replay

`append_thread_context` (`src/agent/prompt.rs`) replays every non-system,
non-rejected message with no budget. These are not tokens spent by the Rust
app itself: kanban renders stored thread messages into the next agent's input,
so the provider counts them against that agent's context and limits. Median
419 tokens is fine; the p90 of 1,900 and max of 4,163 are not. A common source
of accidental duplication is now removed: if a run explicitly posted an
agent `context`, its captured whole-session `agent-reply` is not rendered again
for a fresh launch. Native resume goes further and renders only messages after
the previous run, since that conversation already contains its own output.

Further budgeting means setting a configurable maximum
for this section (for example 2k tokens), walking newest-to-oldest, keeping
recent messages whole, and replacing the omitted prefix with a deterministic
notice or rule-based summary. Questions still awaiting answers and the latest
human answer must be pinned so truncation cannot hide the task's blocking
state. Use the existing cheap token estimator rather than adding an LLM call;
the same clamp pattern in `core/reply.rs` is the model. Store the full thread
unchanged—the budget applies only when rendering a launch prompt. With native
resume this becomes primarily a fallback for fresh launches, not something
paid on every wake.

## 7. Automation

* `scripts/profile-tokens.py` — re-runs this whole profile against live board
  logs. `--cross-project` refits the backend cost model; `--json` emits raw
  per-session rows.
* `scripts/token-budget.sh` — fails when `AGENTS.md`/`CLAUDE.md` exceed their
  token budget (`AGENTS_BUDGET`, `CLAUDE_BUDGET`). Wire it in next to
  `cargo fmt --check` so the saving from (A) cannot silently erode. Uses
  tiktoken when available, bytes/4 otherwise (within 1.3% here).
