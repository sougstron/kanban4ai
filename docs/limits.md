# Provider subscription limits

Reference detail split out of [AGENTS.md](../AGENTS.md) so it is not
auto-loaded into every agent session. Read it when you are touching `core/limits.rs` or `tui/limits.rs`.

## Provider Subscription Limits (`core/limits.rs`, `tui/limits.rs`)

How much of each AI subscription window is left, drawn as one row directly
above the status bar on the Board and Projects screens (`✳ claude 5h 66% ↻3h30m
· 7d 95% ↻6d11h │ ✕ grok 7d 93% ↻4d22h │ ◆ zai
5h 85% ↻4h48m · 7d 97% ↻6d23h │ ✦ synthetic 5h 91% ↻3h59m · 7d 12% ↻3h22m │ ◉ yolo
24h 95%`), and
printed by `kanban limits`. Percentages are what remains (100 − used), not what
is spent.

Sources, all read-only and best effort:

- **claude**: the statusline bridge first. Claude Code (>= 2.1.80) pipes
  `rate_limits` to its statusLine command on every turn, so
  `kanban limits bridge install` wraps the configured statusline command
  (default `~/.claude/settings.json`, `$CLAUDE_CONFIG_DIR` respected) with a
  generated shim at `<store>/claude-statusline-bridge.sh` that tees each
  payload into `<store>/claude-rate-limits.json` while the original command
  keeps rendering the status line; `kanban limits bridge remove` restores it
  (a one-time `settings.json.kanban4ai-bak` is left next to the settings, the
  pre-bridge command in `claude-statusline-bridge.original`). Yields the
  `five_hour` (`5h`) and `seven_day` (`7d`) windows with `used_percentage` and
  epoch-seconds `resets_at` (the OAuth spellings `utilization`/RFC 3339 are
  tolerated). While *every* bridge window has yet to reset the usage endpoint
  is not polled at all; the moment one of them has rolled over the bridge can
  no longer say what the window that replaced it holds (an `any` test kept the
  spent `5h` reading on the row indefinitely, because `7d` resets days out).
  Second source: `GET https://api.anthropic.com/api/oauth/usage` with the OAuth
  access token from `~/.claude/.credentials.json` (`claudeAiOauth.accessToken`)
  and `anthropic-beta: oauth-2025-04-20`. The endpoint allows only a handful of
  requests per access token and then answers 429 for hours, so it is polled at
  most once every 15 minutes (`CLAUDE_USAGE_MIN_INTERVAL_SECS`, remembered in
  `<store>/claude-usage-poll` so a run of CLI processes shares one interval);
  `kanban limits --refresh` and a tap on the claude segment are a user asking
  now: both skip the interval *and* the current-bridge short-circuit, so a
  tap hours after the last Claude Code turn still hits the endpoint. The
  two sources are then merged window by window: for each label the fresher
  observation wins, except that a window which has already reset never
  displaces one that is still running, and `observed_at` becomes the oldest
  observation that survived, so the row never claims to be fresher than the
  stalest number on it. When the stored access token has expired (`expiresAt`,
  5-minute skew) or the endpoint answers 401, the stored refresh token is
  traded for a new one at `POST https://platform.claude.com/v1/oauth/token`
  (`grant_type=refresh_token`, Claude Code's public `client_id`) and the
  rotated pair is written back into `claudeAiOauth`, preserving every other
  field and the file's `0600` mode — the grant rotates the refresh token, so
  keeping the new one private would strand Claude Code with a retired one.
  Note the bridge only fires for interactive Claude Code sessions (`--print`
  runs do not invoke the statusline), which is also when the subscription
  windows actually move; the endpoint is what covers the hours in between.
- **codex**: the OpenAI subscription, which backs the codex CLI *and*
  opencode's `openai/*` models — both spend the same quota, so the row covers
  both. Three sources, newest `observed_at` winning. (1) The codex app-server
  JSON-RPC (`codex -s read-only -a untrusted app-server`, `initialize` then
  `account/rateLimits/read`) answers with live server-side numbers and costs no
  usage; `fetch_all` uses it on its own `CODEX_RPC_MIN_INTERVAL_SECS` (300s)
  poll interval — persisted in `<store>/codex-rpc-poll` so a run of CLI
  processes shares it — and a click on the segment skips that interval.
  (2) Without codex installed, the newest `rollout-*.jsonl` under
  `$CODEX_HOME/sessions/YYYY/MM/DD/` (default `~/.codex`) is streamed for its
  last `rate_limits` payload (`primary`/`secondary` with `used_percent`,
  `window_minutes`, epoch `resets_at`); those numbers are only as fresh as the
  last codex run, so the row appends their age (`(7d old)`). (3) An agent run
  that dies on a 429 hands the `x-codex-*` response headers to
  `record_codex_usage`, which is the only live source on a machine that drives
  OpenAI exclusively through opencode.
- **grok**: `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits`
  with the key and user id from `~/.grok/auth.json` plus
  `X-XAI-Token-Auth: xai-grok-cli`. Yields one window for the current billing
  period (`creditUsagePercent`, `currentPeriod.type`/`.end`).
- **zai**: `GET https://api.z.ai/api/monitor/usage/quota/limit` with the GLM
  Coding Plan API key opencode stores in `~/.local/share/opencode/auth.json`
  (`zai-coding-plan.key`, `$XDG_DATA_HOME` respected). Yields the 5-hour and
  weekly credit windows of the coding plan: one entry per `data.limits[]`
  (`unit`×`number` encode the window length; `nextResetTime` is Unix
  milliseconds), with the exact used percent from `currentValue`/`usage` and
  the integer `percentage` as fallback. The zai key never expires, so its
  segment needs no CLI-driven click refresh.
- **synthetic**: `GET https://api.synthetic.new/v2/quotas` (documented as free —
  the call never counts against the subscription) with `$SYNTHETIC_API_KEY` or
  the `synthetic.key` entry opencode's connect flow stores in the same
  `~/.local/share/opencode/auth.json`. Yields the rolling 5-hour request window
  (`rollingFiveHourLimit.remaining/max`, falling back to
  `subscription.requests/limit`, rolling over at `subscription.renewsAt`) and
  the weekly credit window (`weeklyTokenLimit.percentRemaining`, regenerating
  at `nextRegenAt`). Both quotas regenerate in small ticks rather than resetting
  on a timer, so the reset time is the next capacity gain. The key never
  expires, so the segment needs no CLI-driven click refresh.
- **yolo**: `GET https://yolo-auto.com/v1/usage` with `$YOLO_API_KEY` /
  `$YOLO_AUTO_API_KEY` or the custom yolo provider's `apiKey` from opencode
  (`~/.config/opencode/opencode.json`, `$XDG_CONFIG_HOME` respected), omp
  (`~/.omp/agent/models.yml`), or pi (`models.json` under
  `$PI_CODING_AGENT_DIR`, default `~/.pi/agent`). A provider counts as yolo
  when its id/name contains `yolo` or its `baseURL` points at `yolo-auto.com`.
  The endpoint publishes counters but no quota — `limits.requests` and
  `remaining.requests` are `null` on the current plans, which is why the older
  request-window parse showed nothing — so the ceiling comes from the plan
  itself: `YOLO_DAILY_TOKEN_LIMIT`, the 40,000,000-token rolling day of
  Standard pressure. Only that window (`24h`) is drawn; the plan's
  8,000,000-token hour is deliberately left out because the response carries no
  per-hour counter. Spend is the larger of `usage.byModel[].past24h.totalTokens`
  (truly rolling, but only this key) and `usage.day.project.totalTokens`
  (every key of the project, but a UTC calendar bucket that drops to zero at
  midnight): each is a lower bound on the real rolling day, and the row must
  never promise capacity that is already gone. The window is `rolling` with no
  reset time — a rolling budget frees capacity token by token, so there is no
  rollover instant to count down to. The key never expires, so the segment
  needs no CLI-driven click refresh; the plan's own guidance is to honor
  `Retry-After` on HTTP 429 and retry with jitter rather than to poll harder.

HTTPS goes through `curl -K -`, with the request config (URL and headers) piped
on stdin: no TLS dependency is linked into the crate, and bearer tokens never
appear in a command line where `ps` would expose them. `curl` is an optional
dependency — without it claude, grok, zai, synthetic, and yolo degrade to `n/a`.

A provider with no credentials on the machine reports `not_configured` and is
omitted from the row entirely; `401`/`403` becomes `signed out`. Fetches run on
a background thread started from the event loop (never `App::new`, so no test
or non-TUI caller polls a provider), and results are cached in memory and in
`<store>/limits.json` with a `limits_refresh_interval` TTL, because the claude
usage endpoint rate-limits frequent polling. Saving that snapshot never
replaces a newer claude observation with an older file source — the
background refresh rereads the statusline bridge, which
lags the usage endpoint a click just stored. Claude windows
carry their true observation time (`observed_at`: the last statusline tick, or
the fetch time for an HTTP 200), so both the row and the CLI can show their
age the way codex
rollouts did. A window whose `resets_at` has passed is dropped from the row and
from `kanban limits` (its percentage describes a period that is over), unless
it is a tick-regenerating quota (`LimitWindow.rolling`, synthetic's windows
and yolo's rolling day):
there the reset time is the next capacity gain, not the end of the window, so
the percentage stays until the next poll refreshes it. A provider whose
windows have all rolled over reads `stale` rather than freezing
yesterday's number. The renderer only ever draws
`App::limits`, the snapshot the event loop last pulled from that cache, and
degrades with width: reset times drop first, then window labels and provider
names, then whole providers from the right.

**Click refresh**: every provider segment of the row is a hitbox
(`UiAction::RefreshLimits`); a click refreshes that provider on a background
thread (`refresh_provider_async`, guarded against overlapping runs) and merges
the result into the same caches, so the row updates on the next tick. A click
on claude force-polls `GET /api/oauth/usage` (skipping the 15-minute interval
and the current-bridge short-circuit the background refresh honors) and merges
the result with whatever the statusline bridge still holds, and running
`grok models` renews the short-lived
OIDC token in `~/.grok/auth.json` before the billing fetch — that fixes
"grok reads signed out after
~6h" without a periodic poller — while zai / synthetic / yolo re-fetch over HTTPS
(their keys are long-lived, so no renewal step is needed). The CLIs run in
the scratch cwd `<store>/limits-refresh-cwd` so stray session state never
lands in a project. A 429 from the
usage endpoint keeps the last good Claude windows (the row does not flip to
`n/a`) and doubles the claude usage-endpoint poll interval before the next
poll, capped at 64×; the backoff is claude's own and never delays the other
providers. A transient fetch failure likewise keeps the cached numbers rather
than flipping a provider to `n/a`; only a real state change (signed out,
credentials removed) replaces them.

## Executor-pool gate (`provider_for`, `has_headroom`)

`core/executors.rs` decides a launch before it happens (see **Executor
Pools** in `docs/orchestration.md`); the limits side exposes two pure helpers.

`provider_for(backend, model)` maps a launch pair onto one of the row's
providers: `claude` → `claude`, `codex` → `codex`; the catalog backends
(`opencode`/`omp`/`pi`) resolve by model-id prefix — `openai/*` → `codex`
(the OpenAI subscription backs those runs), `anthropic/*` → `claude`,
`zai*`/`glm*` → `zai`, `synthetic/*` → `synthetic`, `yolo*` → `yolo`,
`xai/*`/`grok*` → `grok`. Anything else returns `None` — a pair no
subscription covers, such as a purely local model.

`has_headroom` checks every **live** window of the resolved provider against
the `orchestration.executors.thresholds` floors: a `5h` label needs
`five_hour_percent` remaining, a `7d`/weekly label needs `week_percent`, any
other label uses `week_percent`. The boundary is inclusive (exactly 5%
passes). `None` (no provider), `not_configured`, `signed out`, `unavailable`,
or a provider with no live window all **pass** the gate: unknown is not
exhausted, and a board whose limits cannot be read must keep running rather
than silently stall every task.

The gate reads `limits::cached()` only — never a blocking fetch. Dispatch
runs on the TUI tick and in the daemon, where an HTTP call would freeze the
event loop; the daemon's tick calls `refresh_if_stale` so the numbers the
gate sees stay fresh without ever blocking on them.
