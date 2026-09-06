# Graph (DAG) orchestration — investigation

Research artifact for TASK-295. **Nothing here is implemented.** It is kept out
of the `AGENTS.md` reference map on purpose: that map indexes shipped behavior,
and adding a row would charge every agent session for a proposal.

Sources are listed at the end.

---

## 1. How it works

### 1.1 The idea

An agent workflow is expressed as a directed acyclic graph. **Nodes** are units
of work (an agent call, or a pure "combine" step that merges results without
calling a model). **Edges** are two things at once:

- an **ordering constraint** — B cannot start until A finished;
- a **context contract** — what A hands to B.

The second half is what people usually skip, and it is where most of the value
sits. An edge that only orders work is a queue. An edge that also decides *what
crosses it* is a context-engineering device.

The canonical loop is **Planner → Executor → Replanner**:

1. **Planner** turns the goal into a graph up front, before any work runs.
2. **Validation** — acyclicity, every referenced node exists, roots are
   reachable, dependencies are consistent. This is a cheap static check that
   catches a whole class of bad plans *before* spending a single agent token.
3. **Executor** walks the graph in waves: every node whose predecessors are all
   complete is eligible; eligible nodes run in parallel up to a concurrency
   cap. This is Kahn's algorithm with a semaphore.
4. **Replanner** patches the graph when reality disagrees with the plan — a
   node fails, or returns something that makes downstream nodes pointless.

Acyclicity is not aesthetic. It is the termination guarantee: a graph with no
cycles cannot loop forever, which is the dominant failure mode of free-form
agent loops.

### 1.2 Where the mainstream patterns sit on the graph

Anthropic's *Building effective agents* draws the line between **workflows**
(LLMs orchestrated through predefined code paths) and **agents** (the model
directs its own process). A DAG is squarely on the workflow side, and the named
patterns are all graph shapes:

| pattern | graph shape |
|---|---|
| prompt chaining | a path — `A → B → C`, with programmatic gates between |
| routing | one node with mutually exclusive out-edges |
| parallelization (sectioning) | fan-out to independent nodes, then a join |
| parallelization (voting) | fan-out of the *same* task, join by consensus |
| orchestrator–workers | the graph is generated at runtime, not fixed |
| evaluator–optimizer | a cycle — deliberately *not* a DAG, bounded by a round counter |

Their headline caution matters more than the patterns: start with the simplest
thing, add structure only when a simpler version demonstrably underperforms.
Agentic systems trade latency and cost for task performance.

### 1.3 The orchestrator–worker findings

Anthropic's multi-agent research system is the most concrete public data point.
A lead agent decomposes the query and spawns subagents that explore in
parallel, **each with its own context window**, returning a distilled summary
rather than their transcript. Numbers worth carrying around:

- agents use ~**4×** the tokens of a chat turn; multi-agent systems ~**15×**;
- token usage alone explains **~80%** of performance variance on browsing
  benchmarks — i.e. much of "multi-agent is smarter" is really "multi-agent
  bought more tokens";
- spawning 3–5 subagents in parallel instead of serially cut wall-clock time by
  **up to 90%** on complex queries;
- it beat single-agent Opus by **90.2%** on their internal research eval.

And the limits, stated by them, which apply directly to a coding board:

> poorly suited for tasks requiring all agents to share the same context, or
> involving many dependencies between agents. Most coding tasks lack
> sufficient parallelizable opportunities.

The complementary piece is *context engineering*: context rot (accuracy decays
as the window fills), a finite attention budget, and sub-agents as a **context
isolation** mechanism — a subagent burns 50k tokens exploring and returns
1–2k. That reframing is important: the graph is not primarily a parallelism
device, it is a **context-partitioning** device that happens to allow
parallelism.

### 1.4 Karpathy's angle

Different emphasis, same structure. His "agentic engineering" framing is that
the engineer's job moves to decomposing goals into structured DAGs of subtasks,
with specs, diff review, eval loops and guardrails. Two ideas transfer well:

- **The commit DAG is a knowledge graph** — "commits are nodes, parent links
  are edges". The experiment history is queryable structure, not a linear log
  to be squashed into one main branch. *"The agent forgets; the graph does
  not."* Memory and evaluation live **outside** any context window.
- **The four conditions for autonomy**: verifiable outputs (metrics, not
  opinions), reversible actions, short feedback cycles, bounded action space.
  His autoresearch run did ~700 experiments over two days unattended because
  all four held.
- **The march of nines**: each additional nine of reliability costs as much as
  every previous one combined. 90% is nowhere near enough for unattended
  operation, and this is the honest ceiling on any "let the graph run
  overnight" pitch.

### 1.5 Короткое резюме (RU)

DAG-оркестрация — это представление работы агентов в виде направленного
ациклического графа: **узлы** — это единицы работы (вызов агента или чистое
слияние результатов), **рёбра** — одновременно порядок выполнения («B не
стартует, пока не закончил A») и контракт передачи контекста («что именно A
передаёт в B»).

Схема работы: **планировщик** строит граф заранее → граф **валидируется**
(нет циклов, все ссылки существуют, корни достижимы) → **исполнитель** идёт
волнами: любой узел, у которого все предшественники завершены, запускается,
параллельно, но не больше лимита слотов → **репланировщик** правит граф, когда
реальность разошлась с планом. Ацикличность даёт гарантию завершения: граф без
циклов не может зациклиться — а это главный способ, которым свободные
агентные циклы сжигают лимиты.

Главное, что обычно понимают неправильно: граф нужен **не ради
параллельности**, а ради **изоляции контекста**. Каждый узел — свежая сессия,
которая получает не всю историю, а сжатую выжимку от предшественников (у
Anthropic — 1–2k токенов вместо 50k, потраченных внутри узла). Это лечит
«context rot» — падение точности по мере заполнения окна. Параллельность —
приятный побочный эффект.

Цифры Anthropic: агент тратит ~4× токенов относительно чата, мульти-агент
~15×; при этом сам объём токенов объясняет ~80% разброса качества. То есть
«мульти-агент умнее» во многом означает «мульти-агент купил больше токенов».
Параллельный запуск 3–5 субагентов сокращает время до 90%. Их же ограничение,
прямо относящееся к нам: мульти-агент **плохо** подходит там, где всем нужен
общий контекст и где между агентами много зависимостей, — «у большинства задач
по кодингу мало возможностей для распараллеливания».

Карпаты добавляет два тезиса: **коммит-DAG — это граф знаний** («агент
забывает, граф — нет»), память и оценка должны жить вне контекстного окна; и
**march of nines** — каждый следующий «девяток» надёжности стоит столько же,
сколько все предыдущие вместе. 90% надёжности недостаточно для работы без
присмотра, и это честный потолок любой идеи «пусть граф крутится ночью».

---

## 2. Implementation in kanban4ai

### 2.1 What already exists

kanban4ai is much closer to a DAG executor than it looks. The hard parts are
done:

| DAG executor needs | kanban4ai already has |
|---|---|
| a node with isolated context | a task + its own session and thread |
| a concurrency-capped executor | `core/scheduler.rs` — `Slots::blocking_cap`, caps total → backend → backend/model → role |
| a clock to walk the graph | `core/daemon.rs` `pump_project` + the TUI tick |
| per-node failure containment | crash-restart backoff (`restart_at`, `crash_restarts`) |
| node-level sandboxing | worktree isolation (`core/vcs.rs`, `docs/worktrees.md`) |
| a distilled node output | the harvested agent reply (`core/reply.rs`) + rule-based compaction (`core/compaction.rs`) |
| a quality gate | the reviewer phase and `kanban verdict` |
| a planner | the designer phase |

What is missing is only the **edge semantics**.

### 2.2 What the current edge actually is

`Task.chained_to: Option<String>` (`src/core/models.rs:314`) — **one** optional
parent. `trigger_chained_tasks` (`src/core/operations.rs:1691`) fires when the
target enters Review and launches every To Do task pointing at it.

So today's graph is a **forest**: out-degree many, in-degree ≤ 1. Three
concrete gaps:

1. **No AND-join.** A task cannot wait on two predecessors. This is the single
   structural blocker — it is what makes a forest a forest.
2. **No context on the edge.** `trigger_chained_tasks` calls `take_task` with a
   fresh session and nothing else; `src/agent/prompt.rs` has no notion of an
   upstream task. The child starts blind. The edge carries ordering only —
   exactly the half of the value that is easy to get.
3. **No cycle validation.** `src/cli/mod.rs:1422` rejects only self-chaining;
   `grep -rn "cycle\|acyclic\|topolog" src/` finds nothing. `A → B → A` is
   representable. It does not currently hot-loop (the trigger only launches
   **To Do** tasks, so the second hop finds the target in Review and skips),
   but it is a silent deadlock rather than a rejected input.

### 2.3 Three increments

**Increment 1 — `depends_on: Vec<String>` (the AND-join).**

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub depends_on: Vec<String>,
```

`skip_serializing_if` is required, not cosmetic: `tests/fixtures/` must keep
round-tripping byte-identically. `chained_to` stays as a read alias — on load,
an empty `depends_on` with a set `chained_to` reads as a single-element
dependency, so existing boards keep working untouched.

The trigger inverts from **push** to **pull**. Instead of "the finished task
launches its children", a readiness sweep asks each To Do task with a non-empty
`depends_on` whether *every* dependency has reached Review or Done. That
inversion is the whole point:

- it is correct for multiple parents (the push version fires on whichever
  parent finishes first — with one parent that bug is unobservable);
- it picks up dependencies satisfied by a **human move**, not just by an agent
  `done`;
- it is idempotent, so it can run on every daemon tick.

Placement: a `dispatch_ready_dependents()` step in `daemon.rs::pump_project`
between `due_restarts()` and `dispatch_queue()`, so a newly-ready node lands in
phase `queued` and the existing cap-checked dispatcher starts it in the same
tick. Ready nodes must go through the queue, never launch directly — otherwise
a wide fan-out ignores every concurrency cap at once.

Validation: reject an edge that closes a cycle, at write time, with a DFS in
`kanban chain` / a new `kanban depends`. Cheap, and it is the acyclicity
guarantee from §1.1.

**Increment 2 — put context on the edge.** The higher-value half, and
independent of increment 1.

When a dependent launches, `agent/prompt.rs` prepends an *Upstream results*
section assembled from each dependency's harvested final reply plus its
compacted context entries, truncated to a configurable budget
(`orchestration.graph.upstream_budget_chars` — thresholds are config-driven per
the project rule, never hardcoded). Both inputs already exist and are already
deterministic; no LLM summarization is introduced, so the compaction rule in
`AGENTS.md` holds.

This is Anthropic's "subagent returns a 1–2k distilled summary" mapped onto
board artifacts, and it is what turns a chain of blind tasks into an actual
pipeline.

**Increment 3 — fan-out and join.** A `kanban split` that creates N sibling
tasks depending on the current one plus a join task depending on all N. This is
the *sectioning* pattern, and with increments 1–2 it is mostly plumbing. Pair
it with the designer phase emitting a **task graph** instead of prose — that is
"the planner generates the execution graph up front" using a substrate that
already exists.

### 2.4 The real risk: worktrees

Parallel siblings each get a worktree cut from a live snapshot and merged back
on completion. Siblings that touch the same files will land in
`integration: conflict`. **A DAG does not remove that constraint** — it makes it
easier to hit, because fan-out is exactly what produces concurrent edits.

Fan-out is therefore only safe when siblings touch disjoint file sets, which in
practice means the planner must assign file ownership per node, and that is a
judgment the planner will sometimes get wrong. Two mitigations, both using
existing knobs: seed a sibling's worktree from the parent's branch rather than
the project snapshot (`orchestration.isolation.seed`), and make the join node's
job be integration rather than more feature work.

This is the concrete form of Anthropic's warning about "many dependencies
between agents". Take it seriously: **depth (unattended sequencing) is the safe
win here; width (parallel fan-out on one codebase) is the risky one.**

---

## 3. Benefits, honestly, on a cheap subscription

The cost model matters. kanban4ai runs against **subscription** backends, not
metered API billing (`core/limits.rs` tracks exactly those windows). So "15×
tokens" is not 15× the invoice — it is 15× the consumption of a *fixed* quota
window. The binding constraint is the rate-limit window, not dollars, and that
changes every conclusion below.

**Human attention — the largest and most certain win.** To Do is manual-start
only by design, so today a 6-step feature costs 6 human touchpoints just to
*start* each step, plus the latency of the human noticing. A validated graph
costs one approval at plan time and one review at the end. Done stays
human-only, so the safety rule is untouched. This benefit does not depend on
any parallelism and does not cost extra tokens — it is pure sequencing, which
is why increment 1 is worth doing even alone.

**Token economics — the nuance that decides the whole question.** Spawning more
agents *spends* more. The saving comes from the other direction: **context
isolation**. One monolithic session doing 6 steps re-reads its own growing
history on every turn; 6 nodes each read a ~1–2k upstream summary and their own
task. Fewer tokens per unit of work *and* better accuracy, because you stay off
the context-rot curve. The rule of thumb: a graph that fans out to buy
parallelism costs more; a graph that partitions context to keep each node small
costs less. Build for the second and take the first only where siblings are
genuinely independent.

**Wall-clock time.** Anthropic's up-to-90% reduction assumed wide independent
fan-out — that is a research-workload number and will not transfer intact to a
coding board. What does transfer is **utilization**: quota windows currently go
unused overnight and while a human is not looking at the board. A graph plus
the daemon converts idle window into work. On a small subscription this is the
main lever — not "faster", but "the quota you already paid for stops expiring
unused".

**Accuracy.** Two forces, opposite signs. In favor: smaller per-node context
(less rot), a bounded action space per node, and failure that stays local — one
node crashing hits the existing per-task backoff while its siblings keep going.
Against: errors compound along a path, and a bad plan at the root poisons every
descendant, which a single long session would have had a chance to notice and
self-correct. The existing reviewer phase is the right gate, applied to
high-fan-in nodes rather than to everything (`use_reviewer` is already
per-task, so this needs no new mechanism).

**Cost to build.** Low relative to the payoff, because the scheduler, caps,
daemon, isolation, crash backoff, notifications and reviewer all already exist.
Increment 1 is one field, one readiness sweep, one daemon step and a cycle
check. Increment 2 is a prompt section over data that is already harvested.
Increment 3 is the only one that needs real design work, and it is the one with
the worktree risk — so it is correctly last.

**Recommendation.** Do increments 1 and 2. They are small, they use existing
machinery, and together they deliver the two benefits that actually hold up:
fewer human touchpoints and smaller per-node context. Treat increment 3 as a
separate decision gated on evidence that real tasks on this board decompose
into file-disjoint parallel work — Anthropic's own finding is that most coding
tasks do not, and the march of nines says an unattended graph will be reliable
enough to be pleasant long before it is reliable enough to be trusted.

---

## Sources

- [Anthropic — Building effective agents](https://www.anthropic.com/engineering/building-effective-agents)
- [Anthropic — How we built our multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)
- [Anthropic — Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- [The Karpathy Loop: AI Agents That Improve Themselves](https://www.dooza.ai/blog/karpathy-loop-graph-engineering)
- [The Karpathy Effect: From Vibe-Coding to Agentic Engineering](https://dataxad.com/blog/andrej-karpathy-agentic-engineering/)
- [Karpathy's March of Nines (VentureBeat)](https://venturebeat.com/technology/karpathys-march-of-nines-shows-why-90-ai-reliability-isnt-even-close-to)
- [DAGs: The Backbone of Modern Multi-Agent AI](https://santanub.medium.com/directed-acyclic-graphs-the-backbone-of-modern-multi-agent-ai-d9a0fe842780)
- [S-DAG: A Subject-Based Directed Acyclic Graph for Multi-Agent Heterogeneous Reasoning](https://arxiv.org/html/2511.06727v1)
- [LangDAG — DAG orchestration for LLM agent workflows](https://github.com/reedxiao/langdag)
